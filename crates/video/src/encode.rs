// SPDX-License-Identifier: MPL-2.0

//! Hardware video encoding of one console: the guest framebuffer in, a compressed stream out.
//!
//! The framebuffer arrives as a DMA-BUF that QEMU shares with us. With `blob=on`, it is guest
//! memory exported through udmabuf: linear RGB, which VA-API cannot import as a surface (the
//! VA driver takes RGB only tiled). So it is handed to GStreamer as ordinary mapped memory:
//! `vapostproc` uploads it and converts it to NV12 on the GPU's video engine, and the encoder
//! reads the result from GPU memory.
//!
//! ```text
//! appsrc ! queue ! vapostproc ! video/x-raw(memory:VAMemory) ! encoder ! parser ! appsink
//! ```

use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_allocators::{DmaBufAllocator, prelude::*};
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use tracing::warn;

/// The video codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Royalty free, the most efficient at low bitrates; 4:2:0 only on the hardware encoders.
    Av1,
    /// Royalty free, with full chroma (profile 1) on the hardware encoders of Intel GPUs.
    Vp9,
    /// Subject to patent royalties, see ARCHITECTURE.md.
    Hevc,
}

impl Codec {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "av1" => Some(Self::Av1),
            "vp9" => Some(Self::Vp9),
            "hevc" | "h265" => Some(Self::Hevc),
            _ => None,
        }
    }

    /// The usual file extension of the recorded stream.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Av1 | Self::Vp9 => "ivf",
            Self::Hevc => "h265",
        }
    }

    /// Whether the hardware encoder does 4:4:4.
    pub fn has_full_chroma(self) -> bool {
        matches!(self, Self::Vp9 | Self::Hevc)
    }

    /// The encoder element with its codec-specific properties, and the output caps that pick
    /// the profile.
    fn encoder(self, full_chroma: bool) -> Result<(&'static str, &'static str)> {
        Ok(match (self, full_chroma) {
            // Low-power encoders: the only kind Intel Arc GPUs have.
            //
            // No reference to future frames (no B-frames, hierarchical-level=1): by default AV1
            // and VP9 code groups of up to 32 frames ahead, and hold the frames back until the
            // group is complete, seconds at a low frame rate. This also means no hidden frames:
            // one frame per picture.
            //
            // Keyframes as rarely as the encoders allow: a client asks for one when it needs it.
            (Self::Av1, false) => (
                "vaav1lpenc hierarchical-level=1 key-int-max=1024",
                "video/x-av1",
            ),
            (Self::Av1, true) => bail!("no 4:4:4 AV1 on this encoder"),
            (Self::Vp9, false) => (
                "vavp9lpenc hierarchical-level=1 key-int-max=1024",
                "video/x-vp9,profile=(string)0",
            ),
            (Self::Vp9, true) => (
                "vavp9lpenc hierarchical-level=1 key-int-max=1024",
                "video/x-vp9,profile=(string)1",
            ),
            (Self::Hevc, false) => (
                "vah265lpenc b-frames=0 key-int-max=1024",
                "video/x-h265,profile=(string)main",
            ),
            (Self::Hevc, true) => (
                "vah265lpenc b-frames=0 key-int-max=1024",
                "video/x-h265,profile=(string)main-444",
            ),
        })
    }

    /// The parser, and the caps of the encoded stream as it leaves the encoder.
    fn parser(self) -> (&'static str, &'static str) {
        match self {
            // Whole temporal units, OBUs with their size: what RTP and IVF carry.
            Self::Av1 => (
                "av1parse",
                "video/x-av1,stream-format=obu-stream,alignment=tu,parsed=true",
            ),
            // One picture per buffer: the encoder makes no hidden frames, and if it did, a
            // superframe would keep the hidden frame with the one that shows it.
            Self::Vp9 => ("vp9parse", "video/x-vp9,alignment=super-frame,parsed=true"),
            Self::Hevc => (
                "h265parse",
                "video/x-h265,stream-format=byte-stream,alignment=au,parsed=true",
            ),
        }
    }

    /// The caps of the encoded stream, to feed it to another pipeline.
    pub fn stream_caps(self) -> &'static str {
        self.parser().1
    }

    /// The RTP payloader and the codec's RTP encoding name.
    pub fn rtp(self) -> (&'static str, &'static str) {
        match self {
            Self::Av1 => ("rtpav1pay", "AV1"),
            // The Rust payloader: the C one cannot parse the frames of the VA encoder.
            Self::Vp9 => ("rtpvp9pay2", "VP9"),
            Self::Hevc => (
                "rtph265pay config-interval=-1 aggregate-mode=zero-latency",
                "H265",
            ),
        }
    }

    /// What the RTP caps add for this codec, as the offer's fmtp line will say it: VP9 in 4:4:4
    /// is profile 1, which a client must decode to take the offer.
    pub fn rtp_extra(self, full_chroma: bool) -> &'static str {
        match (self, full_chroma) {
            (Self::Vp9, true) => ",profile-id=(string)1",
            _ => "",
        }
    }

    /// Whether a client that announces `codec` (a media type, with its fmtp parameters after a
    /// `;` when it has any) decodes this stream.
    pub fn decoded_by(self, full_chroma: bool, codec: &str) -> bool {
        let mut parts = codec.split(';').map(str::trim);
        if !parts
            .next()
            .is_some_and(|mime| mime.eq_ignore_ascii_case(self.mime_type()))
        {
            return false;
        }
        match (self, full_chroma) {
            // VP9 in 4:4:4 is profile 1, which the client must announce.
            (Self::Vp9, true) => parts.any(|p| p.eq_ignore_ascii_case("profile-id=1")),
            _ => true,
        }
    }

    /// The codec's media type in a browser (`RTCRtpReceiver.getCapabilities`).
    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Av1 => "video/AV1",
            Self::Vp9 => "video/VP9",
            Self::Hevc => "video/H265",
        }
    }
}

