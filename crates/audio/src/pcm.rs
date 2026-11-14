// SPDX-License-Identifier: MPL-2.0

//! The guest's sample formats, as QEMU describes them, to and from floating point samples in
//! [-1, 1]; and channels to and from stereo.

/// The sample format of the guest's sound, as QEMU gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pcm {
    pub bits: u8,
    pub signed: bool,
    pub float: bool,
    pub big_endian: bool,
    pub rate: u32,
    pub channels: u8,
}

/// The sample types this program takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sample {
    S8,
    U8,
    S16,
    U16,
    S32,
    F32,
}

impl Pcm {
    /// The sample type, when this program takes it and the format has sound (a rate and
    /// channels).
    fn sample(self) -> Option<Sample> {
        if self.rate == 0 || self.channels == 0 {
            return None;
        }
        Some(match (self.bits, self.float, self.signed) {
            (8, false, true) => Sample::S8,
            (8, false, false) => Sample::U8,
            (16, false, true) => Sample::S16,
            (16, false, false) => Sample::U16,
            (32, false, true) => Sample::S32,
            (32, true, _) => Sample::F32,
            _ => return None,
        })
    }

    /// Whether this program takes samples in this format.
    pub fn is_supported(self) -> bool {
        self.sample().is_some()
    }

    /// The bytes of a frame: one sample of each channel.
    pub fn frame_bytes(self) -> usize {
        usize::from(self.bits / 8) * usize::from(self.channels)
    }

    /// The samples in `bytes`, in [-1, 1], whole frames only.
    pub fn to_float(self, bytes: &[u8]) -> Vec<f32> {
        let Some(sample) = self.sample() else {
            return Vec::new();
        };
        let size = usize::from(self.bits / 8);
        let whole = bytes.len() - bytes.len() % self.frame_bytes().max(1);
        bytes[..whole]
            .chunks_exact(size)
            .map(|b| {
                let word = |n: usize| -> [u8; 4] {
                    let mut w = [0; 4];
                    if self.big_endian {
                        w[4 - n..].copy_from_slice(&b[..n]);
                        w.reverse();
                    } else {
                        w[..n].copy_from_slice(&b[..n]);
                    }
                    w
                };
                match sample {
                    Sample::S8 => f32::from(b[0] as i8) / 128.0,
                    Sample::U8 => (f32::from(b[0]) - 128.0) / 128.0,
                    Sample::S16 => {
                        let w = word(2);
                        f32::from(i16::from_le_bytes([w[0], w[1]])) / 32768.0
                    }
                    Sample::U16 => {
                        let w = word(2);
                        (f32::from(u16::from_le_bytes([w[0], w[1]])) - 32768.0) / 32768.0
                    }
                    Sample::S32 => i32::from_le_bytes(word(4)) as f32 / 2_147_483_648.0,
                    Sample::F32 => f32::from_le_bytes(word(4)).clamp(-1.0, 1.0),
                }
            })
            .collect()
    }

    /// `samples`, in [-1, 1], as bytes of this format.
    pub fn from_float(self, samples: &[f32]) -> Vec<u8> {
        let Some(sample) = self.sample() else {
            return Vec::new();
        };
        let mut bytes = Vec::with_capacity(samples.len() * usize::from(self.bits / 8));
        for &s in samples {
            let s = s.clamp(-1.0, 1.0);
            // The scale of `to_float`, both ways: -1 is the lowest value, and the highest is
            // just under 1.
            let scaled =
                |full: f64, low: f64, high: f64| (f64::from(s) * full).round().clamp(low, high);
            let mut word = match sample {
                Sample::S8 => vec![scaled(128.0, -128.0, 127.0) as i8 as u8],
                Sample::U8 => vec![(scaled(128.0, -128.0, 127.0) + 128.0) as u8],
                Sample::S16 => (scaled(32768.0, -32768.0, 32767.0) as i16)
                    .to_le_bytes()
                    .to_vec(),
                Sample::U16 => ((scaled(32768.0, -32768.0, 32767.0) + 32768.0) as u16)
                    .to_le_bytes()
                    .to_vec(),
                Sample::S32 => (scaled(2_147_483_648.0, -2_147_483_648.0, 2_147_483_647.0) as i32)
                    .to_le_bytes()
                    .to_vec(),
                Sample::F32 => s.to_le_bytes().to_vec(),
            };
            if self.big_endian {
                word.reverse();
            }
            bytes.extend(word);
        }
        bytes
    }
}

