// SPDX-License-Identifier: MPL-2.0

//! Low-latency hardware video encoding of a desktop, straight on the GPU's own API.
//!
//! A desktop's frames go in as they are in memory (RGB), one call each, and each comes back
//! encoded before the call returns: no frame is held back, none refers to a later one. A
//! keyframe is made when asked, and only then; the bitrate changes with the next frame,
//! without a keyframe.
//!
//! ```no_run
//! # fn main() -> qdw_hw_encoder::Result<()> {
//! use qdw_hw_encoder::{Codec, Encoder, Frame, PixelFormat, Settings};
//!
//! let mut encoder = Encoder::open(None, Settings::new(Codec::Hevc, 10_000_000))?;
//! # let (pixels, width, height) = (vec![0; 1920 * 1080 * 4], 1920, 1080);
//! let frame = Frame {
//!     pixels: &pixels,
//!     format: PixelFormat::Bgrx,
//!     width,
//!     height,
//!     stride: width as usize * 4,
//! };
//! let packet = encoder.encode(&frame, false)?;
//! # Ok(())
//! # }
//! ```
//!
//! Today, on VA-API (Intel, AMD): AV1 in 4:2:0, and HEVC in 4:2:0 and 4:4:4, 8 and 10 bits each.

mod av1;
mod av1_encoder;
mod bits;
mod common;
mod convert;
mod error;
pub mod h265;
mod hevc;
mod va;

use std::os::fd::BorrowedFd;
use std::path::Path;
use std::sync::Arc;

pub use error::{Error, Result};

use av1_encoder::Av1;
use convert::Converter;
use hevc::Hevc;
use va::Display;

/// A video codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// AV1, profile 0 (4:2:0). Royalty free.
    Av1,
    /// H.265. Subject to patent royalties.
    Hevc,
}

/// How the pixels of a frame are laid out in memory: four bytes each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Blue, green, red, then a byte that is not looked at (alpha or padding): DRM's
    /// `XRGB8888` and `ARGB8888`.
    Bgrx,
    /// Red, green, blue, then a byte that is not looked at: DRM's `XBGR8888` and `ABGR8888`.
    Rgbx,
}

/// A frame to encode: the desktop as it is now.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    /// The rows, from the top: `stride` bytes each, of which the first `4 * width` are
    /// pixels (the last row may stop after its pixels).
    pub pixels: &'a [u8],
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
}

/// A frame in a DMA-BUF of one plane: GPU memory, or memory a DMA-BUF exports (a guest's
/// framebuffer, with udmabuf). The GPU reads it where it is.
#[derive(Debug, Clone, Copy)]
pub struct DmaBuf<'a> {
    pub fd: BorrowedFd<'a>,
    /// The size of the whole buffer, in bytes.
    pub size: u32,
    /// The DRM format: `XB24`, `AB24`, `XR24` or `AR24` (`DRM_FORMAT_XBGR8888`...).
    pub fourcc: u32,
    /// The DRM format modifier: 0 for linear.
    pub modifier: u64,
    /// Where the first row is, and the bytes of a row.
    pub offset: u32,
    pub stride: u32,
    pub width: u32,
    pub height: u32,
}

/// A DMA-BUF imported by an encoder, to encode as often as its contents change.
pub struct Imported {
    surface: va::Surface,
    format: PixelFormat,
    width: u32,
    height: u32,
}

/// The pixel format and libva's format of a DRM format (drm_fourcc.h names the bytes of a
/// 32-bit little-endian word from the most significant: `XBGR8888` is R, G, B, X in memory).
fn drm_format(fourcc: u32) -> Option<(PixelFormat, u32)> {
    match &fourcc.to_le_bytes() {
        b"XB24" => Some((PixelFormat::Rgbx, va::fourcc(b"RGBX"))),
        b"AB24" => Some((PixelFormat::Rgbx, va::fourcc(b"RGBA"))),
        b"XR24" => Some((PixelFormat::Bgrx, va::fourcc(b"BGRX"))),
        b"AR24" => Some((PixelFormat::Bgrx, va::fourcc(b"BGRA"))),
        _ => None,
    }
}

/// What to encode to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub codec: Codec,
    /// 4:4:4 instead of 4:2:0: the colours at full resolution, for sharp coloured text.
    pub full_chroma: bool,
    /// 10 bits per component instead of 8.
    pub ten_bits: bool,
    /// The bitrate to start at, in bit/s: see [`Encoder::set_bitrate`].
    pub bitrate: u32,
    /// The highest bitrate the stream will be set to, in bit/s, when higher than `bitrate`:
    /// the stream says, from its start, what a decoder must be able to take.
    pub max_bitrate: u32,
    /// The most frames encoded in a second: what the stream says a decoder must be able to
    /// take, and what the rate control spreads the bitrate over.
    pub framerate: u32,
}

impl Settings {
    /// 4:2:0, 8 bits, up to 60 frames a second.
    pub fn new(codec: Codec, bitrate: u32) -> Self {
        Self {
            codec,
            full_chroma: false,
            ten_bits: false,
            bitrate,
            max_bitrate: bitrate,
            framerate: 60,
        }
    }
}

/// An encoded frame.
#[derive(Debug, Clone)]
pub struct Packet {
    /// The frame in the codec's stream format: for AV1, a temporal unit of OBUs with their
    /// sizes (the low overhead format of the AV1 specification, 5.2), with the sequence
    /// header before a key frame; for HEVC, an access unit as a byte stream (H.265 annex B),
    /// with the parameter sets before a keyframe.
    pub data: Vec<u8>,
    /// A frame that needs no other to be decoded.
    pub keyframe: bool,
}

/// The encoder of a codec, for frames of one size.
enum CodecEncoder {
    Av1(Av1),
    Hevc(Hevc),
}

