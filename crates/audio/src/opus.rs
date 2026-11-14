// SPDX-License-Identifier: MPL-2.0

//! libopus, for the sound of the WebRTC sessions: 48 kHz stereo (RFC 7587 3.1 makes every Opus
//! stream on RTP 48 kHz, 2 channels in SDP), 16-bit samples.

use std::ffi::{CStr, c_int};

use anyhow::{Result, anyhow, bail};

/// The bindings bindgen generates from the installed headers.
#[allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    unused_imports,
    clippy::all
)]
mod sys {
    include!(concat!(env!("OUT_DIR"), "/opus.rs"));
}

/// The sample rate and channels of every stream here.
pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;

/// The largest Opus packet (RFC 6716 3.4: 1275 bytes a frame, up to 3 frames of 20 ms in
/// 60 ms; more than any 10 ms frame needs).
const MAX_PACKET: usize = 4000;

/// The longest frame a decoder may give: 120 ms, the longest Opus packet (RFC 6716 3.2.5).
const MAX_FRAME: usize = RATE as usize * 120 / 1000;

/// An error out of a libopus status.
fn check(call: &str, status: c_int) -> Result<c_int> {
    if status >= 0 {
        return Ok(status);
    }
    // SAFETY: opus_strerror returns a static string for any status.
    let message = unsafe { CStr::from_ptr(sys::opus_strerror(status)) };
    Err(anyhow!("{call}: {}", message.to_string_lossy()))
}

/// An Opus encoder of 48 kHz stereo.
pub struct Encoder(*mut sys::OpusEncoder);

// SAFETY: the encoder is only used through `&mut self`, by one thread at a time; libopus keeps
// no state outside it.
unsafe impl Send for Encoder {}

impl Encoder {
    /// An encoder of general audio (music, speech, the guest's sounds), at a constant
    /// `bitrate` in bit/s.
    pub fn new(bitrate: u32) -> Result<Self> {
        let mut error = 0;
        // SAFETY: a new encoder; libopus writes its status in `error`.
        let encoder = unsafe {
            sys::opus_encoder_create(
                RATE as i32,
                CHANNELS as c_int,
                sys::OPUS_APPLICATION_AUDIO as c_int,
                &mut error,
            )
        };
        check("opus_encoder_create", error)?;
        if encoder.is_null() {
            bail!("opus_encoder_create gave no encoder");
        }
        let encoder = Self(encoder);
        for (request, value) in [
            (sys::OPUS_SET_BITRATE_REQUEST, bitrate as c_int),
            // A constant bitrate, as before with GStreamer's opusenc: a steady share of the
            // connection.
            (sys::OPUS_SET_VBR_REQUEST, 0),
        ] {
            // SAFETY: both requests take an opus_int32, which is a C int here.
            check("opus_encoder_ctl", unsafe {
                sys::opus_encoder_ctl(encoder.0, request as c_int, value)
            })?;
        }
        Ok(encoder)
    }

    /// One frame of interleaved stereo samples (2.5, 5, 10, 20, 40 or 60 ms) in a packet.
    pub fn encode(&mut self, samples: &[i16]) -> Result<Vec<u8>> {
        let mut packet = vec![0; MAX_PACKET];
        // SAFETY: `samples` holds the frames given, `packet` its length.
        let length = check("opus_encode", unsafe {
            sys::opus_encode(
                self.0,
                samples.as_ptr(),
                (samples.len() / CHANNELS) as c_int,
                packet.as_mut_ptr(),
                packet.len() as i32,
            )
        })?;
        packet.truncate(length as usize);
        Ok(packet)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: the encoder made in `new`, destroyed once.
        unsafe { sys::opus_encoder_destroy(self.0) };
    }
}

/// An Opus decoder to 48 kHz stereo.
pub struct Decoder(*mut sys::OpusDecoder);

// SAFETY: as for the encoder.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Self> {
        let mut error = 0;
        // SAFETY: a new decoder; libopus writes its status in `error`.
        let decoder =
            unsafe { sys::opus_decoder_create(RATE as i32, CHANNELS as c_int, &mut error) };
        check("opus_decoder_create", error)?;
        if decoder.is_null() {
            bail!("opus_decoder_create gave no decoder");
        }
        Ok(Self(decoder))
    }

    /// The interleaved stereo samples of a packet.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<i16>> {
        let mut samples = vec![0; MAX_FRAME * CHANNELS];
        // SAFETY: `packet` and `samples` with their lengths; no forward error correction.
        let frames = check("opus_decode", unsafe {
            sys::opus_decode(
                self.0,
                packet.as_ptr(),
                packet.len() as i32,
                samples.as_mut_ptr(),
                MAX_FRAME as c_int,
                0,
            )
        })?;
        samples.truncate(frames as usize * CHANNELS);
        Ok(samples)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: the decoder made in `new`, destroyed once.
        unsafe { sys::opus_decoder_destroy(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tone goes through the encoder and the decoder and comes out the same, but for the
    /// codec's delay and its loss.
    #[test]
    fn a_tone_comes_back() {
        let mut encoder = Encoder::new(128_000).unwrap();
        let mut decoder = Decoder::new().unwrap();
        let tone = |i: usize| {
            ((i as f64 * 440.0 * std::f64::consts::TAU / 48_000.0).sin() * 8000.0) as i16
        };
        let mut decoded = Vec::new();
        for frame in 0..50 {
            let samples: Vec<i16> = (0..480)
                .flat_map(|i| {
                    let s = tone(frame * 480 + i);
                    [s, s]
                })
                .collect();
            let packet = encoder.encode(&samples).unwrap();
            assert!(
                !packet.is_empty() && packet.len() < 400,
                "{} bytes",
                packet.len()
            );
            decoded.extend(decoder.decode(&packet).unwrap());
        }
        assert_eq!(decoded.len(), 50 * 480 * 2);
        // The energy of the last half second, as loud as the tone's (its RMS is 8000/√2).
        let tail = &decoded[decoded.len() / 2..];
        let rms =
            (tail.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / tail.len() as f64).sqrt();
        assert!((5000.0..6300.0).contains(&rms), "rms {rms}");
    }
}