/// Interleaved samples of `channels` as stereo: mono on both sides, the first two of more.
pub fn to_stereo(samples: &[f32], channels: usize) -> Vec<f32> {
    match channels {
        2 => samples.to_vec(),
        1 => samples.iter().flat_map(|&s| [s, s]).collect(),
        0 => Vec::new(),
        n => samples.chunks_exact(n).flat_map(|f| [f[0], f[1]]).collect(),
    }
}

/// Interleaved stereo samples as `channels`: both sides mixed for mono, then silence past the
/// first two.
pub fn from_stereo(samples: &[f32], channels: usize) -> Vec<f32> {
    match channels {
        2 => samples.to_vec(),
        1 => samples
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[left, right]| (left + right) / 2.0)
            .collect(),
        0 => Vec::new(),
        n => samples
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|frame| {
                let mut wide = vec![0.0; n];
                wide[..2].copy_from_slice(frame);
                wide
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(bits: u8, signed: bool, float: bool, big_endian: bool) -> Pcm {
        Pcm {
            bits,
            signed,
            float,
            big_endian,
            rate: 48_000,
            channels: 2,
        }
    }

    #[test]
    fn formats_without_sound_are_refused() {
        assert!(pcm(16, true, false, false).is_supported());
        assert!(
            !Pcm {
                rate: 0,
                ..pcm(16, true, false, false)
            }
            .is_supported()
        );
        assert!(
            !Pcm {
                channels: 0,
                ..pcm(16, true, false, false)
            }
            .is_supported()
        );
        assert!(!pcm(24, true, false, false).is_supported());
    }

    /// Every format reads its extremes and zero as -1, 1 and 0, and writes them back the same.
    #[test]
    fn samples_go_both_ways() {
        let cases: [(Pcm, Vec<u8>, [f32; 2]); 6] = [
            (
                pcm(16, true, false, false),
                vec![0x00, 0x80, 0xff, 0x7f],
                [-1.0, 32767.0 / 32768.0],
            ),
            (
                pcm(16, true, false, true),
                vec![0x80, 0x00, 0x7f, 0xff],
                [-1.0, 32767.0 / 32768.0],
            ),
            (
                pcm(16, false, false, false),
                vec![0x00, 0x00, 0x00, 0x80],
                [-1.0, 0.0],
            ),
            (pcm(8, false, false, false), vec![0x00, 0x80], [-1.0, 0.0]),
            (
                pcm(32, true, false, false),
                vec![0, 0, 0, 0x80, 0, 0, 0, 0],
                [-1.0, 0.0],
            ),
            (
                pcm(32, true, true, false),
                [(-0.5f32).to_le_bytes(), 0.25f32.to_le_bytes()].concat(),
                [-0.5, 0.25],
            ),
        ];
        for (format, bytes, expected) in cases {
            let samples = format.to_float(&bytes);
            assert_eq!(samples, expected, "{format:?}");
            let back = format.from_float(&samples);
            assert_eq!(
                format.to_float(&back),
                format.to_float(&bytes),
                "{format:?}"
            );
        }
        // A part of a frame at the end is left out.
        assert_eq!(
            pcm(16, true, false, false)
                .to_float(&[0, 0, 0, 0, 0, 0])
                .len(),
            2
        );
    }

    #[test]
    fn channels() {
        assert_eq!(to_stereo(&[0.1, 0.2], 1), [0.1, 0.1, 0.2, 0.2]);
        assert_eq!(
            to_stereo(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6], 3),
            [0.1, 0.2, 0.4, 0.5]
        );
        assert_eq!(from_stereo(&[0.1, 0.3], 1), [0.2]);
        assert_eq!(from_stereo(&[0.1, 0.3], 3), [0.1, 0.3, 0.0]);
    }
}
