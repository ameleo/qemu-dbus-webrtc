// SPDX-License-Identifier: MPL-2.0

//! What the VA-API encoders of every codec share: the entrypoint, the checks of a
//! configuration, the rate control, and the headers written here.

use std::time::Instant;

use crate::error::{Error, Result};
use crate::va::{Buffer, Context, Display, sys};

/// The low-power entrypoint: the encoder of the GPU's fixed-function hardware, the only one
/// Intel GPUs have since Arc.
pub(crate) const ENTRYPOINT: sys::VAEntrypoint = sys::VAEntrypointEncSliceLP;

/// What the encoder is told of the distance between keyframes, which only come when asked:
/// the rate control plans for pictures predicted from the one before.
pub(crate) const INTRA_PERIOD: u32 = 1024;

/// Check that the GPU encodes `profile` with pictures of the family `rt_format`, at a
/// constant bitrate, and takes `headers` (`VA_ENC_PACKED_HEADER_*`, none when 0) from the
/// application; `what` names the codec and profile in the error.
pub(crate) fn check(
    display: &Display,
    profile: sys::VAProfile,
    rt_format: u32,
    headers: u32,
    what: &str,
) -> Result<()> {
    if !display.has(profile, ENTRYPOINT) {
        return Err(Error::Unsupported(what.into()));
    }
    let attribute = |attribute| display.attribute(profile, ENTRYPOINT, attribute);
    if attribute(sys::VAConfigAttribRTFormat).is_none_or(|formats| formats & rt_format == 0) {
        return Err(Error::Unsupported(format!("{what} pictures")));
    }
    if attribute(sys::VAConfigAttribRateControl).is_none_or(|modes| modes & sys::VA_RC_CBR == 0) {
        return Err(Error::Unsupported("constant bitrate".into()));
    }
    if headers != 0
        && attribute(sys::VAConfigAttribEncPackedHeaders)
            .is_none_or(|packed| packed & headers != headers)
    {
        return Err(Error::Unsupported("headers from the application".into()));
    }
    Ok(())
}

/// The size of a buffer that holds any coded picture of `width`x`height`: more than the
/// picture takes uncompressed in the largest format, which no coded picture reaches, and room
/// for the headers.
pub(crate) fn coded_size(width: u32, height: u32) -> usize {
    width as usize * height as usize * 3 * 2 + (1 << 16)
}

/// How fast the measured frame rate follows the frames: about over this many seconds.
const PACE_TIME_CONSTANT: f64 = 0.5;

/// The encoder is told a new bitrate when the one it should have moved by this share.
const RETELL_SHARE: f64 = 0.1;

/// The rate control: a constant bitrate, which may change from one picture to the next.
///
/// A VA encoder knows nothing of time: it gives each frame its bitrate divided by the frame
/// rate it was told. A desktop's frames come when it changes, from a few a second to the
/// most the stream allows (`framerate`). So the frame rate is measured here, and the encoder
/// is told it again when it moves.
pub(crate) struct RateControl {
    /// The middle of the driver's range of speed against quality, when it has one.
    quality: Option<u32>,
    /// The most frames a second the encoder gets.
    framerate: u32,
    /// In bit/s.
    bitrate: u32,
    /// When the last frame came, and the time between frames, averaged.
    last_frame: Option<Instant>,
    interval: f64,
    /// The bitrate, in bit/s, and the frame rate the encoder was last told; 0 before the
    /// first frame.
    told: (u32, u32),
    /// The bitrate changed since the encoder was last told.
    changed: bool,
}

impl RateControl {
    pub fn new(display: &Display, profile: sys::VAProfile, framerate: u32, bitrate: u32) -> Self {
        let quality = display
            .attribute(profile, ENTRYPOINT, sys::VAConfigAttribEncQualityRange)
            .filter(|&levels| levels > 1)
            .map(|levels| levels.div_ceil(2));
        let framerate = framerate.max(1);
        Self {
            quality,
            framerate,
            bitrate,
            last_frame: None,
            interval: 1.0 / f64::from(framerate),
            told: (0, 0),
            changed: false,
        }
    }

