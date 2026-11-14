// SPDX-License-Identifier: MPL-2.0

//! Video encoding of one console: the guest framebuffer in, a compressed stream out, with the
//! `qdw-hw-encoder` crate and no media framework.
//!
//! On a GPU, a framebuffer in a DMA-BUF is imported by the GPU, which reads it where it is (with
//! virgl, in the GPU's own memory); one in memory shared with QEMU is copied to the GPU. The
//! GPU converts it to YUV and encodes it. On the CPU (VP9, with libvpx), the framebuffer is
//! read and converted there. The encoding runs on a thread of its own (see [`crate::direct`]).

use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use gstreamer as gst;
use qdw_hw_encoder as hw;
pub use qdw_hw_encoder::PixelFormat;

use crate::direct::{Direct, FRAMERATE, Mapping, Picture, Source};

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

    /// Its name on the command line and in the configuration.
    pub fn name(self) -> &'static str {
        match self {
            Self::Av1 => "av1",
            Self::Vp9 => "vp9",
            Self::Hevc => "hevc",
        }
    }

    /// The encoders' codec.
    pub(crate) fn hw(self) -> hw::Codec {
        match self {
            Self::Av1 => hw::Codec::Av1,
            Self::Vp9 => hw::Codec::Vp9,
            Self::Hevc => hw::Codec::Hevc,
        }
    }

    /// Whether the hardware encoder does 4:4:4.
    pub fn has_full_chroma(self) -> bool {
        matches!(self, Self::Vp9 | Self::Hevc)
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

    /// The caps of the encoded stream, as the viewers' pipelines take it: an AV1 temporal
    /// unit of OBUs with their sizes, a VP9 frame (no hidden frame: one picture each), an HEVC
    /// access unit in Annex B.
    pub fn stream_caps(self) -> &'static str {
        match self {
            Self::Av1 => "video/x-av1,stream-format=obu-stream,alignment=tu,parsed=true",
            Self::Vp9 => "video/x-vp9,alignment=super-frame,parsed=true",
            Self::Hevc => "video/x-h265,stream-format=byte-stream,alignment=au,parsed=true",
        }
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
    // Our own descriptor, which the framebuffer keeps.
    let fd = fd.as_fd().try_clone_to_owned()?;
    let size = rustix::fs::seek(&fd, rustix::fs::SeekFrom::End(0)).context("framebuffer size")?;
    let needed = u64::from(offset) + u64::from(stride) * u64::from(height);
    if size < needed {
        bail!("framebuffer of {size} bytes, {needed} needed");
    }
    Ok((fd, size as usize))
}

/// The pixels of a DRM format, which are the encoders' (DRM names the bytes from the most
/// significant end of a little-endian word: `XBGR8888` is R, G, B, X in memory). The alpha
/// byte is not looked at.
fn drm_format(fourcc: u32) -> Option<PixelFormat> {
    match &fourcc.to_le_bytes() {
        b"XB24" | b"AB24" => Some(PixelFormat::Rgbx),
        b"XR24" | b"AR24" => Some(PixelFormat::Bgrx),
        _ => None,
    }
}

/// The pixels of a pixman format, as QEMU names the formats of the framebuffers it keeps in
/// memory (in a 32-bit word, from the most significant end: `x8r8g8b8` is B, G, R, X in
/// memory on a little-endian host). The alpha byte is not looked at.
pub fn pixman_format(code: u32) -> Option<PixelFormat> {
    match code {
        0x2002_0888 | 0x2002_8888 => Some(PixelFormat::Bgrx), // x8r8g8b8, a8r8g8b8
        0x2003_0888 | 0x2003_8888 => Some(PixelFormat::Rgbx), // x8b8g8r8, a8b8g8r8
        _ => None,
    }
}

/// Where the encoded frames go.
pub(crate) type Sink = Arc<std::sync::Mutex<Box<dyn FnMut(&gst::Buffer) + Send>>>;

/// What an encoder produced.
#[derive(Default, Clone)]
pub(crate) struct Counters {
    /// Frames out of the encoder.
    pub(crate) encoded: Arc<AtomicU64>,
    /// Bytes out of the encoder.
    pub(crate) bytes: Arc<AtomicU64>,
    /// Bytes out of the encoder since it started.
    pub(crate) output: Arc<AtomicU64>,
    /// The time from a frame asked for to its encoding handed to the viewers, added over
    /// the frames, and the longest, in microseconds.
    pub(crate) latency: Arc<AtomicU64>,
    pub(crate) latency_max: Arc<AtomicU64>,
}

/// What an encoder produced over a while.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Produced {
    pub frames: u64,
    pub bytes: u64,
    /// From a frame asked for (the guest drew) to its encoding handed to the viewers: the
    /// mean over the frames, and the longest.
    pub latency: std::time::Duration,
    pub latency_max: std::time::Duration,
}

