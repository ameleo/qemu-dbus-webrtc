// SPDX-License-Identifier: MPL-2.0

//! The encoding of one console, on a thread of its own: the `qdw-hw-encoder` crate, on the
//! GPU, or on the CPU with libvpx.
//!
//! The thread encodes the framebuffer as it is when it gets to it: frames asked for while it
//! is busy make one, the latest. On the GPU, a DMA-BUF framebuffer is imported once and read
//! where it is; memory shared with QEMU is read by the CPU and copied to the GPU. On the CPU,
//! both are read where they are mapped.

use std::ffi::c_void;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow};
use qdw_hw_encoder as hw;
use tracing::{info, warn};

use crate::encode::{Codec, Counters, EncodedFrame, PixelFormat, Settings, Sink};

/// The most frames a second a console sends. The encoders measure how many really come, and
/// spread the bitrate over them.
pub(crate) const FRAMERATE: u32 = 60;

/// `DMA_BUF_IOCTL_SYNC` (linux/dma-buf.h): brackets the CPU's reads of a DMA-BUF.
const DMA_BUF_SYNC_READ: u64 = 1;
const DMA_BUF_SYNC_START: u64 = 0;
const DMA_BUF_SYNC_END: u64 = 1 << 2;

/// A framebuffer mapped in this process, to be read by the CPU.
pub(crate) struct Mapping {
    pointer: *mut c_void,
    length: usize,
    /// A DMA-BUF's own descriptor, whose reads are bracketed for its exporter.
    dma_buf: Option<OwnedFd>,
}

// SAFETY: the mapping is only read, and unmapped once, when dropped.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Map the `length` bytes of `fd` to read them; a DMA-BUF when `dma_buf`. The mapping
    /// keeps the memory: `fd` may close.
    pub(crate) fn new(fd: &OwnedFd, length: usize, dma_buf: bool) -> Result<Self> {
        // SAFETY: a new shared mapping of an open file, at an address the kernel picks.
        let pointer = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                length,
                rustix::mm::ProtFlags::READ,
                rustix::mm::MapFlags::SHARED,
                fd,
                0,
            )
        }
        .context("mapping the framebuffer")?;
        let dma_buf = if dma_buf {
            Some(fd.as_fd().try_clone_to_owned()?)
        } else {
            None
        };
        Ok(Self {
            pointer,
            length,
            dma_buf,
        })
    }

    fn sync(&self, flags: u64) {
        let Some(fd) = &self.dma_buf else {
            return;
        };
        type Sync = rustix::ioctl::Setter<{ rustix::ioctl::opcode::write::<u64>(b'b', 0) }, u64>;
        // SAFETY: DMA_BUF_IOCTL_SYNC takes a `struct dma_buf_sync`, one 64-bit word of flags.
        if let Err(error) = unsafe { rustix::ioctl::ioctl(fd, Sync::new(flags)) } {
            warn!(%error, "DMA-BUF not synchronized");
        }
    }

    /// Give `read` the framebuffer's bytes. The guest draws in them while they are read, as
    /// with any shared framebuffer: they are only copied, and a frame caught half drawn is
    /// followed by another.
    fn read<R>(&self, read: impl FnOnce(&[u8]) -> R) -> R {
        self.sync(DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ);
        // SAFETY: `length` bytes mapped until this is dropped; the slice does not outlive the
        // call.
        let bytes = unsafe { std::slice::from_raw_parts(self.pointer.cast::<u8>(), self.length) };
        let result = read(bytes);
        self.sync(DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ);
        result
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: mapped in `new`, and no slice of it outlives `read`.
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
    /// When the frame was asked for: the guest had drawn.
    pub asked: std::time::Instant,
    pub source: Arc<Source>,
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub offset: usize,
    pub stride: usize,
}

/// The frame of `picture` in its framebuffer's `bytes`.
fn frame<'a>(picture: &Picture, bytes: &'a [u8]) -> hw::Frame<'a> {
    hw::Frame {
        pixels: &bytes[picture.offset.min(bytes.len())..],
        format: picture.format,
        width: picture.width,
        height: picture.height,
        stride: picture.stride,
    }
}

