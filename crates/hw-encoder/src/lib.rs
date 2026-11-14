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
//! Today: HEVC on VA-API (Intel, AMD), in 4:2:0 and 4:4:4, 8 and 10 bits.

mod bits;
mod convert;
mod error;
pub mod h265;
mod hevc;
mod va;

use std::path::Path;
use std::sync::Arc;

pub use error::{Error, Result};

use convert::Converter;
use hevc::Hevc;
use va::Display;

/// A video codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
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
    /// The frame in the codec's stream format: for HEVC, an access unit as a byte stream
    /// (H.265 annex B), with the parameter sets before a keyframe.
    pub data: Vec<u8>,
    /// A frame that needs no other to be decoded.
    pub keyframe: bool,
}

/// What encodes frames of one size and format.
struct Session {
    converter: Converter,
    codec: Hevc,
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
        if frame.width == 0 || frame.height == 0 {
            return Err(Error::Frame("no pixels".into()));
        }
        if self
            .session
            .as_ref()
            .is_none_or(|session| !session.converter.takes(frame))
        {
            // The old session goes first: its memory is the GPU's.
            self.session = None;
            let codec = Hevc::new(&self.display, &self.settings, frame.width, frame.height)?;
            let converter = Converter::new(
                &self.display,
                frame.format,
                (frame.width, frame.height),
                codec.coded_size(),
            )?;
            self.session = Some(Session { converter, codec });
        }
        let session = self.session.as_mut().expect("a session was just made");
        session.converter.convert(frame, session.codec.input())?;
        session.codec.encode(keyframe)
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