/// A guest framebuffer, ready to be encoded as often as it changes.
struct Framebuffer {
    source: Arc<Source>,
    format: PixelFormat,
    width: u32,
    height: u32,
    offset: usize,
    stride: usize,
}

/// One console's encoder.
pub struct Encoder {
    direct: Direct,
    framebuffer: Option<Framebuffer>,
    counters: Counters,
    /// When the last frame went into the encoder.
    last_frame: std::cell::Cell<std::time::Instant>,
}

impl Encoder {
    /// Start an encoder: on the first GPU that encodes `settings`, or on the CPU when they
    /// say so. Its output goes to `sink`, called once per encoded frame (an access unit in
    /// Annex B for HEVC, a VP9 frame, an AV1 temporal unit), on the encoder's thread.
    pub fn new(
        settings: &Settings,
        sink: impl FnMut(&gst::Buffer) + Send + 'static,
    ) -> Result<Self> {
        let counters = Counters::default();
        let sink: Sink = Arc::new(std::sync::Mutex::new(Box::new(sink)));
        let direct = Direct::new(settings, sink, counters.clone())?;
        Ok(Self {
            direct,
            framebuffer: None,
            counters,
            last_frame: std::cell::Cell::new(std::time::Instant::now()),
        })
    }

    /// Encode with new `settings`, from the next frame on: a new encoder, whose first frame,
    /// encoded now from the current framebuffer, is a keyframe.
    pub fn restart(&mut self, settings: &Settings) -> Result<()> {
        self.direct.restart(settings)?;
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
        let format = drm_format(fourcc).with_context(|| format!("DRM format {fourcc:#x}"))?;
        let (fd, size) = own(fd, offset, width, stride, height)?;
        let source = Source::DmaBuf {
            fd,
            size: u32::try_from(size).context("framebuffer size")?,
            fourcc,
        };
        self.install(source, format, width, height, offset, stride);
        Ok(())
    }

    /// A new framebuffer in memory shared with QEMU: `fd` holds `width`x`height` pixels in
    /// `format`, from `offset`.
    pub fn set_shared_framebuffer(
        &mut self,
        fd: &OwnedFd,
        width: u32,
        height: u32,
        format: PixelFormat,
        offset: u32,
        stride: u32,
    ) -> Result<()> {
        let (fd, size) = own(fd, offset, width, stride, height)?;
        let source = Source::Mapped(Mapping::new(&fd, size, false)?);
        self.install(source, format, width, height, offset, stride);
        Ok(())
    }

    fn install(
        &mut self,
        source: Source,
        format: PixelFormat,
        width: u32,
        height: u32,
        offset: u32,
        stride: u32,
    ) {
        self.framebuffer = Some(Framebuffer {
            source: Arc::new(source),
            format,
            width,
            height,
            offset: offset as usize,
            stride: stride as usize,
        });
    }

    /// The guest has drawn: encode the framebuffer as it is now.
    pub fn frame(&self) -> Result<()> {
        let Some(fb) = &self.framebuffer else {
            return Ok(());
        };
        self.direct.frame(Picture {
            asked: std::time::Instant::now(),
            source: fb.source.clone(),
            format: fb.format,
            width: fb.width,
            height: fb.height,
            offset: fb.offset,
            stride: fb.stride,
        });
        self.last_frame.set(std::time::Instant::now());
        Ok(())
    }

    /// The shortest time between two frames: the encoders take up to [`FRAMERATE`] frames a
    /// second, and measure how many really come.
    pub fn frame_interval(&self) -> std::time::Duration {
        std::time::Duration::from_nanos(1_000_000_000 / u64::from(FRAMERATE))
    }

    /// How long ago the last frame went into the encoder.
    pub fn since_last_frame(&self) -> std::time::Duration {
        self.last_frame.get().elapsed()
    }

    /// Make the next frame a keyframe, and encode the framebuffer now: a new viewer, or one
    /// that lost a frame, needs it even if the guest does not draw.
    pub fn keyframe(&self) -> Result<()> {
        self.direct.keyframe();
        self.frame()
    }

    /// Change the target bitrate, in kbit/s, from the next frame on, without a keyframe.
    pub fn set_bitrate(&self, bitrate: u32) {
        self.direct.set_bitrate(bitrate);
    }

    /// The head is off: nothing to encode until the next framebuffer.
    pub fn clear_framebuffer(&mut self) {
        self.framebuffer = None;
    }

    /// Bytes encoded since the encoder started.
    pub fn output(&self) -> u64 {
        self.counters.output.load(Ordering::Relaxed)
    }