/// The rate control buffer, in kbit: half a second of the bitrate, which bounds the size of a
/// frame. The encoders default to two seconds, which lets a keyframe outgrow the buffer that
/// the VA encoders allocate for one frame (1.5 MB for AV1 at 3440x1440): the driver then writes
/// past it, and GStreamer, which does not check for that, reads past it and crashes. A smaller
/// buffer also keeps the bursts on the network short.
fn cpb_size(bitrate: u32) -> u32 {
    (bitrate / 2).max(1)
}

/// Encoder settings.
#[derive(Debug, Clone)]
pub struct Settings {
    pub codec: Codec,
    /// 4:4:4 instead of 4:2:0: sharp coloured text and thin lines, at a higher bitrate.
    pub full_chroma: bool,
    /// Target bitrate, in kbit/s.
    pub bitrate: u32,
}

/// The GStreamer raw format of a DRM fourcc (DRM names the bytes from the most significant
/// end of a little-endian word, GStreamer in memory order).
fn video_format(fourcc: u32) -> Option<gst_video::VideoFormat> {
    use gst_video::VideoFormat as F;
    match &fourcc.to_le_bytes() {
        b"XB24" => Some(F::Rgbx),
        b"AB24" => Some(F::Rgba),
        b"XR24" => Some(F::Bgrx),
        b"AR24" => Some(F::Bgra),
        _ => None,
    }
}

/// A guest framebuffer, ready to be pushed as often as it changes.
struct Framebuffer {
    caps: gst::Caps,
    memory: gst::Memory,
    format: gst_video::VideoFormat,
    width: u32,
    height: u32,
    offset: usize,
    stride: i32,
}

/// One console's encoder.
pub struct Encoder {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    framebuffer: Option<Framebuffer>,
    /// Frames out of the encoder.
    encoded: Arc<AtomicU64>,
    /// Bytes out of the encoder.
    bytes: Arc<AtomicU64>,
    /// When the last frame went into the encoder.
    last_frame: std::cell::Cell<std::time::Instant>,
}

