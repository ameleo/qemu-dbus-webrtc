// SPDX-License-Identifier: MPL-2.0

//! From the desktop's pixels to the picture the encoder reads, converted to YUV by the GPU's
//! video processing. Pixels in GPU memory, or any memory a DMA-BUF exports, are read where
//! they are; pixels in the process's memory are copied to GPU memory first.

use std::cell::OnceCell;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::va::{self, Buffer, Config, Context, Display, Surface, sys};
use crate::{Frame, PixelFormat};

/// The bytes of a pixel, in every format taken.
const PIXEL: usize = 4;

/// A converter for frames of one size and format.
pub(crate) struct Converter {
    display: Arc<Display>,
    context: Context,
    /// Kept as long as its context.
    _config: Config,
    /// Where a frame in the process's memory is copied to, the size of the coded picture:
    /// made with the first such frame.
    rgb: OnceCell<Surface>,
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
        let config = Config::new(display, sys::VAProfileNone, sys::VAEntrypointVideoProc, &[])?;
        let context = Context::new(&config, coded_width, coded_height)?;
        Ok(Self {
            display: display.clone(),
            context,
            _config: config,
            rgb: OnceCell::new(),
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

    /// Whether this converter takes frames of this format and size.
    pub fn takes(&self, format: PixelFormat, width: u32, height: u32) -> bool {
        (format, width, height) == (self.format, self.width, self.height)
    }

    /// Convert `frame`, in the process's memory, into `target`, a YUV surface of the coded
    /// picture's size.
    pub fn convert(&self, frame: &Frame<'_>, target: &Surface) -> Result<()> {
        let rgb = self.upload(frame)?;
        // The whole surface: the frame, and its edges repeated up to the coded size.
        self.process(rgb, None, target)
    }

    /// Convert `source`, an RGB surface of the frame's size, into `target`. Past the frame,
    /// up to the coded size, `target` keeps what it had: pixels no decoder shows, the same
    /// from one picture to the next, which cost the encoder next to nothing.
    pub fn convert_surface(&self, source: &Surface, target: &Surface) -> Result<()> {
        if !source.of(&self.display) {
            return Err(Error::Frame("a frame imported by another encoder".into()));
        }
        let frame = sys::VARectangle {
            x: 0,
            y: 0,
            width: self.width as u16,
            height: self.height as u16,
        };
        self.process(source, Some(&frame), target)
    }

    /// One pass of the video processing, from `source` to `target`, of the region `region` of
    /// both (the whole surfaces when `None`). The conversion is BT.601, limited range, from
    /// full range RGB.
    fn process(
        &self,
        source: &Surface,
        region: Option<&sys::VARectangle>,
        target: &Surface,
    ) -> Result<()> {
        let region = region.map_or(std::ptr::null(), std::ptr::from_ref);
        let mut pipeline = sys::VAProcPipelineParameterBuffer {
            surface: source.id(),
            surface_region: region,
            surface_color_standard: sys::VAProcColorStandardSRGB,
            output_region: region,
            output_color_standard: sys::VAProcColorStandardBT601,
            ..Default::default()
        };
        pipeline.input_color_properties.color_range = sys::VA_SOURCE_RANGE_FULL as u8;
        pipeline.output_color_properties.color_range = sys::VA_SOURCE_RANGE_REDUCED as u8;
        // SAFETY: the structure of this buffer type. The driver reads the region it points
        // to when the picture is rendered, below, while `region` is alive.
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
    fn upload(&self, frame: &Frame<'_>) -> Result<&Surface> {
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
        let rgb = match self.rgb.get() {
            Some(rgb) => rgb,
            None => {
                // The alpha byte, when the format has one, is not looked at.
                let surface = Surface::new(
                    &self.display,
                    sys::VA_RT_FORMAT_RGB32,
                    self.layouts()[0],
                    self.coded_width,
                    self.coded_height,
                )?;
                self.rgb.get_or_init(|| surface)
            }
        };
        let coded_width = self.coded_width as usize * PIXEL;
        let coded_rows = self.coded_height as usize;
        rgb.write(|mapped| {
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
        })??;
        Ok(rgb)
    }
}
