// SPDX-License-Identifier: MPL-2.0

//! Recording of an encoded stream to a file, to check it with a reference decoder.
//!
//! HEVC and H.264 come out of the encoder in Annex B, which is a file format in itself. VP9
//! frames and AV1 temporal units carry no framing of their own, so they go in IVF: a 32-byte file header, then each
//! frame behind its size and timestamp.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

use crate::encode::Codec;

/// IVF timestamps are in this many units per second.
const IVF_TIMEBASE: u32 = 1000;

pub struct Recorder {
    file: File,
    /// For IVF: when the recording started, the origin of the frame timestamps.
    ivf_start: Option<Instant>,
}

impl Recorder {
    pub fn create(path: &Path, codec: Codec) -> io::Result<Self> {
        let mut file = File::create(path)?;
        let ivf_fourcc = match codec {
            Codec::Av1 => Some(b"AV01"),
            Codec::Vp9 => Some(b"VP90"),
            Codec::Hevc | Codec::H264 => None,
        };
        let ivf_start = match ivf_fourcc {
            Some(fourcc) => {
                let mut header = Vec::with_capacity(32);
                header.extend_from_slice(b"DKIF");
                header.extend_from_slice(&0u16.to_le_bytes()); // version
                header.extend_from_slice(&32u16.to_le_bytes()); // header size
                header.extend_from_slice(fourcc);
                // Width and height: decoders take them from the frames, which may change size.
                header.extend_from_slice(&0u16.to_le_bytes());
                header.extend_from_slice(&0u16.to_le_bytes());
                header.extend_from_slice(&IVF_TIMEBASE.to_le_bytes()); // timebase denominator
                header.extend_from_slice(&1u32.to_le_bytes()); // timebase numerator
                header.extend_from_slice(&0u32.to_le_bytes()); // frame count, unknown
                header.extend_from_slice(&0u32.to_le_bytes()); // unused
                file.write_all(&header)?;
                Some(Instant::now())
            }
            None => None,
        };
        Ok(Self { file, ivf_start })
    }

    pub fn write(&mut self, frame: &[u8]) -> io::Result<()> {
        if let Some(start) = self.ivf_start {
            let size = u32::try_from(frame.len()).map_err(io::Error::other)?;
            let timestamp = start.elapsed().as_millis() as u64 * u64::from(IVF_TIMEBASE) / 1000;
            self.file.write_all(&size.to_le_bytes())?;
            self.file.write_all(&timestamp.to_le_bytes())?;
        }
        self.file.write_all(frame)
    }
}
