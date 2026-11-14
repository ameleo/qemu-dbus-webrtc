// SPDX-License-Identifier: MPL-2.0

//! Encoding straight on the GPU's own API, with no GStreamer pipeline: the `qdw-hw-encoder`
//! library, on a thread of its own.
//!
//! The thread encodes the framebuffer as it is when it gets to it: frames asked for while it
//! is busy make one, the latest, as the pipeline's queues do.
//!
//! A DMA-BUF framebuffer is imported by the GPU, which reads it where it is. A framebuffer in
//! memory shared with QEMU is read by the CPU and copied to the GPU.

use std::ffi::c_void;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer_video as gst_video;
use qdw_hw_encoder as hw;
use tracing::{info, warn};

use crate::encode::{Codec, Counters, Settings, Sink};

/// The most frames a second the stream is said to carry, and what the rate control spreads
/// the bitrate over: what the VA encoders of GStreamer assume as well.
const FRAMERATE: u32 = 30;

/// A framebuffer in memory shared with QEMU, mapped in this process for as long as it is the
/// console's.
pub(crate) struct Mapping {
    pointer: *mut c_void,
    length: usize,
}

// SAFETY: the mapping is only read, and unmapped once, when dropped.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Map the `length` bytes of `fd` to read them. The mapping keeps the memory: `fd` may
    /// close.
    pub(crate) fn new(fd: OwnedFd, length: usize) -> Result<Self> {
        // SAFETY: a new shared mapping of a file we own, at an address the kernel picks.
        let pointer = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                length,
                rustix::mm::ProtFlags::READ,
                rustix::mm::MapFlags::SHARED,
                &fd,
                0,
            )
        }
        .context("mapping the framebuffer")?;
        Ok(Self { pointer, length })
    }

    /// The framebuffer's bytes. The guest draws in them while they are read, as with any
    /// shared framebuffer: the bytes are only copied, and a frame caught half drawn is
    /// followed by another.
    fn bytes(&self) -> &[u8] {
        // SAFETY: `length` bytes mapped until this is dropped, which the slice cannot outlive.
        unsafe { std::slice::from_raw_parts(self.pointer.cast::<u8>(), self.length) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: mapped in `new`, and no slice of it outlives it.
        let _ = unsafe { rustix::mm::munmap(self.pointer, self.length) };
    }
}

/// Where a framebuffer is.
pub(crate) enum Source {
    /// In memory shared with QEMU.
    Mapped(Mapping),
    /// In a DMA-BUF of one linear plane, of `size` bytes, in the DRM format `fourcc`.
    DmaBuf { fd: OwnedFd, size: u32, fourcc: u32 },
}

/// A framebuffer to encode.
#[derive(Clone)]
pub(crate) struct Picture {
    pub source: Arc<Source>,
    pub format: gst_video::VideoFormat,
    pub width: u32,
    pub height: u32,
    pub offset: usize,
    pub stride: usize,
}

/// What the encoding thread is asked, until it takes it.
#[derive(Default)]
struct Asked {
    /// The framebuffer to encode now.
    picture: Option<Picture>,
    keyframe: bool,
    /// In bit/s.
    bitrate: Option<u32>,
    /// Another encoder to go on with.
    encoder: Option<hw::Encoder>,
    stop: bool,
}

struct Shared {
    asked: Mutex<Asked>,
    wake: Condvar,
}

/// The library's settings for ours.
fn settings(settings: &Settings) -> Result<hw::Settings> {
    if settings.codec != Codec::Hevc || settings.software {
        return Err(anyhow!("only HEVC on a GPU is encoded directly"));
    }
    let bitrate = settings.bitrate.saturating_mul(1000);
    Ok(hw::Settings {
        codec: hw::Codec::Hevc,
        full_chroma: settings.full_chroma,
        ten_bits: settings.ten_bits,
        bitrate,
        max_bitrate: settings.max_bitrate.saturating_mul(1000).max(bitrate),
        framerate: FRAMERATE,
    })
}

/// The pixels of a framebuffer, for the library; the alpha byte is not looked at.
fn pixel_format(format: gst_video::VideoFormat) -> Option<hw::PixelFormat> {
    use gst_video::VideoFormat as F;
    match format {
        F::Bgrx | F::Bgra => Some(hw::PixelFormat::Bgrx),
        F::Rgbx | F::Rgba => Some(hw::PixelFormat::Rgbx),
        _ => None,
    }
}