    /// What the encoder produced since the last call.
    pub fn take_counters(&self) -> Produced {
        let counters = &self.counters;
        let frames = counters.encoded.swap(0, Ordering::Relaxed);
        let total = counters.latency.swap(0, Ordering::Relaxed);
        let max = counters.latency_max.swap(0, Ordering::Relaxed);
        Produced {
            frames,
            bytes: counters.bytes.swap(0, Ordering::Relaxed),
            latency: std::time::Duration::from_micros(total.checked_div(frames).unwrap_or(0)),
            latency_max: std::time::Duration::from_micros(max),
        }
    }

    /// Finish the stream: the frame being encoded goes through before this returns.
    pub fn finish(mut self) {
        self.direct.stop();
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

    /// Three frames of a framebuffer in shared memory, encoded in `codec`, on the CPU when
    /// `software`: a keyframe, a frame after a change of bitrate, and the keyframe of a
    /// restart, each with whether it is a delta unit. `None` where no GPU encodes `codec`.
    fn encoded(codec: Codec, full_chroma: bool, software: bool) -> Option<Vec<(Vec<u8>, bool)>> {
        gst::init().unwrap();
        let (width, height) = (320, 240);
        let file = rustix::fs::memfd_create("fb", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
        rustix::fs::ftruncate(&file, u64::from(width * height * 4)).unwrap();
        let settings = Settings {
            codec,
            full_chroma,
            bitrate: 1000,
            ten_bits: false,
            software,
            max_bitrate: 4000,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let Ok(mut encoder) = Encoder::new(&settings, move |frame: &gst::Buffer| {
            let delta = frame.flags().contains(gst::BufferFlags::DELTA_UNIT);
            let _ = tx.send((frame.map_readable().unwrap().to_vec(), delta));
        }) else {
            eprintln!("no {codec:?} encoder: skipped");
            return None;
        };
        encoder
            .set_shared_framebuffer(&file, width, height, PixelFormat::Bgrx, 0, width * 4)
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
        let Some(frames) = encoded(Codec::Hevc, true, false) else {
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
        let Some(frames) = encoded(Codec::Av1, false, false) else {
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

    /// The frame markers of VP9 frames (VP9 bitstream specification 6.2): a key frame, an
    /// inter frame, a key frame, every one shown.
    fn check_vp9(frames: &[(Vec<u8>, bool)]) {
        for ((frame, delta), key) in frames.iter().zip([true, false, true]) {
            assert_eq!(frame[0] >> 6, 0b10, "frame_marker");
            // frame_type (0: KEY_FRAME), show_frame.
            assert_eq!(frame[0] & 0b100 == 0, key);
            assert_ne!(frame[0] & 0b10, 0);
            assert_eq!(*delta, !key);
        }
    }

    /// VP9 4:4:4 on the GPU's own API.
    #[test]
    fn vp9_is_encoded_on_the_gpu() {
        if let Some(frames) = encoded(Codec::Vp9, true, false) {
            check_vp9(&frames);
        }
    }

    /// VP9 4:4:4 on the CPU, with libvpx: on every host.
    #[test]
    fn vp9_is_encoded_on_the_cpu() {
        check_vp9(&encoded(Codec::Vp9, true, true).unwrap());
    }

    #[test]
    fn ten_bits_are_announced() {
        assert!(Codec::Av1.deep_by("video/AV1;bit-depth=10"));
        assert!(Codec::Av1.deep_by("video/av1; bit-depth=10"));
        assert!(!Codec::Av1.deep_by("video/AV1"));
        assert!(!Codec::Vp9.deep_by("video/VP9;bit-depth=10"));
        assert!(Codec::Hevc.deep_by("video/H265;bit-depth=10"));
        assert!(!Codec::Hevc.deep_by("video/AV1;bit-depth=10"));
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

    /// The offer's HEVC parameters are those of the bitstream the encoder writes, in each
    /// profile. Skipped where no GPU encodes HEVC.
    #[test]
    fn hevc_offers_say_what_the_encoder_writes() {
        for (full_chroma, ten_bits) in [(false, false), (false, true), (true, false), (true, true)]
        {
            let settings = hw::Settings {
                full_chroma,
                ten_bits,
                ..hw::Settings::new(hw::Codec::Hevc, 1_000_000)
            };
            let Ok(mut encoder) = hw::Encoder::open(None, settings) else {
                eprintln!("no HEVC encoder for {settings:?}: skipped");
                continue;
            };
            let pixels = vec![0x80; 320 * 240 * 4];
            let frame = hw::Frame {
                pixels: &pixels,
                format: PixelFormat::Bgrx,
                width: 320,
                height: 240,
                stride: 320 * 4,
            };
            let stream = encoder.encode(&frame, true).unwrap().data;
            let (idc, compatibility, constraints) = sps_profile(&stream);
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