impl CodecEncoder {
    fn new(display: &Arc<Display>, settings: &Settings, width: u32, height: u32) -> Result<Self> {
        Ok(match settings.codec {
            Codec::Av1 => Self::Av1(Av1::new(display, settings, width, height)?),
            Codec::Hevc => Self::Hevc(Hevc::new(display, settings, width, height)?),
        })
    }

    fn coded_size(&self) -> (u32, u32) {
        match self {
            Self::Av1(codec) => codec.coded_size(),
            Self::Hevc(codec) => codec.coded_size(),
        }
    }

    fn input(&self) -> &va::Surface {
        match self {
            Self::Av1(codec) => codec.input(),
            Self::Hevc(codec) => codec.input(),
        }
    }

    fn set_bitrate(&mut self, bitrate: u32) {
        match self {
            Self::Av1(codec) => codec.set_bitrate(bitrate),
            Self::Hevc(codec) => codec.set_bitrate(bitrate),
        }
    }

    fn encode(&mut self, keyframe: bool) -> Result<Packet> {
        match self {
            Self::Av1(codec) => codec.encode(keyframe),
            Self::Hevc(codec) => codec.encode(keyframe),
        }
    }
}

/// What encodes frames of one size and format.
struct Session {
    converter: Converter,
    codec: CodecEncoder,
}

/// An encoder of one video stream on a GPU.
pub struct Encoder {
    display: Arc<Display>,
    settings: Settings,
    session: Option<Session>,
}

impl Encoder {
    /// An encoder on the GPU of the DRM render node `device` (`/dev/dri/renderD128`), or on
    /// the first GPU of the host that encodes `settings`.
    pub fn open(device: Option<&Path>, settings: Settings) -> Result<Self> {
        if settings.bitrate == 0 || settings.framerate == 0 {
            return Err(Error::Unsupported("no bitrate or no frame rate".into()));
        }
        let encodes = |display: &Display| match settings.codec {
            Codec::Av1 => av1_encoder::supported(display, settings.full_chroma, settings.ten_bits),
            Codec::Hevc => hevc::supported(display, settings.full_chroma, settings.ten_bits),
        };
        let display = match device {
            Some(device) => {
                let display = Display::open(device)?;
                if !encodes(&display) {
                    return Err(Error::Unsupported(format!(
                        "{settings:?} on {}",
                        device.display()
                    )));
                }
                display
            }
            None => Display::nodes()
                .iter()
                .filter_map(|node| Display::open(node).ok())
                .find(|display| encodes(display))
                .ok_or_else(|| Error::Unsupported(format!("{settings:?} on any GPU")))?,
        };
        Ok(Self {
            display,
            settings,
            session: None,
        })
    }

    /// The GPU's driver, by its own words.
    pub fn driver(&self) -> String {
        self.display.vendor()
    }

    /// Encode a frame. It is a keyframe when `keyframe` asks for one, and when it has to be:
    /// the first frame, the first of another size or format, the one after a failure.
    pub fn encode(&mut self, frame: &Frame<'_>, keyframe: bool) -> Result<Packet> {
        let session = self.session(frame.format, frame.width, frame.height)?;
        session.converter.convert(frame, session.codec.input())?;
        session.codec.encode(keyframe)
    }

    /// Import a DMA-BUF, to encode it with [`Encoder::encode_imported`].
    pub fn import(&self, buffer: &DmaBuf<'_>) -> Result<Imported> {
        let (format, fourcc) = drm_format(buffer.fourcc).ok_or_else(|| {
            Error::Frame(format!(
                "DRM format {}",
                String::from_utf8_lossy(&buffer.fourcc.to_le_bytes())
            ))
        })?;
        if buffer.width == 0 || buffer.height == 0 {
            return Err(Error::Frame("no pixels".into()));
        }
        let surface = va::Surface::import(
            &self.display,
            buffer.fd,
            buffer.size,
            buffer.fourcc,
            fourcc,
            buffer.modifier,
            (buffer.width, buffer.height),
            buffer.offset,
            buffer.stride,
        )?;
        Ok(Imported {
            surface,
            format,
            width: buffer.width,
            height: buffer.height,
        })
    }

    /// Encode an imported DMA-BUF as it is now, as [`Encoder::encode`] does a frame.
    pub fn encode_imported(&mut self, frame: &Imported, keyframe: bool) -> Result<Packet> {
        let session = self.session(frame.format, frame.width, frame.height)?;
        session
            .converter
            .convert_surface(&frame.surface, session.codec.input())?;
        session.codec.encode(keyframe)
    }

    /// What encodes frames of this format and size: the current session, or a new one.
    fn session(&mut self, format: PixelFormat, width: u32, height: u32) -> Result<&mut Session> {
        if width == 0 || height == 0 {
            return Err(Error::Frame("no pixels".into()));
        }
        if self
            .session
            .as_ref()
            .is_none_or(|session| !session.converter.takes(format, width, height))
        {
            // The old session goes first: its memory is the GPU's.
            self.session = None;
            let codec = CodecEncoder::new(&self.display, &self.settings, width, height)?;
            let converter =
                Converter::new(&self.display, format, (width, height), codec.coded_size())?;
            self.session = Some(Session { converter, codec });
        }
        Ok(self.session.as_mut().expect("a session was just made"))
    }

    /// Change the bitrate, in bit/s, from the next frame on, without a keyframe. It should
    /// stay within the `max_bitrate` of the settings.
    pub fn set_bitrate(&mut self, bitrate: u32) {
        self.settings.bitrate = bitrate.max(1);
        if let Some(session) = &mut self.session {
            session.codec.set_bitrate(self.settings.bitrate);
        }
    }
}