/// How a GPU reads a DMA-BUF framebuffer: imported, where it is; or, when the GPU cannot
/// import it, mapped and read by the CPU, slowly but read.
enum Read {
    Imported(hw::Imported),
    Mapped(Mapping),
}

/// What encodes, and what it made of the current framebuffer: a GPU's import of the
/// DMA-BUF, or the CPU's mapping of it.
enum Engine {
    Gpu {
        encoder: Box<hw::Encoder>,
        imported: Option<(Arc<Source>, Read)>,
    },
    Cpu {
        encoder: Box<hw::CpuEncoder>,
        mapped: Option<(Arc<Source>, Mapping)>,
    },
}

impl Engine {
    /// Open the encoder of `settings`: the first GPU that encodes them, or the CPU.
    fn open(settings: &Settings) -> Result<Self> {
        let bitrate = settings.bitrate.saturating_mul(1000);
        let library = hw::Settings {
            codec: settings.codec.hw(),
            full_chroma: settings.full_chroma,
            ten_bits: settings.ten_bits,
            bitrate,
            max_bitrate: settings.max_bitrate.saturating_mul(1000).max(bitrate),
            framerate: FRAMERATE,
        };
        if settings.software {
            if settings.codec != Codec::Vp9 {
                return Err(anyhow!("encoding on the CPU is VP9 only"));
            }
            let encoder = hw::CpuEncoder::new(library)?;
            info!(
                codec = ?settings.codec,
                full_chroma = settings.full_chroma,
                "encoded on the CPU, with libvpx"
            );
            return Ok(Self::Cpu {
                encoder: Box::new(encoder),
                mapped: None,
            });
        }
        let encoder = hw::Encoder::open(None, library)?;
        info!(
            codec = ?settings.codec,
            driver = %encoder.driver(),
            full_chroma = settings.full_chroma,
            ten_bits = settings.ten_bits,
            "encoded on the GPU"
        );
        Ok(Self::Gpu {
            encoder: Box::new(encoder),
            imported: None,
        })
    }

    fn set_bitrate(&mut self, bitrate: u32) {
        match self {
            Self::Gpu { encoder, .. } => encoder.set_bitrate(bitrate),
            Self::Cpu { encoder, .. } => {
                if let Err(error) = encoder.set_bitrate(bitrate) {
                    warn!(%error, "bitrate not changed");
                }
            }
        }
    }

    /// Encode `picture`; a DMA-BUF is imported or mapped once, and kept with the framebuffer
    /// it is.
    fn encode(&mut self, picture: &Picture, keyframe: bool) -> Result<hw::Packet> {
        match (self, &*picture.source) {
            (Self::Gpu { encoder, .. }, Source::Mapped(mapping)) => {
                mapping.read(|bytes| Ok(encoder.encode(&frame(picture, bytes), keyframe)?))
            }
            (Self::Cpu { encoder, .. }, Source::Mapped(mapping)) => {
                mapping.read(|bytes| Ok(encoder.encode(&frame(picture, bytes), keyframe)?))
            }
            (Self::Gpu { encoder, imported }, Source::DmaBuf { fd, size, fourcc }) => {
                if !imported
                    .as_ref()
                    .is_some_and(|(source, _)| Arc::ptr_eq(source, &picture.source))
                {
                    // The previous framebuffer's import goes first.
                    *imported = None;
                    let import = encoder.import(&hw::DmaBuf {
                        fd: fd.as_fd(),
                        size: *size,
                        fourcc: *fourcc,
                        modifier: 0,
                        offset: u32::try_from(picture.offset)?,
                        stride: u32::try_from(picture.stride)?,
                        width: picture.width,
                        height: picture.height,
                    });
                    let read = match import {
                        Ok(frame) => Read::Imported(frame),
                        Err(error) => {
                            // Once for this framebuffer, not at every frame.
                            warn!(%error, "framebuffer not imported: read by the CPU");
                            Read::Mapped(Mapping::new(fd, *size as usize, true)?)
                        }
                    };
                    *imported = Some((picture.source.clone(), read));
                }
                match imported.as_ref().map(|(_, read)| read) {
                    Some(Read::Imported(frame)) => Ok(encoder.encode_imported(frame, keyframe)?),
                    Some(Read::Mapped(mapping)) => {
                        mapping.read(|bytes| Ok(encoder.encode(&frame(picture, bytes), keyframe)?))
                    }
                    None => unreachable!("read just above"),
                }
            }
            (Self::Cpu { encoder, mapped }, Source::DmaBuf { fd, size, .. }) => {
                if !mapped
                    .as_ref()
                    .is_some_and(|(source, _)| Arc::ptr_eq(source, &picture.source))
                {
                    *mapped = None;
                    let mapping = Mapping::new(fd, *size as usize, true)?;
                    *mapped = Some((picture.source.clone(), mapping));
                }
                let (_, mapping) = mapped.as_ref().expect("mapped just above");
                mapping.read(|bytes| Ok(encoder.encode(&frame(picture, bytes), keyframe)?))
            }
        }
    }
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
    engine: Option<Engine>,
    stop: bool,
}

