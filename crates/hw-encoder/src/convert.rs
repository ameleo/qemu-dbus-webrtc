// SPDX-License-Identifier: MPL-2.0

//! From the desktop's pixels in system memory to the picture the encoder reads: the pixels
//! are copied to GPU memory, and converted there to YUV by the GPU's video processing.

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::va::{self, Buffer, Config, Context, Display, Surface, sys};
use crate::{Frame, PixelFormat};

/// The bytes of a pixel, in every format taken.
const PIXEL: usize = 4;

/// A converter for frames of one size and format.
pub(crate) struct Converter {
    context: Context,
    /// Kept as long as its context.
    _config: Config,
    /// Where the frame is copied to: the size of the coded picture.
    rgb: Surface,
    format: PixelFormat,
    width: u32,
    height: u32,
    coded_width: u32,
    coded_height: u32,
}

impl Converter {
    /// A converter of frames of `width`x`height` pixels to coded pictures of
    /// `coded_width`x`coded_height`, which are at least as large.
    pub fn new(
        display: &Arc<Display>,
        format: PixelFormat,
        (width, height): (u32, u32),
        (coded_width, coded_height): (u32, u32),
    ) -> Result<Self> {
        if !display.has(sys::VAProfileNone, sys::VAEntrypointVideoProc) {
            return Err(Error::Unsupported("video processing".into()));
        }
        // The alpha byte, when the format has one, is not looked at.
        let fourcc = match format {
            PixelFormat::Bgrx => va::fourcc(b"BGRX"),
            PixelFormat::Rgbx => va::fourcc(b"RGBX"),
        };
        let config = Config::new(display, sys::VAProfileNone, sys::VAEntrypointVideoProc, &[])?;
        let context = Context::new(&config, coded_width, coded_height)?;
        let rgb = Surface::new(
            display,
            sys::VA_RT_FORMAT_RGB32,
            fourcc,
            coded_width,
            coded_height,
        )?;
        Ok(Self {
            context,
            _config: config,
            rgb,
            format,
            width,
            height,
            coded_width,
            coded_height,
        })
    }

    /// The names libva has for the layout of the RGB surface: by the bytes in memory
    /// (`BGRX`), which is how it is asked for, and by the 32-bit word from its most
    /// significant end (`XRGB`), which is how some drivers name it back; with alpha as well.
    fn layouts(&self) -> [u32; 4] {
        match self.format {
            PixelFormat::Bgrx => [b"BGRX", b"XRGB", b"BGRA", b"ARGB"],
            PixelFormat::Rgbx => [b"RGBX", b"XBGR", b"RGBA", b"ABGR"],
        }
        .map(va::fourcc)
    }

    /// Whether this converter takes frames like `frame`.
    pub fn takes(&self, frame: &Frame<'_>) -> bool {
        (frame.format, frame.width, frame.height) == (self.format, self.width, self.height)
    }

    /// Convert `frame` into `target`, a YUV surface of the coded picture's size. The
    /// conversion is BT.601, limited range, from full range RGB.
    pub fn convert(&self, frame: &Frame<'_>, target: &Surface) -> Result<()> {
        self.upload(frame)?;
        let mut pipeline = sys::VAProcPipelineParameterBuffer {
            surface: self.rgb.id(),
            surface_color_standard: sys::VAProcColorStandardSRGB,
            output_color_standard: sys::VAProcColorStandardBT601,
            ..Default::default()
        };
        pipeline.input_color_properties.color_range = sys::VA_SOURCE_RANGE_FULL as u8;
        pipeline.output_color_properties.color_range = sys::VA_SOURCE_RANGE_REDUCED as u8;
        // SAFETY: the structure of this buffer type; the pointers it holds are null.
        let buffer = unsafe {
            Buffer::parameter(
                &self.context,
                sys::VAProcPipelineParameterBufferType,
                &pipeline,
            )
        }?;
        self.context.render(target, &[buffer])
    }

    /// Copy the frame to the RGB surface. Beyond the frame, up to the coded picture's size,
    /// the last column and the last row are repeated: what costs the encoder the least, and
    /// what shows if a decoder shows more than the frame (an odd size in 4:2:0).
    fn upload(&self, frame: &Frame<'_>) -> Result<()> {
        let width = self.width as usize * PIXEL;
        let rows = self.height as usize;
        if frame.stride < width {
            return Err(Error::Frame(format!(
                "rows of {} bytes for {} pixels",
                frame.stride, self.width
            )));
        }
        let needed = frame.stride * (rows - 1) + width;
        if frame.pixels.len() < needed {
            return Err(Error::Frame(format!(
                "{} bytes of pixels, {needed} needed",
                frame.pixels.len()
            )));
        }
        let coded_width = self.coded_width as usize * PIXEL;
        let coded_rows = self.coded_height as usize;
        self.rgb.write(|mapped| {
            if !self.layouts().contains(&mapped.fourcc) {
                return Err(Error::Unsupported(format!(
                    "the GPU keeps RGB surfaces as {}, not as asked",
                    String::from_utf8_lossy(&mapped.fourcc.to_le_bytes())
                )));
            }
            let last = mapped
                .offset
                .checked_add(mapped.pitch * (coded_rows - 1) + coded_width);
            if mapped.pitch < coded_width || last.is_none_or(|last| last > mapped.data.len()) {
                return Err(Error::Unsupported(
                    "the GPU's RGB surface is smaller than asked".into(),
                ));
            }
            for row in 0..coded_rows {
                let source = &frame.pixels[frame.stride * row.min(rows - 1)..][..width];
                let target = &mut mapped.data[mapped.offset + mapped.pitch * row..][..coded_width];
                let (pixels, padding) = target.split_at_mut(width);
                pixels.copy_from_slice(source);
                for pixel in padding.as_chunks_mut::<PIXEL>().0 {
                    pixel.copy_from_slice(&source[width - PIXEL..]);
                }
            }
            Ok(())
        })?
    }
}
