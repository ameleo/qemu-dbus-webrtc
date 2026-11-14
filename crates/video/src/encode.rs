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
//!
//! AV1 and HEVC on a GPU leave GStreamer out: the `qdw-hw-encoder` crate imports the
//! framebuffer on the GPU and drives its encoder itself (see [`crate::direct`]). The stream
//! that comes out goes the same way as the pipeline's.

use std::os::fd::{AsFd, IntoRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_allocators::{DmaBufAllocator, FdAllocator, FdMemoryFlags, prelude::*};
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use tracing::warn;

use crate::direct::{Direct, Mapping, Picture, Source};

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

    /// Whether the encoder takes 10 bits per component, when the clients decode them: AV1 and
    /// HEVC (Main 10, Main 4:4:4 10). In 10 bits, the guest's 8-bit RGB loses no precision on its way to YUV, where 8
    /// bits round each pixel and band smooth gradients (measured on the A380: an 8-bit encode
    /// of a gradient stops at 43.8 dB in RGB whatever the bitrate, a 10-bit one reaches 56 dB
    /// with fewer bits; 3 % fewer bits on text). AV1 in 10 bits is still its Main profile,
    /// so SDP does not tell it apart: a client says it decodes it in its hello (see
    /// [`Codec::deep_by`]); HEVC likewise, for the server's offer leaves its profile out. VP9 in
    /// 10 bits is profile 2 or 3, which far fewer clients decode.
    pub fn has_ten_bits(self) -> bool {
        matches!(self, Self::Av1 | Self::Hevc)
    }

    /// The raw format the encoder takes.
    fn raw_format(self, full_chroma: bool, ten_bits: bool) -> &'static str {
        match (full_chroma, ten_bits && self.has_ten_bits()) {
            (true, true) => "Y410",
            (true, false) => "VUYA",
            (false, true) => "P010_10LE",
            (false, false) => "NV12",
        }
    }

    /// Whether a client that announces `codec`, as for [`Codec::decoded_by`], decodes this
    /// stream in 10 bits: `video/AV1;bit-depth=10`, `video/H265;bit-depth=10`. Not an SDP
    /// parameter: it only goes between
    /// this server and its clients.
    pub fn deep_by(self, codec: &str) -> bool {
        let mut parts = codec.split(';').map(str::trim);
        self.has_ten_bits()
            && parts
                .next()
                .is_some_and(|mime| mime.eq_ignore_ascii_case(self.mime_type()))
            && parts.any(|p| p.eq_ignore_ascii_case("bit-depth=10"))
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

    /// The fmtp parameters of the stream (RFC 7798 7.1), as caps fields, for HEVC: the offer
    /// must say what the stream is, for an absent parameter means a default (profile-id 1,
    /// level-id 93, interop-constraints B00000000000...) that ours are not. Taken from the
    /// bitstream the VA encoder writes (`general_profile_idc`, the 32
    /// `general_profile_compatibility_flag`s and the 48 bits from
    /// `general_progressive_source_flag` on, in the order of the bitstream, base16): measured,
    /// and checked against a real encoding by a test. The level is the highest the encoder may
    /// reach (6.2, `general_level_idc` 186, for up to 8192x4320 pixels), which every lower level
    /// complies with: the encoder picks it by the screen's size, which changes.
    pub fn rtp_fmtp(self, full_chroma: bool, ten_bits: bool) -> Option<String> {
        if self != Self::Hevc {
            return None;
        }
        // (profile-id, profile-compatibility-indicator, interop-constraints)
        let (profile, compatibility, constraints) = match (full_chroma, ten_bits) {
            // Main: compatible with Main and Main 10.
            (false, false) => (1, "60000000", "900000000000"),
            // Main 10.
            (false, true) => (2, "20000000", "900000000000"),
            // Format range extensions, Main 4:4:4, then Main 4:4:4 10 (max_8bit_constraint off).
            (true, false) => (4, "08000000", "9E0800000000"),
            (true, true) => (4, "08000000", "9C0800000000"),
        };
        Some(format!(
            "profile-id=(string){profile},tier-flag=(string)0,level-id=(string)186,\
             profile-compatibility-indicator=(string){compatibility},\
             interop-constraints=(string){constraints}"
        ))
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
            // VP9 in 4:4:4 is profile 1, HEVC's is a format range extensions profile, profile-id
            // 4 (RFC 7798 7.1): the client must announce it, no parameter meaning the codec's
            // default profile.
            (Self::Vp9, true) => parts.any(|p| p.eq_ignore_ascii_case("profile-id=1")),
            (Self::Hevc, true) => parts.any(|p| p.eq_ignore_ascii_case("profile-id=4")),
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
    /// 10 bits per component, for codecs that have it (see [`Codec::has_ten_bits`]).
    pub ten_bits: bool,
    /// Encode on the CPU (VP9 only) rather than on the GPU.
    pub software: bool,
    /// The highest bitrate the stream will be set to, in kbit/s: what an AV1 or HEVC stream
    /// says of itself, from its start (its level).
    pub max_bitrate: u32,
    /// Encode on the GPU's own API where this program does (AV1, HEVC), rather than through
    /// GStreamer's VA elements.
    pub direct: bool,
}

/// The software encoder: libvpx's VP9, for hosts without a GPU. Real time (deadline=1,
/// cpu-used=6: measured at 3440x1440, 69 frames a second on about 7 cores, 48.8 dB at 5 Mbit/s;
/// cpu-used=8 is faster but blurs the picture), no frame held back (lag-in-frames=0), keyframes
/// only when asked, constant bitrate with half a second of buffer, as on the GPU.
fn software_encoder(full_chroma: bool, bitrate: u32) -> (String, &'static str) {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    let (raw, profile) = if full_chroma {
        ("Y444", "video/x-vp9,profile=(string)1")
    } else {
        ("I420", "video/x-vp9,profile=(string)0")
    };
    (
        format!(
            "videoconvert ! video/x-raw,format={raw},colorimetry=bt601 \
             ! vp9enc name=encoder deadline=1 cpu-used=6 row-mt=true tile-columns=2 \
             threads={threads} lag-in-frames=0 end-usage=cbr keyframe-mode=disabled \
             buffer-size=500 buffer-initial-size=250 buffer-optimal-size=400 \
             target-bitrate={bps}",
            bps = u64::from(bitrate) * 1000,
        ),
        profile,
    )
}