    /// Change the bitrate, in bit/s, from the next picture on.
    pub fn set_bitrate(&mut self, bitrate: u32) {
        if bitrate != self.bitrate {
            self.bitrate = bitrate;
            self.changed = true;
        }
    }

    /// A frame comes now: measure the frame rate.
    pub fn frame(&mut self) {
        self.frame_at(Instant::now());
    }

    fn frame_at(&mut self, now: Instant) {
        if let Some(last) = self.last_frame.replace(now) {
            // Within the fastest the encoder was told and a frame a second, the slowest the
            // average takes in.
            let dt = (now - last)
                .as_secs_f64()
                .clamp(1.0 / f64::from(self.framerate), 1.0);
            let weight = (dt / PACE_TIME_CONSTANT).min(1.0);
            self.interval += (dt - self.interval) * weight;
        }
    }

    /// The frame rate measured, at most the one the encoder was told.
    pub fn measured(&self) -> f64 {
        (1.0 / self.interval).min(f64::from(self.framerate))
    }

    /// The bitrate, in bit/s.
    pub fn bitrate(&self) -> u32 {
        self.bitrate
    }

    /// The frame rate to tell the encoder, in whole frames a second.
    fn told_framerate(&self) -> u32 {
        (self.measured().round() as u32).clamp(1, self.framerate)
    }

    /// Whether the encoder should be told the frame rate again: it moved by `RETELL_SHARE`.
    fn retell(&self) -> bool {
        let (now, told) = (f64::from(self.told_framerate()), f64::from(self.told.1));
        (now - told).abs() > told * RETELL_SHARE
    }

    /// The rate control buffer, in bits: half a second of the bitrate. It bounds the size of a
    /// picture, a keyframe's above all, and so the bursts on the network.
    fn buffer_size(&self) -> u32 {
        (self.bitrate / 2).max(1)
    }

    /// What the encoder is told with a picture: everything with a keyframe, the bitrate when
    /// it moved, nothing otherwise.
    pub fn buffers(&self, context: &Context, keyframe: bool) -> Result<Vec<Buffer>> {
        let mut buffers = Vec::new();
        let retell = keyframe || self.changed || self.retell();
        if retell {
            // Frames a second as a fraction: the numerator in the low 16 bits, the
            // denominator in the high ones. libva takes a denominator of 0 for 1, but Intel's
            // AV1 encoder refuses it.
            let framerate = sys::VAEncMiscParameterFrameRate {
                framerate: (self.told_framerate() & 0xffff) | (1 << 16),
                ..Default::default()
            };
            // SAFETY: the structure of this parameter type.
            buffers.push(unsafe {
                Buffer::miscellaneous(context, sys::VAEncMiscParameterTypeFrameRate, &framerate)
            }?);
        }
        if keyframe && let Some(quality_level) = self.quality {
            let quality = sys::VAEncMiscParameterBufferQualityLevel {
                quality_level,
                ..Default::default()
            };
            // SAFETY: the structure of this parameter type.
            buffers.push(unsafe {
                Buffer::miscellaneous(context, sys::VAEncMiscParameterTypeQualityLevel, &quality)
            }?);
        }
        if retell {
            let buffer_size = self.buffer_size();
            let rate = sys::VAEncMiscParameterRateControl {
                bits_per_second: self.bitrate(),
                // Constant bitrate: the target is the maximum.
                target_percentage: 100,
                // The time the buffer holds, in milliseconds: half a second.
                window_size: 500,
                ..Default::default()
            };
            let hrd = sys::VAEncMiscParameterHRD {
                initial_buffer_fullness: buffer_size / 2,
                buffer_size,
                ..Default::default()
            };
            // SAFETY: the structures of these parameter types.
            unsafe {
                buffers.push(Buffer::miscellaneous(
                    context,
                    sys::VAEncMiscParameterTypeRateControl,
                    &rate,
                )?);
                buffers.push(Buffer::miscellaneous(
                    context,
                    sys::VAEncMiscParameterTypeHRD,
                    &hrd,
                )?);
            }
        }
        Ok(buffers)
    }

