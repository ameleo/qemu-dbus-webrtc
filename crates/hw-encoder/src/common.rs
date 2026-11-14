// SPDX-License-Identifier: MPL-2.0

//! What the VA-API encoders of every codec share: the entrypoint, the checks of a
//! configuration, the rate control, and the headers written here.

use crate::error::{Error, Result};
use crate::va::{Buffer, Context, Display, sys};

/// The low-power entrypoint: the encoder of the GPU's fixed-function hardware, the only one
/// Intel GPUs have since Arc.
pub(crate) const ENTRYPOINT: sys::VAEntrypoint = sys::VAEntrypointEncSliceLP;

/// What the encoder is told of the distance between keyframes, which only come when asked:
/// the rate control plans for pictures predicted from the one before.
pub(crate) const INTRA_PERIOD: u32 = 1024;

/// Check that the GPU encodes `profile` with pictures of the family `rt_format`, at a
/// constant bitrate, and takes `headers` (`VA_ENC_PACKED_HEADER_*`) from the application;
/// `what` names the codec and profile in the error.
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
    if attribute(sys::VAConfigAttribEncPackedHeaders)
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

/// The rate control: a constant bitrate, which may change from one picture to the next.
pub(crate) struct RateControl {
    /// The middle of the driver's range of speed against quality, when it has one.
    quality: Option<u32>,
    framerate: u32,
    /// In bit/s.
    bitrate: u32,
    /// The encoder was not told of `bitrate` yet.
    changed: bool,
}

impl RateControl {
    pub fn new(display: &Display, profile: sys::VAProfile, framerate: u32, bitrate: u32) -> Self {
        let quality = display
            .attribute(profile, ENTRYPOINT, sys::VAConfigAttribEncQualityRange)
            .filter(|&levels| levels > 1)
            .map(|levels| levels.div_ceil(2));
        Self {
            quality,
            framerate,
            bitrate,
            changed: false,
        }
    }

    pub fn bitrate(&self) -> u32 {
        self.bitrate
    }

    /// Change the bitrate, in bit/s, from the next picture on.
    pub fn set_bitrate(&mut self, bitrate: u32) {
        if bitrate != self.bitrate {
            self.bitrate = bitrate;
            self.changed = true;
        }
    }

    /// The rate control buffer, in bits: half a second of the bitrate. It bounds the size of
    /// a picture, a keyframe's above all, and so the bursts on the network.
    fn buffer_size(&self) -> u32 {
        (self.bitrate / 2).max(1)
    }

    /// What the encoder is told with a picture: everything with a keyframe, the bitrate with
    /// the first picture after it changed, nothing otherwise.
    pub fn buffers(&self, context: &Context, keyframe: bool) -> Result<Vec<Buffer>> {
        let mut buffers = Vec::new();
        if keyframe {
            // Frames a second as a fraction: the numerator in the low 16 bits, the
            // denominator in the high ones. libva takes a denominator of 0 for 1, but Intel's
            // AV1 encoder refuses it.
            let framerate = sys::VAEncMiscParameterFrameRate {
                framerate: (self.framerate & 0xffff) | (1 << 16),
                ..Default::default()
            };
            // SAFETY: the structure of this parameter type.
            buffers.push(unsafe {
                Buffer::miscellaneous(context, sys::VAEncMiscParameterTypeFrameRate, &framerate)
            }?);
            if let Some(quality_level) = self.quality {
                let quality = sys::VAEncMiscParameterBufferQualityLevel {
                    quality_level,
                    ..Default::default()
                };
                // SAFETY: the structure of this parameter type.
                buffers.push(unsafe {
                    Buffer::miscellaneous(
                        context,
                        sys::VAEncMiscParameterTypeQualityLevel,
                        &quality,
                    )
                }?);
            }
        }
        if keyframe || self.changed {
            let buffer_size = self.buffer_size();
            let rate = sys::VAEncMiscParameterRateControl {
                bits_per_second: self.bitrate,
                // Constant bitrate: the target is the maximum.
                target_percentage: 100,
                // The time the buffer holds, in milliseconds.
                window_size: (u64::from(buffer_size) * 1000 / u64::from(self.bitrate.max(1)))
                    as u32,
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

    /// The encoder was given the buffers of a picture.
    pub fn told(&mut self) {
        self.changed = false;
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