struct Shared {
    asked: Mutex<Asked>,
    wake: Condvar,
}

/// An encoder, and its thread.
pub(crate) struct Direct {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Direct {
    /// Open the encoder of `settings` and start its thread; an error when no GPU, nor the
    /// CPU when asked, encodes them.
    pub(crate) fn new(settings: &Settings, sink: Sink, counters: Counters) -> Result<Self> {
        let engine = Engine::open(settings)?;
        let shared = Arc::new(Shared {
            asked: Mutex::new(Asked::default()),
            wake: Condvar::new(),
        });
        let thread = std::thread::Builder::new()
            .name("video-encoder".into())
            .spawn({
                let shared = shared.clone();
                move || run(engine, &shared, &sink, &counters)
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
    pub(crate) fn restart(&self, settings: &Settings) -> Result<()> {
        let engine = Engine::open(settings)?;
        self.ask(|asked| {
            asked.engine = Some(engine);
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

/// The encoding thread: encodes what is asked until told to stop.
fn run(mut engine: Engine, shared: &Shared, sink: &Sink, counters: &Counters) {
    // The last error said, which is not said again at every frame.
    let mut last_error = None;
    loop {
        let (picture, keyframe) = {
            let mut asked = shared.asked.lock().unwrap();
            while asked.picture.is_none() && !asked.stop {
                asked = shared.wake.wait(asked).unwrap();
            }
            if asked.stop {
                return;
            }
            if let Some(other) = asked.engine.take() {
                engine = other;
            }
            if let Some(bitrate) = asked.bitrate.take() {
                engine.set_bitrate(bitrate);
            }
            let Some(picture) = asked.picture.take() else {
                continue;
            };
            (picture, std::mem::take(&mut asked.keyframe))
        };
        let packet = match engine.encode(&picture, keyframe) {
            Ok(packet) => packet,
            Err(error) => {
                let error = format!("{error:#}");
                if last_error.as_ref() != Some(&error) {
                    warn!(%error, "frame not encoded");
                    last_error = Some(error);
                }
                // A keyframe asked for still is: a frame can fail before the codec (an
                // import, the conversion), which leaves the stream going on from the last
                // frame encoded.
                if keyframe {
                    shared.asked.lock().unwrap().keyframe = true;
                }
                continue;
            }
        };
        last_error = None;
        let size = packet.data.len() as u64;
        let keyframe = packet.keyframe;
        let frame = EncodedFrame {
            data: packet.data.into(),
            keyframe,
        };
        (sink.lock().unwrap())(&frame);
        // From the guest's drawing to the frame handed to the viewers' pipelines.
        let latency = picture.asked.elapsed().as_micros() as u64;
        counters.encoded.fetch_add(1, Ordering::Relaxed);
        counters.bytes.fetch_add(size, Ordering::Relaxed);
        counters.latency.fetch_add(latency, Ordering::Relaxed);
        counters.latency_max.fetch_max(latency, Ordering::Relaxed);
        counters.recent_frames.fetch_add(1, Ordering::Relaxed);
        counters
            .recent_latency
            .fetch_add(latency, Ordering::Relaxed);
        counters
            .recent_latency_max
            .fetch_max(latency, Ordering::Relaxed);
    }
}