impl Encoder {
    /// Build and start an encoder. Its output goes to `sink`, called once per encoded frame
    /// (an access unit in Annex B for HEVC, a VP9 frame, an AV1 temporal unit), on a GStreamer
    /// thread.
    pub fn new(
        settings: &Settings,
        sink: impl FnMut(&gst::Buffer) + Send + 'static,
    ) -> Result<Self> {
        let (encoder, profile) = settings.codec.encoder(settings.full_chroma)?;
        // BT.601, limited range: the VA encoders do not write the colour space into the stream,
        // and decoders then assume BT.601. Converting with BT.709 shifts the colours.
        let raw = if settings.full_chroma { "VUYA" } else { "NV12" };
        let description = format!(
            "appsrc name=src is-live=true format=time do-timestamp=true \
             ! queue max-size-buffers=3 max-size-bytes=0 max-size-time=0 \
             ! vapostproc \
             ! video/x-raw(memory:VAMemory),format={raw},colorimetry=bt601 \
             ! {encoder} name=encoder rate-control=cbr bitrate={bitrate} cpb-size={cpb} \
             ! {profile} \
             ! {parser} ! {stream_caps} \
             ! appsink name=sink sync=false",
            bitrate = settings.bitrate,
            cpb = cpb_size(settings.bitrate),
            parser = settings.codec.parser().0,
            stream_caps = settings.codec.stream_caps(),
        );
        let pipeline = gst::parse::launch(&description)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        let src = pipeline
            .by_name("src")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .ok_or_else(|| anyhow!("no appsrc"))?;
        let appsink = pipeline
            .by_name("sink")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .ok_or_else(|| anyhow!("no appsink"))?;

        let encoded = Arc::new(AtomicU64::new(0));
        let bytes = Arc::new(AtomicU64::new(0));
        let (counted, sized) = (encoded.clone(), bytes.clone());
        let sink = std::sync::Mutex::new(sink);
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |appsink| {
                    let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer_owned().ok_or(gst::FlowError::Error)?;
                    counted.fetch_add(1, Ordering::Relaxed);
                    sized.fetch_add(buffer.size() as u64, Ordering::Relaxed);
                    (sink.lock().unwrap())(&buffer);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        // Report pipeline errors; they end the stream, not the program.
        let bus = pipeline.bus().ok_or_else(|| anyhow!("no bus"))?;
        bus.set_sync_handler(|_, message| {
            if let gst::MessageView::Error(error) = message.view() {
                warn!(
                    source = ?error.src().map(|s| s.path_string()),
                    error = %error.error(),
                    debug = ?error.debug(),
                    "encoder pipeline error"
                );
            }
            gst::BusSyncReply::Drop
        });

        pipeline.set_state(gst::State::Playing)?;
        Ok(Self {
            pipeline,
            src,
            framebuffer: None,
            encoded,
            bytes,
            last_frame: std::cell::Cell::new(std::time::Instant::now()),
        })
    }

    /// A new framebuffer: `fd` is a single-plane DMA-BUF of `width`x`height` pixels in DRM
    /// format `fourcc`, linear.
    pub fn set_framebuffer(
        &mut self,
        fd: &OwnedFd,
        width: u32,
        height: u32,
        fourcc: u32,
        offset: u32,
        stride: u32,
    ) -> Result<()> {
        let format =
            video_format(fourcc).ok_or_else(|| anyhow!("unsupported fourcc {fourcc:#x}"))?;
        // Our own descriptor: GStreamer closes it with the memory.
        let fd = fd.as_fd().try_clone_to_owned()?;
        let size = rustix::fs::seek(&fd, rustix::fs::SeekFrom::End(0)).context("DMA-BUF size")?;
        let needed = u64::from(offset) + u64::from(stride) * u64::from(height);
        if size < needed {
            bail!("DMA-BUF of {size} bytes, {needed} needed");
        }
        // SAFETY: the descriptor is a DMA-BUF, and owned by the memory from now on.
        let memory = unsafe { DmaBufAllocator::new().alloc_dmabuf(fd, size as usize)? };
        let caps = gst_video::VideoInfo::builder(format, width, height)
            .fps(gst::Fraction::new(0, 1))
            .build()?
            .to_caps()?;
        self.framebuffer = Some(Framebuffer {
            caps,
            memory,
            format,
            width,
            height,
            offset: offset as usize,
            stride: stride as i32,
        });
        Ok(())
    }

    /// The guest has drawn: encode the framebuffer as it is now.
    pub fn frame(&self) -> Result<()> {
        let Some(fb) = &self.framebuffer else {
            return Ok(());
        };
        if self.src.caps().as_ref() != Some(&fb.caps) {
            self.src.set_caps(Some(&fb.caps));
        }
        let mut buffer = gst::Buffer::new();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.append_memory(fb.memory.clone());
            // The layout of the pixels in the DMA-BUF, which the caps alone do not give.
            gst_video::VideoMeta::add_full(
                buffer,
                gst_video::VideoFrameFlags::empty(),
                fb.format,
                fb.width,
                fb.height,
                &[fb.offset],
                &[fb.stride],
            )?;
        }
        self.src
            .push_buffer(buffer)
            .map_err(|e| anyhow!("pushing a frame: {e:?}"))?;
        self.last_frame.set(std::time::Instant::now());
        Ok(())
    }