    /// The encoder was given the buffers of a picture, a keyframe's when `keyframe`.
    pub fn told(&mut self, keyframe: bool) {
        if keyframe || self.changed || self.retell() {
            self.told = (self.bitrate, self.told_framerate());
            self.changed = false;
        }
    }
}

/// A header written here, for the encoder to put in the stream as it is. `has_emulation_bytes`
/// tells the driver not to add any: H.265's NAL units come with theirs, AV1 has none.
pub(crate) fn packed(
    context: &Context,
    buffers: &mut Vec<Buffer>,
    type_: sys::VAEncPackedHeaderType,
    header: &[u8],
) -> Result<()> {
    let parameters = sys::VAEncPackedHeaderParameterBuffer {
        type_,
        bit_length: header.len() as u32 * 8,
        has_emulation_bytes: 1,
        ..Default::default()
    };
    // SAFETY: the structure of this buffer type.
    buffers.push(unsafe {
        Buffer::parameter(
            context,
            sys::VAEncPackedHeaderParameterBufferType,
            &parameters,
        )
    }?);
    buffers.push(Buffer::bytes(
        context,
        sys::VAEncPackedHeaderDataBufferType,
        header,
    )?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn rate_control(framerate: u32) -> RateControl {
        RateControl {
            quality: None,
            framerate,
            bitrate: 10_000_000,
            last_frame: None,
            interval: 1.0 / f64::from(framerate),
            told: (0, 0),
            changed: false,
        }
    }

    /// Frames every `interval` from `start`, for `seconds`.
    fn frames(rate: &mut RateControl, start: Instant, interval: Duration, seconds: u32) -> Instant {
        let count =
            (Duration::from_secs(u64::from(seconds)).as_secs_f64() / interval.as_secs_f64()) as u32;
        let mut now = start;
        for _ in 0..count {
            now += interval;
            rate.frame_at(now);
        }
        now
    }

    #[test]
    fn the_frame_rate_is_measured() {
        let mut rate = rate_control(60);
        let start = Instant::now();
        // 25 frames a second, then 60, then more than the most: 60.
        let now = frames(&mut rate, start, Duration::from_millis(40), 3);
        assert_eq!(rate.told_framerate(), 25);
        let now = frames(&mut rate, now, Duration::from_micros(16_667), 3);
        assert_eq!(rate.told_framerate(), 60);
        let now = frames(&mut rate, now, Duration::from_millis(5), 3);
        assert_eq!(rate.told_framerate(), 60);
        // A pause longer than a second counts as a second: one frame a second at the least.
        frames(&mut rate, now, Duration::from_secs(5), 20);
        assert_eq!(rate.told_framerate(), 1);
    }

    #[test]
    fn the_encoder_is_told_again_when_the_rate_moves() {
        let mut rate = rate_control(60);
        rate.told(true);
        assert_eq!(rate.told, (10_000_000, 60));
        assert!(!rate.retell());
        // Within a tenth: not again.
        rate.interval = 1.0 / 56.0;
        assert!(!rate.retell());
        rate.interval = 1.0 / 30.0;
        assert!(rate.retell());
        rate.told(false);
        assert_eq!(rate.told, (10_000_000, 30));
        assert!(!rate.retell());
        // A new bitrate is told with the next frame, the frame rate with it.
        rate.set_bitrate(5_000_000);
        assert!(rate.changed);
        rate.told(false);
        assert_eq!(rate.told, (5_000_000, 30));
        assert!(!rate.changed);
    }
}