/// An encoder on the GPU's own API, and its thread.
pub(crate) struct Direct {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Direct {
    /// Open the GPU's encoder for `settings` and start its thread; an error when the GPU, or
    /// the library, does not encode them.
    pub(crate) fn new(settings_: &Settings, sink: Sink, counters: Counters) -> Result<Self> {
        let encoder = hw::Encoder::open(None, settings(settings_)?)?;
        info!(
            driver = %encoder.driver(),
            full_chroma = settings_.full_chroma,
            ten_bits = settings_.ten_bits,
            "HEVC encoded on the GPU's own API"
        );
        let shared = Arc::new(Shared {
            asked: Mutex::new(Asked::default()),
            wake: Condvar::new(),
        });
        let thread = std::thread::Builder::new()
            .name("video-encoder".into())
            .spawn({
                let shared = shared.clone();
                move || run(encoder, &shared, &sink, &counters)
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    fn ask(&self, ask: impl FnOnce(&mut Asked)) {
        ask(&mut self.shared.asked.lock().unwrap());
        self.shared.wake.notify_one();
    }

    /// Encode `picture` as it is now.
    pub(crate) fn frame(&self, picture: Picture) {
        self.ask(|asked| asked.picture = Some(picture));
    }

    /// Make the next frame a keyframe.
    pub(crate) fn keyframe(&self) {
        self.ask(|asked| asked.keyframe = true);
    }

    /// Change the bitrate, in kbit/s, from the next frame on.
    pub(crate) fn set_bitrate(&self, bitrate: u32) {
        self.ask(|asked| asked.bitrate = Some(bitrate.saturating_mul(1000)));
    }

    /// Encode with other settings from the next frame on, which is a keyframe.
    pub(crate) fn restart(&self, settings_: &Settings) -> Result<()> {
        let encoder = hw::Encoder::open(None, settings(settings_)?)?;
        self.ask(|asked| {
            asked.encoder = Some(encoder);
            asked.bitrate = None;
        });
        Ok(())
    }

    /// Stop the thread: the frame it is encoding goes through before this returns.
    pub(crate) fn stop(&mut self) {
        self.ask(|asked| asked.stop = true);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Direct {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Encode `picture` with `encoder`; a DMA-BUF is imported once, and kept in `imported` with
/// the framebuffer it is.
fn encode(
    encoder: &mut hw::Encoder,
    imported: &mut Option<(Arc<Source>, hw::Imported)>,
    picture: &Picture,
    keyframe: bool,
) -> Result<hw::Packet> {
    match &*picture.source {
        Source::Mapped(mapping) => {
            let format = pixel_format(picture.format)
                .ok_or_else(|| anyhow!("framebuffer format {:?}", picture.format))?;
            let bytes = mapping.bytes();
            Ok(encoder.encode(
                &hw::Frame {
                    pixels: &bytes[picture.offset.min(bytes.len())..],
                    format,
                    width: picture.width,
                    height: picture.height,
                    stride: picture.stride,
                },
                keyframe,
            )?)
        }
        Source::DmaBuf { fd, size, fourcc } => {
            if !imported
                .as_ref()
                .is_some_and(|(source, _)| Arc::ptr_eq(source, &picture.source))
            {
                // The previous framebuffer's import goes first.
                *imported = None;
                let frame = encoder.import(&hw::DmaBuf {
                    fd: fd.as_fd(),
                    size: *size,
                    fourcc: *fourcc,
                    modifier: 0,
                    offset: u32::try_from(picture.offset)?,
                    stride: u32::try_from(picture.stride)?,
                    width: picture.width,
                    height: picture.height,
                })?;
                *imported = Some((picture.source.clone(), frame));
            }
            let (_, frame) = imported.as_ref().expect("imported just above");
            Ok(encoder.encode_imported(frame, keyframe)?)
        }
    }
}

/// The encoding thread: encodes what is asked until told to stop.
fn run(mut encoder: hw::Encoder, shared: &Shared, sink: &Sink, counters: &Counters) {
    // The framebuffer the encoder imported, which is the encoder's own.
    let mut imported = None;
    loop {
        let (picture, keyframe) = {
            let mut asked = shared.asked.lock().unwrap();
            while asked.picture.is_none() && !asked.stop {
                asked = shared.wake.wait(asked).unwrap();
            }
            if asked.stop {
                return;
            }
            if let Some(other) = asked.encoder.take() {
                imported = None;
                encoder = other;
            }
            if let Some(bitrate) = asked.bitrate.take() {
                encoder.set_bitrate(bitrate);
            }
            let Some(picture) = asked.picture.take() else {
                continue;
            };
            (picture, std::mem::take(&mut asked.keyframe))
        };
        let encoded = encode(&mut encoder, &mut imported, &picture, keyframe);
        let packet = match encoded {
            Ok(packet) => packet,
            Err(error) => {
                // The library makes the next frame a keyframe.
                warn!(error = %format!("{error:#}"), "frame not encoded");
                continue;
            }
        };
        let size = packet.data.len() as u64;
        let keyframe = packet.keyframe;
        let mut buffer = gst::Buffer::from_slice(packet.data);
        if !keyframe {
            buffer
                .get_mut()
                .expect("a buffer just made")
                .set_flags(gst::BufferFlags::DELTA_UNIT);
        }
        counters.encoded.fetch_add(1, Ordering::Relaxed);
        counters.bytes.fetch_add(size, Ordering::Relaxed);
        counters.output.fetch_add(size, Ordering::Relaxed);
        (sink.lock().unwrap())(&buffer);
    }
}