    /// How long ago the last frame went into the encoder.
    pub fn since_last_frame(&self) -> std::time::Duration {
        self.last_frame.get().elapsed()
    }

    /// Make the next frame a keyframe, and encode the framebuffer now: a new viewer, or one
    /// that lost a frame, needs it even if the guest does not draw.
    pub fn keyframe(&self) -> Result<()> {
        self.src.send_event(
            gst_video::DownstreamForceKeyUnitEvent::builder()
                .all_headers(true)
                .build(),
        );
        self.frame()
    }

    /// Change the target bitrate, in kbit/s, and the rate control buffer with it. The VA
    /// encoders of GStreamer 1.26 reconfigure for it, which resets the GOP and costs a
    /// keyframe; patched (the `.daas` build), they take it with the next frame.
    pub fn set_bitrate(&self, bitrate: u32) {
        if let Some(encoder) = self.pipeline.by_name("encoder") {
            encoder.set_property("cpb-size", cpb_size(bitrate));
            encoder.set_property("bitrate", bitrate);
        }
    }

    /// The head is off: nothing to encode until the next framebuffer.
    pub fn clear_framebuffer(&mut self) {
        self.framebuffer = None;
    }

    /// Frames and bytes encoded since the last call.
    pub fn take_counters(&self) -> (u64, u64) {
        (
            self.encoded.swap(0, Ordering::Relaxed),
            self.bytes.swap(0, Ordering::Relaxed),
        )
    }

    /// Finish the stream: the last frames go through before this returns.
    pub fn finish(self) {
        let _ = self.src.end_of_stream();
        if let Some(bus) = self.pipeline.bus() {
            let _ =
                bus.timed_pop_filtered(gst::ClockTime::from_seconds(2), &[gst::MessageType::Eos]);
        }
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoded_by() {
        assert!(Codec::Av1.decoded_by(false, "video/AV1"));
        assert!(Codec::Vp9.decoded_by(false, "video/vp9;profile-id=0"));
        assert!(Codec::Vp9.decoded_by(true, "video/VP9;profile-id=1"));
        assert!(!Codec::Vp9.decoded_by(true, "video/VP9;profile-id=0"));
        assert!(!Codec::Vp9.decoded_by(true, "video/VP9"));
        assert!(!Codec::Av1.decoded_by(false, "video/VP9"));
    }
}