/// Our own copy of `fd`, and its size, which must hold `height` rows of `stride` bytes from
/// `offset`.
fn own(
    fd: &OwnedFd,
    offset: u32,
    width: u32,
    stride: u32,
    height: u32,
) -> Result<(OwnedFd, usize)> {
    // Rows at least as long as their pixels, 4 bytes each in every format taken: shorter, the
    // last row would be read past what is checked below.
    if u64::from(stride) < u64::from(width) * 4 {
        bail!("rows of {stride} bytes for {width} pixels");
    }
    // Our own descriptor: GStreamer closes it with the memory.
    let fd = fd.as_fd().try_clone_to_owned()?;
    let size = rustix::fs::seek(&fd, rustix::fs::SeekFrom::End(0)).context("framebuffer size")?;
    let needed = u64::from(offset) + u64::from(stride) * u64::from(height);
    if size < needed {
        bail!("framebuffer of {size} bytes, {needed} needed");
    }
    Ok((fd, size as usize))
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

/// The encoder's input keeps the latest frame only, dropping older ones: without a bound, the
/// appsrc queues every frame pushed while the encoder is behind (the GPU busy), and the picture
/// lags more and more (measured: 35 frames, over a second, after 2 s at 30 frames a second into
/// an encoder taking 10). Every frame shows the framebuffer as it is when encoded, so the
/// latest loses nothing. `downstream` drops the oldest (measured too).
const LATEST: &str = "leaky-type=downstream max-buffers=1";
/// The queue after it, which gives the encoder its own thread, holds one frame too and drops
/// the same way: three frames waiting in it kept the picture half a second behind a slow
/// encoder, encoded one after the other for nothing (measured as above: 529 ms behind with
/// three, 128 ms with one, about the time the encoder takes for a frame).
const QUEUE: &str = "queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream";

/// Where the encoded frames go.
pub(crate) type Sink = Arc<std::sync::Mutex<Box<dyn FnMut(&gst::Buffer) + Send>>>;

/// What an encoder produced, across its pipelines.
#[derive(Default, Clone)]
pub(crate) struct Counters {
    /// Frames out of the encoder.
    pub(crate) encoded: Arc<AtomicU64>,
    /// Bytes out of the encoder.
    pub(crate) bytes: Arc<AtomicU64>,
    /// Bytes out of the encoder since it started.
    pub(crate) output: Arc<AtomicU64>,
}

/// What encodes: a GStreamer pipeline, or the GPU's own API. An encoder keeps the one it
/// started with.
enum Backend {
    Pipeline {
        pipeline: gst::Pipeline,
        src: gst_app::AppSrc,
    },
    Direct(Direct),
}

/// Start what encodes `settings`: the GPU's own API when asked and able, a pipeline
/// otherwise.
fn backend(settings: &Settings, sink: &Sink, counters: &Counters) -> Result<Backend> {
    if settings.direct && !settings.software && matches!(settings.codec, Codec::Av1 | Codec::Hevc) {
        match Direct::new(settings, sink.clone(), counters.clone()) {
            Ok(direct) => return Ok(Backend::Direct(direct)),
            Err(error) => warn!(
                error = %format!("{error:#}"),
                "no direct encoder: encoding through GStreamer"
            ),
        }
    }
    let (pipeline, src) = build(settings, sink, counters)?;
    Ok(Backend::Pipeline { pipeline, src })
}

/// Build and start an encoding pipeline for `settings`, its output to `sink`.
fn build(
    settings: &Settings,
    sink: &Sink,
    counters: &Counters,
) -> Result<(gst::Pipeline, gst_app::AppSrc)> {
    if settings.software {
        if settings.codec != Codec::Vp9 {
            bail!("encoding on the CPU is VP9 only");
        }
        let (encoder, profile) = software_encoder(settings.full_chroma, settings.bitrate);
        let description = format!(
            "appsrc name=src is-live=true format=time do-timestamp=true {LATEST} \
             ! {QUEUE} \
             ! {encoder} ! {profile} \
             ! {parser} ! {stream_caps} \
             ! appsink name=sink sync=false",
            parser = settings.codec.parser().0,
            stream_caps = settings.codec.stream_caps(),
        );
        return start(&description, sink, counters);
    }
    let (encoder, profile) = settings.codec.encoder(settings.full_chroma)?;
    // BT.601, limited range: the VA encoders do not write the colour space into the stream,
    // and decoders then assume BT.601. Converting with BT.709 shifts the colours.
    let raw = settings
        .codec
        .raw_format(settings.full_chroma, settings.ten_bits);
    let profile = profile_caps(
        settings.codec,
        profile,
        settings.ten_bits && settings.codec.has_ten_bits(),
    );
    let description = format!(
        "appsrc name=src is-live=true format=time do-timestamp=true {LATEST} \
         ! {QUEUE} \
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
    start(&description, sink, counters)
}

/// Build and start the pipeline of `description`, its output to `sink`.
fn start(
    description: &str,
    sink: &Sink,
    counters: &Counters,
) -> Result<(gst::Pipeline, gst_app::AppSrc)> {
    let pipeline = gst::parse::launch(description)?
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

    let counters = counters.clone();
    let sink = sink.clone();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |appsink| {
                let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer_owned().ok_or(gst::FlowError::Error)?;
                let size = buffer.size() as u64;
                counters.encoded.fetch_add(1, Ordering::Relaxed);
                counters.bytes.fetch_add(size, Ordering::Relaxed);
                counters.output.fetch_add(size, Ordering::Relaxed);
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
    Ok((pipeline, src))
}

/// The output caps of the encoder, which pick its profile: in 10 bits, HEVC's 10-bit
/// counterpart (`main-10`, `main-444-10`); AV1's Main profile has both depths.
fn profile_caps(codec: Codec, profile: &str, ten_bits: bool) -> String {
    if ten_bits && codec == Codec::Hevc {
        format!("{profile}-10")
    } else {
        profile.to_string()
    }
}

/// The GStreamer raw format of a pixman format, as QEMU names the formats of the framebuffers
/// it keeps in memory (in a 32-bit word, from the most significant end: `x8r8g8b8` is
/// B, G, R, X in memory on a little-endian host).
pub fn pixman_format(code: u32) -> Option<gst_video::VideoFormat> {
    use gst_video::VideoFormat as F;
    match code {
        0x2002_0888 => Some(F::Bgrx), // x8r8g8b8
        0x2002_8888 => Some(F::Bgra), // a8r8g8b8
        0x2003_0888 => Some(F::Rgbx), // x8b8g8r8
        0x2003_8888 => Some(F::Rgba), // a8b8g8r8
        _ => None,
    }
}

/// Where a framebuffer's pixels are, for what encodes them.
enum Pixels {
    /// For a pipeline.
    Memory(gst::Memory),
    /// For the GPU's own API.
    Direct(Arc<Source>),
}

/// A guest framebuffer, ready to be pushed as often as it changes.
struct Framebuffer {
    caps: gst::Caps,
    pixels: Pixels,
    format: gst_video::VideoFormat,
    width: u32,
    height: u32,
    offset: usize,
    stride: i32,
}

/// One console's encoder.
pub struct Encoder {
    backend: Backend,
    sink: Sink,
    framebuffer: Option<Framebuffer>,
    counters: Counters,
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
        let counters = Counters::default();
        let sink: Sink = Arc::new(std::sync::Mutex::new(Box::new(sink)));
        let backend = backend(settings, &sink, &counters)?;
        Ok(Self {
            backend,
            sink,
            framebuffer: None,
            counters,
            last_frame: std::cell::Cell::new(std::time::Instant::now()),
        })
    }

    /// Encode with new `settings`, from the next frame on: a new pipeline, whose first frame,
    /// encoded now from the current framebuffer, is a keyframe. Some encoders cannot change
    /// their input format once started (the HEVC one keeps the format it started with), so a
    /// change of depth needs a new one. The old pipeline stops first: none of its frames comes
    /// after the new keyframe.
    pub fn restart(&mut self, settings: &Settings) -> Result<()> {
        match &mut self.backend {
            Backend::Pipeline { pipeline, src } => {
                let (new_pipeline, new_src) = build(settings, &self.sink, &self.counters)?;
                let _ = pipeline.set_state(gst::State::Null);
                *pipeline = new_pipeline;
                *src = new_src;
            }
            Backend::Direct(direct) => direct.restart(settings)?,
        }
        self.frame()
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
        let (fd, size) = own(fd, offset, width, stride, height)?;
        let pixels = match &self.backend {
            // SAFETY: the descriptor is a DMA-BUF, and owned by the memory from now on.
            Backend::Pipeline { .. } => {
                Pixels::Memory(unsafe { DmaBufAllocator::new().alloc_dmabuf(fd, size)? })
            }
            // Imported by the GPU, which reads it where it is: in GPU memory with virgl, read
            // by the CPU through the bus, it would take a good part of a second a frame.
            Backend::Direct(_) => Pixels::Direct(Arc::new(Source::DmaBuf {
                fd,
                size: u32::try_from(size).context("framebuffer size")?,
                fourcc,
            })),
        };
        self.install(pixels, format, width, height, offset, stride)
    }

    /// A new framebuffer in memory shared with QEMU: `fd` holds `width`x`height` pixels in
    /// `format`, from `offset`. The encoder reads it in place, and uploads it to the GPU when
    /// it encodes there.
    pub fn set_shared_framebuffer(
        &mut self,
        fd: &OwnedFd,
        width: u32,
        height: u32,
        format: gst_video::VideoFormat,
        offset: u32,
        stride: u32,
    ) -> Result<()> {
        let (fd, size) = own(fd, offset, width, stride, height)?;
        let pixels = match &self.backend {
            // SAFETY: the descriptor is a mappable file (a memfd), and owned by the memory
            // from now on.
            Backend::Pipeline { .. } => Pixels::Memory(unsafe {
                FdAllocator::new().alloc_fd(fd.into_raw_fd(), size, FdMemoryFlags::NONE)?
            }),
            Backend::Direct(_) => Pixels::Direct(Arc::new(Source::Mapped(Mapping::new(fd, size)?))),
        };
        self.install(pixels, format, width, height, offset, stride)
    }

    fn install(
        &mut self,
        pixels: Pixels,
        format: gst_video::VideoFormat,
        width: u32,
        height: u32,
        offset: u32,
        stride: u32,
    ) -> Result<()> {
        let stride = i32::try_from(stride).context("framebuffer stride")?;
        let caps = gst_video::VideoInfo::builder(format, width, height)
            .fps(gst::Fraction::new(0, 1))
            .build()?
            .to_caps()?;
        self.framebuffer = Some(Framebuffer {
            caps,
            pixels,
            format,
            width,
            height,
            offset: offset as usize,
            stride,
        });
        Ok(())
    }

    /// The guest has drawn: encode the framebuffer as it is now.
    pub fn frame(&self) -> Result<()> {
        let Some(fb) = &self.framebuffer else {
            return Ok(());
        };
        match (&self.backend, &fb.pixels) {
            (Backend::Pipeline { src, .. }, Pixels::Memory(memory)) => {
                if src.caps().as_ref() != Some(&fb.caps) {
                    src.set_caps(Some(&fb.caps));
                }
                let mut buffer = gst::Buffer::new();
                {
                    let buffer = buffer.get_mut().unwrap();
                    buffer.append_memory(memory.clone());
                    // The layout of the pixels in the DMA-BUF, which the caps alone do not
                    // give.
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
                src.push_buffer(buffer)
                    .map_err(|e| anyhow!("pushing a frame: {e:?}"))?;
            }
            (Backend::Direct(direct), Pixels::Direct(source)) => direct.frame(Picture {
                source: source.clone(),
                format: fb.format,
                width: fb.width,
                height: fb.height,
                offset: fb.offset,
                stride: fb.stride as usize,
            }),
            // An encoder keeps its backend, and its framebuffers are made for it.
            _ => bail!("a framebuffer made for another encoder"),
        }
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
        match &self.backend {
            Backend::Pipeline { src, .. } => {
                src.send_event(
                    gst_video::DownstreamForceKeyUnitEvent::builder()
                        .all_headers(true)
                        .build(),
                );
            }
            Backend::Direct(direct) => direct.keyframe(),
        }
        self.frame()
    }

    /// Change the target bitrate, in kbit/s, and the rate control buffer with it. The VA
    /// encoders of GStreamer 1.26 reconfigure for it, which resets the GOP and costs a
    /// keyframe; patched (the `.daas` build), they take it with the next frame, as the GPU's
    /// own API does.
    pub fn set_bitrate(&self, bitrate: u32) {
        let pipeline = match &self.backend {
            Backend::Pipeline { pipeline, .. } => pipeline,
            Backend::Direct(direct) => return direct.set_bitrate(bitrate),
        };
        if let Some(encoder) = pipeline.by_name("encoder") {
            if encoder.find_property("target-bitrate").is_some() {
                // libvpx, which takes it with the next frame.
                let bps = i32::try_from(u64::from(bitrate) * 1000).unwrap_or(i32::MAX);
                encoder.set_property("target-bitrate", bps);
            } else {
                encoder.set_property("cpb-size", cpb_size(bitrate));
                encoder.set_property("bitrate", bitrate);
            }
        }
    }

    /// The head is off: nothing to encode until the next framebuffer.
    pub fn clear_framebuffer(&mut self) {
        self.framebuffer = None;
    }

    /// Bytes encoded since the encoder started.
    pub fn output(&self) -> u64 {
        self.counters.output.load(Ordering::Relaxed)
    }

    /// Frames and bytes encoded since the last call.
    pub fn take_counters(&self) -> (u64, u64) {
        (
            self.counters.encoded.swap(0, Ordering::Relaxed),
            self.counters.bytes.swap(0, Ordering::Relaxed),
        )
    }

    /// Finish the stream: the last frames go through before this returns.
    pub fn finish(mut self) {
        match &mut self.backend {
            Backend::Pipeline { pipeline, src } => {
                let _ = src.end_of_stream();
                if let Some(bus) = pipeline.bus() {
                    let _ = bus.timed_pop_filtered(
                        gst::ClockTime::from_seconds(2),
                        &[gst::MessageType::Eos],
                    );
                }
                // Dropped next: the pipeline stops.
            }
            // No frame waits in it: the one being encoded goes through.
            Backend::Direct(direct) => direct.stop(),
        }
    }
}

impl Drop for Encoder {
    /// Stopped, finished or not: GStreamer wants a pipeline stopped before it goes.
    fn drop(&mut self) {
        // The direct encoder stops when it is dropped.
        if let Backend::Pipeline { pipeline, .. } = &self.backend {
            let _ = pipeline.set_state(gst::State::Null);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framebuffers_fit_their_file() {
        let file = rustix::fs::memfd_create("test", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
        rustix::fs::ftruncate(&file, 64 * 4 * 8).unwrap();
        // 64x8 pixels, 256-byte rows: the whole file.
        assert!(own(&file, 0, 64, 256, 8).is_ok());
        // One row more than the file holds.
        assert!(own(&file, 0, 64, 256, 9).is_err());
        assert!(own(&file, 4, 64, 256, 8).is_err());
        // Rows shorter than their pixels.
        assert!(own(&file, 0, 64, 255, 8).is_err());
    }

    /// A real encoder, on the CPU: the pipeline builds, and a framebuffer comes out encoded.
    #[test]
    fn a_framebuffer_is_encoded() {
        gst::init().unwrap();
        let (width, height) = (64, 64);
        let file = rustix::fs::memfd_create("fb", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
        rustix::fs::ftruncate(&file, u64::from(width * height * 4)).unwrap();
        let settings = Settings {
            codec: Codec::Vp9,
            full_chroma: false,
            bitrate: 1000,
            ten_bits: false,
            software: true,
            max_bitrate: 1000,
            direct: true,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let mut encoder = Encoder::new(&settings, move |frame: &gst::Buffer| {
            let _ = tx.send(frame.size());
        })
        .unwrap();
        encoder
            .set_shared_framebuffer(
                &file,
                width,
                height,
                gst_video::VideoFormat::Bgrx,
                0,
                width * 4,
            )
            .unwrap();
        encoder.keyframe().unwrap();
        let size = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert!(size > 0);
        encoder.finish();
    }

    /// Three frames of a framebuffer in shared memory, encoded on the GPU's own API in
    /// `codec`: a keyframe, a frame after a change of bitrate, and the keyframe of a restart,
    /// each with whether it is a delta unit. `None` where no GPU encodes `codec` that way.
    fn encoded_directly(codec: Codec, full_chroma: bool) -> Option<Vec<(Vec<u8>, bool)>> {
        gst::init().unwrap();
        let (width, height) = (320, 240);
        let file = rustix::fs::memfd_create("fb", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
        rustix::fs::ftruncate(&file, u64::from(width * height * 4)).unwrap();
        let settings = Settings {
            codec,
            full_chroma,
            bitrate: 1000,
            ten_bits: false,
            software: false,
            max_bitrate: 4000,
            direct: true,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let Ok(mut encoder) = Encoder::new(&settings, move |frame: &gst::Buffer| {
            let delta = frame.flags().contains(gst::BufferFlags::DELTA_UNIT);
            let _ = tx.send((frame.map_readable().unwrap().to_vec(), delta));
        }) else {
            eprintln!("no {codec:?} encoder: skipped");
            return None;
        };
        if !matches!(encoder.backend, Backend::Direct(_)) {
            eprintln!("no direct {codec:?} encoder: skipped");
            return None;
        }
        encoder
            .set_shared_framebuffer(
                &file,
                width,
                height,
                gst_video::VideoFormat::Bgrx,
                0,
                width * 4,
            )
            .unwrap();
        let wait = std::time::Duration::from_secs(10);
        let mut frames = Vec::new();
        encoder.keyframe().unwrap();
        frames.push(rx.recv_timeout(wait).unwrap());
        encoder.set_bitrate(2000);
        encoder.frame().unwrap();
        frames.push(rx.recv_timeout(wait).unwrap());
        encoder.restart(&settings).unwrap();
        frames.push(rx.recv_timeout(wait).unwrap());
        encoder.finish();
        Some(frames)
    }

    /// HEVC on the GPU's own API: a keyframe with its parameter sets, then pictures predicted
    /// from it, a bitrate change included.
    #[test]
    fn hevc_is_encoded_directly() {
        let Some(frames) = encoded_directly(Codec::Hevc, true) else {
            return;
        };
        // The NAL unit types of a frame (H.265 table 7-1).
        let types = |frame: &[u8]| -> Vec<u8> {
            frame
                .windows(4)
                .filter(|w| w[..3] == [0, 0, 1])
                .map(|w| (w[3] >> 1) & 0x3f)
                .collect()
        };
        // VPS, SPS, PPS, IDR; a trailing picture; the keyframe of the restart.
        let expected: [(&[u8], bool); 3] = [
            (&[32, 33, 34, 19], false),
            (&[1], true),
            (&[32, 33, 34, 19], false),
        ];
        for ((frame, delta), (nal_types, delta_unit)) in frames.iter().zip(expected) {
            assert_eq!(types(frame), nal_types);
            assert_eq!(*delta, delta_unit);
        }
    }

    /// AV1 on the GPU's own API: temporal units of OBUs, a key frame with the sequence header,
    /// then frames predicted from it, a bitrate change included.
    #[test]
    fn av1_is_encoded_directly() {
        let Some(frames) = encoded_directly(Codec::Av1, false) else {
            return;
        };
        // The OBU types of a temporal unit (AV1 5.3.1, 6.2.2), each OBU with its size.
        let types = |mut unit: &[u8]| -> Vec<u8> {
            let mut types = Vec::new();
            while let Some(&header) = unit.first() {
                types.push((header >> 3) & 0x0f);
                let mut at = 1 + usize::from(header & 0x04 != 0);
                let (mut size, mut shift) = (0, 0);
                loop {
                    let byte = unit[at];
                    at += 1;
                    size |= usize::from(byte & 0x7f) << shift;
                    shift += 7;
                    if byte & 0x80 == 0 {
                        break;
                    }
                }
                unit = &unit[at + size..];
            }
            types
        };
        // Temporal delimiter, sequence header, frame header, tile group; then without the
        // sequence header; then a key frame again.
        let expected: [(&[u8], bool); 3] = [
            (&[2, 1, 3, 4], false),
            (&[2, 3, 4], true),
            (&[2, 1, 3, 4], false),
        ];
        for ((frame, delta), (obu_types, delta_unit)) in frames.iter().zip(expected) {
            assert_eq!(types(frame), obu_types);
            assert_eq!(*delta, delta_unit);
        }
    }

    #[test]
    fn ten_bits_are_announced() {
        assert!(Codec::Av1.deep_by("video/AV1;bit-depth=10"));
        assert!(Codec::Av1.deep_by("video/av1; bit-depth=10"));
        assert!(!Codec::Av1.deep_by("video/AV1"));
        assert!(!Codec::Vp9.deep_by("video/VP9;bit-depth=10"));
        assert!(Codec::Hevc.deep_by("video/H265;bit-depth=10"));
        assert!(!Codec::Hevc.deep_by("video/AV1;bit-depth=10"));
        assert_eq!(
            profile_caps(Codec::Hevc, "video/x-h265,profile=(string)main-444", true),
            "video/x-h265,profile=(string)main-444-10"
        );
        assert_eq!(profile_caps(Codec::Av1, "video/x-av1", true), "video/x-av1");
        assert!(Codec::Av1.decoded_by(false, "video/AV1;bit-depth=10"));
    }

    #[test]
    fn decoded_by() {
        assert!(Codec::Av1.decoded_by(false, "video/AV1"));
        assert!(Codec::Vp9.decoded_by(false, "video/vp9;profile-id=0"));
        assert!(Codec::Vp9.decoded_by(true, "video/VP9;profile-id=1"));
        assert!(!Codec::Vp9.decoded_by(true, "video/VP9;profile-id=0"));
        assert!(!Codec::Vp9.decoded_by(true, "video/VP9"));
        assert!(!Codec::Av1.decoded_by(false, "video/VP9"));
        assert!(Codec::Hevc.decoded_by(false, "video/H265"));
        assert!(Codec::Hevc.decoded_by(true, "video/H265;profile-id=4;tier-flag=0"));
        assert!(!Codec::Hevc.decoded_by(true, "video/H265;profile-id=1"));
        assert!(!Codec::Hevc.decoded_by(true, "video/H265"));
    }

    /// The fields of a profile_tier_level (H.265 7.3.3) from the first SPS of an HEVC stream in
    /// Annex B: general_profile_idc, the compatibility flags and the 48 constraint bits, each as
    /// the offer gives them.
    fn sps_profile(stream: &[u8]) -> (u32, String, String) {
        let mut nals = stream
            .windows(3)
            .enumerate()
            .filter(|(_, w)| *w == [0, 0, 1]);
        let start = nals
            .find(|&(i, _)| stream.get(i + 3).is_some_and(|b| (b >> 1) & 0x3f == 33))
            .map(|(i, _)| i + 5)
            .expect("no SPS");
        // The payload without its emulation prevention bytes.
        let mut rbsp = Vec::new();
        let mut zeros = 0;
        for &b in &stream[start..(start + 32).min(stream.len())] {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            rbsp.push(b);
        }
        // sps_video_parameter_set_id, sps_max_sub_layers_minus1, sps_temporal_id_nesting_flag:
        // one byte; then general_profile_space (2), general_tier_flag (1),
        // general_profile_idc (5): one byte; then 4 bytes of flags, 6 of constraints.
        let profile = u32::from(rbsp[1] & 0x1f);
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02X}")).collect::<String>();
        (profile, hex(&rbsp[2..6]), hex(&rbsp[6..12]))
    }

    /// The offer's HEVC parameters are those of the bitstream the VA encoder writes. Skipped
    /// where there is no VA HEVC encoder.
    #[test]
    fn hevc_offers_say_what_the_encoder_writes() {
        gst::init().unwrap();
        if gst::ElementFactory::find("vah265lpenc").is_none() {
            eprintln!("no vah265lpenc: skipped");
            return;
        }
        for (full_chroma, ten_bits) in [(false, false), (false, true), (true, false), (true, true)]
        {
            let (encoder, profile) = Codec::Hevc.encoder(full_chroma).unwrap();
            let description = format!(
                "videotestsrc num-buffers=1 ! video/x-raw,width=320,height=240 ! vapostproc \
                 ! video/x-raw(memory:VAMemory),format={raw} ! {encoder} \
                 ! {profile} ! h265parse ! video/x-h265,stream-format=byte-stream \
                 ! appsink name=sink",
                raw = Codec::Hevc.raw_format(full_chroma, ten_bits),
                profile = profile_caps(Codec::Hevc, profile, ten_bits),
            );
            let pipeline = gst::parse::launch(&description)
                .unwrap()
                .downcast::<gst::Pipeline>()
                .unwrap();
            let sink = pipeline
                .by_name("sink")
                .unwrap()
                .downcast::<gst_app::AppSink>()
                .unwrap();
            pipeline.set_state(gst::State::Playing).unwrap();
            let sample = sink.pull_sample().unwrap();
            let map = sample.buffer().unwrap().map_readable().unwrap();
            let (idc, compatibility, constraints) = sps_profile(&map);
            drop(map);
            pipeline.set_state(gst::State::Null).unwrap();
            let fmtp = Codec::Hevc.rtp_fmtp(full_chroma, ten_bits).unwrap();
            let expected = (
                format!("profile-id=(string){idc},"),
                format!("profile-compatibility-indicator=(string){compatibility},"),
                format!("interop-constraints=(string){constraints}"),
            );
            assert!(
                fmtp.contains(&expected.0)
                    && fmtp.contains(&expected.1)
                    && fmtp.contains(&expected.2),
                "4:4:4 {full_chroma}, 10 bits {ten_bits}: the encoder writes {expected:?}, the \
                 offer says {fmtp}"
            );
        }
    }
}
