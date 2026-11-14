// SPDX-License-Identifier: MPL-2.0

//! A stream of interleaved samples from one rate to another, in pieces of any length, with
//! rubato's FFT resampler. The same rate goes through untouched.

use anyhow::{Result, anyhow};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler as _};

/// The frames the resampler takes at once: 10 ms at 48 kHz.
const CHUNK: usize = 480;

pub struct Resampler {
    /// `None` when the rates are the same.
    fft: Option<Fft<f32>>,
    channels: usize,
    /// Samples not resampled yet: less than a chunk.
    pending: Vec<f32>,
}

impl Resampler {
    pub fn new(from: u32, to: u32, channels: usize) -> Result<Self> {
        let fft = if from == to {
            None
        } else {
            Some(
                Fft::<f32>::new(
                    from as usize,
                    to as usize,
                    CHUNK,
                    channels.max(1),
                    FixedSync::Input,
                )
                .map_err(|e| anyhow!("resampler from {from} to {to} Hz: {e}"))?,
            )
        };
        Ok(Self {
            fft,
            channels: channels.max(1),
            pending: Vec::new(),
        })
    }

    /// Resample `samples`, interleaved: what comes out is what whole chunks gave so far; the
    /// rest waits for the next samples.
    pub fn process(&mut self, samples: &[f32]) -> Vec<f32> {
        let Some(fft) = &mut self.fft else {
            return samples.to_vec();
        };
        self.pending.extend_from_slice(samples);
        let mut out = Vec::new();
        loop {
            let needed = fft.input_frames_next();
            if self.pending.len() < needed * self.channels {
                break;
            }
            let capacity = fft.output_frames_max();
            let mut output = vec![0.0; capacity * self.channels];
            let input = InterleavedSlice::new(&self.pending, self.channels, needed)
                .expect("the samples of `needed` frames");
            let mut output_adapter =
                InterleavedSlice::new_mut(&mut output, self.channels, capacity)
                    .expect("room for the most frames out");
            match fft.process_into_buffer(&input, &mut output_adapter, None) {
                Ok((used, written)) => {
                    self.pending.drain(..used * self.channels);
                    out.extend_from_slice(&output[..written * self.channels]);
                }
                Err(_) => {
                    // Cannot happen with buffers of these sizes; never loop on it.
                    self.pending.clear();
                    break;
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second at 48 kHz comes out as about a second at 44.1 kHz, a tone kept.
    #[test]
    fn rates_change() {
        let mut resampler = Resampler::new(48_000, 44_100, 1).unwrap();
        let tone: Vec<f32> = (0..48_000)
            .map(|i| (i as f32 * 1000.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5)
            .collect();
        // In pieces of 441 samples, as QEMU might give them.
        let out: Vec<f32> = tone
            .chunks(441)
            .flat_map(|c| resampler.process(c))
            .collect();
        // All but the last chunk's worth.
        assert!((43_000..=44_100).contains(&out.len()), "{}", out.len());
        // The tone's power (0.5²/2) is kept, past the resampler's start.
        let tail = &out[out.len() / 2..];
        let power = tail.iter().map(|s| s * s).sum::<f32>() / tail.len() as f32;
        assert!((0.11..0.14).contains(&power), "power {power}");
        // The same rate goes through untouched.
        let mut same = Resampler::new(48_000, 48_000, 2).unwrap();
        assert_eq!(same.process(&[0.1, 0.2]), [0.1, 0.2]);
    }
}
