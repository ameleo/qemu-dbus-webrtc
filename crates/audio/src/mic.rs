// SPDX-License-Identifier: MPL-2.0

//! The guest's microphone: the clients' sound, converted to the format the guest records in
//! (its sample type, rate and channels).
//!
//! QEMU reads the buffer when it wants, and waits for the answer: the buffer gives at once what
//! it holds. It keeps 100 ms at most, the oldest dropped first, so that the delay cannot grow.

use std::collections::VecDeque;
use std::sync::Mutex;

use anyhow::{Result, anyhow};

use crate::pcm::{self, Pcm};
use crate::resample::Resampler;

/// The decoded sound of a client, as the microphone takes it.
pub const CANONICAL: &str = "audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=2";

/// [`CANONICAL`] as a sample format.
const CANONICAL_PCM: Pcm = Pcm {
    bits: 16,
    signed: true,
    float: false,
    big_endian: false,
    rate: 48_000,
    channels: 2,
};

/// What the buffer holds at most: 100 ms.
const MAX_DELAY_DIVISOR: usize = 10;

/// Samples in the guest's format.
struct Samples {
    bytes: VecDeque<u8>,
    /// The most it holds, in bytes: a whole number of frames.
    max: usize,
    /// The bytes of one frame. Whatever comes in and goes out does in whole frames: a part of
    /// one would shift every later sample.
    frame: usize,
    /// The guest's format, and the resampler from 48 kHz to its rate.
    format: Option<(Pcm, Resampler)>,
}

impl Samples {
    /// Add samples, in whole frames (a part of one at the end is left out), then drop the
    /// oldest past `max`.
    fn append(&mut self, samples: &[u8]) {
        let whole = samples.len() - samples.len() % self.frame;
        self.bytes.extend(&samples[..whole]);
        self.trim();
    }

    /// Drop the oldest frames past `max`.
    fn trim(&mut self) {
        let excess = self.bytes.len().saturating_sub(self.max);
        let excess = excess.div_ceil(self.frame) * self.frame;
        self.bytes.drain(..excess.min(self.bytes.len()));
    }

    /// Up to `size` bytes, in whole frames.
    fn take(&mut self, size: usize) -> Vec<u8> {
        let size = size.min(self.bytes.len());
        let size = size - size % self.frame;
        self.bytes.drain(..size).collect()
    }
}

/// The microphone of the guest.
pub struct Microphone {
    /// Samples in the guest's format, whole frames only (all channels of one instant).
    buffer: Mutex<Samples>,
}

impl Microphone {
    pub fn new() -> Result<Self> {
        Ok(Self {
            buffer: Mutex::new(Samples {
                bytes: VecDeque::new(),
                max: 0,
                frame: 1,
                format: None,
            }),
        })
    }

    /// The guest now records in this format.
    pub fn set_format(&self, pcm: Pcm) -> Result<()> {
        if !pcm.is_supported() {
            return Err(anyhow!("unsupported sample format {pcm:?}"));
        }
        let resampler = Resampler::new(CANONICAL_PCM.rate, pcm.rate, 2)?;
        let frame = pcm.frame_bytes().max(1);
        let mut samples = self.buffer.lock().unwrap();
        // Samples of the former format mean nothing in the new one.
        samples.bytes.clear();
        samples.format = Some((pcm, resampler));
        samples.frame = frame;
        samples.max = frame * (pcm.rate as usize / MAX_DELAY_DIVISOR);
        Ok(())
    }

    /// A client's sound, in [`CANONICAL`].
    pub fn push(&self, samples: &[u8]) {
        let mut buffer = self.buffer.lock().unwrap();
        let Some((pcm, resampler)) = &mut buffer.format else {
            return;
        };
        let pcm = *pcm;
        let resampled = resampler.process(&CANONICAL_PCM.to_float(samples));
        let guest = pcm.from_float(&pcm::from_stereo(&resampled, usize::from(pcm.channels)));
        buffer.append(&guest);
    }

    /// Up to `size` bytes of samples in the guest's format, at once, in whole frames.
    pub fn read(&self, size: usize) -> Vec<u8> {
        self.buffer.lock().unwrap().take(size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client's sound comes out in the guest's format: resampled to its rate, mixed to its
    /// channels, in whole frames.
    #[test]
    fn a_client_is_heard_in_the_guest_format() {
        let mic = Microphone::new().unwrap();
        // 44.1 kHz mono, 16 bits: frames of 2 bytes.
        mic.set_format(Pcm {
            bits: 16,
            signed: true,
            float: false,
            big_endian: false,
            rate: 44_100,
            channels: 1,
        })
        .unwrap();
        // 100 ms of the canonical format, a constant level: 0x1111 on both sides.
        for _ in 0..10 {
            mic.push(&[0x11; 480 * 4]);
        }
        let heard = mic.read(1 << 20);
        // About 100 ms at 44.1 kHz, mono 16 bits, less what the resampler holds back.
        assert_eq!(heard.len() % 2, 0);
        assert!(
            (3_500..=8_820).contains(&heard.len()),
            "{} bytes",
            heard.len()
        );
        // The level comes through, past the resampler's start.
        let tail = i16::from_le_bytes([heard[heard.len() - 2], heard[heard.len() - 1]]);
        assert!((0x1000..0x1200).contains(&tail), "{tail:#x}");
        // Another format: what was waiting is dropped.
        mic.set_format(Pcm {
            channels: 2,
            ..CANONICAL_PCM
        })
        .unwrap();
        assert!(mic.read(1 << 20).is_empty());
    }

    #[test]
    fn whole_frames_only() {
        // Frames of 4 bytes (16-bit stereo), 3 at most.
        let mut samples = Samples {
            bytes: (0..14).collect(),
            max: 12,
            frame: 4,
            format: None,
        };
        // 14 bytes, 12 at most: the oldest whole frame goes, not 2 bytes of it.
        samples.trim();
        assert_eq!(samples.bytes.len(), 10);
        assert_eq!(samples.bytes.front(), Some(&4));
        // 6 bytes asked: one whole frame given.
        assert_eq!(samples.take(6), [4, 5, 6, 7]);
        assert_eq!(samples.take(100), [8, 9, 10, 11]);
        // What is left is less than a frame: nothing.
        assert!(samples.take(100).is_empty());
        // What comes in does in whole frames too: 6 bytes are one frame, the rest left out.
        samples.bytes.clear();
        samples.append(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(samples.bytes.len(), 4);
        samples.append(&[7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18]);
        assert_eq!(
            samples.take(100),
            [7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18]
        );
    }
}
