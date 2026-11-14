// SPDX-License-Identifier: MPL-2.0

//! Encodes a scrolling page of coloured text-like marks and says how long each frame took
//! and what came out.
//!
//! ```text
//! bench [width height [frames [420|444 [8|10 [kbit/s [output.h265 [frames.bgrx [fps]]]]]]]]
//! ```
//!
//! The codec is `BENCH_CODEC`: `hevc` (the default), `av1`, `vp9`, or `vp9-cpu` for libvpx.
//!
//! The frames come at `fps` a second (30 by default), as a desktop's would; the bitrate is
//! halved for the third quarter of them. `frames.bgrx` gets the frames
//! as they went in, to compare with the decoded ones.

use std::time::{Duration, Instant};

use qdw_hw_encoder::{Codec, CpuEncoder, Encoder, Frame, Packet, PixelFormat, Result, Settings};

/// A page taller than the screen: rows of small coloured marks on white, like text.
fn page(width: u32, height: u32) -> Vec<u8> {
    let mut pixels = vec![0xff; (width * height * 4) as usize];
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut random = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for line in 0..height / 20 {
        let colour = [
            [0x20, 0x20, 0x20],
            [0xc0, 0x20, 0x20],
            [0x20, 0x80, 0x20],
            [0x20, 0x40, 0xc0],
        ][(line % 4) as usize];
        let mut x = 16;
        while x + 12 < width.saturating_sub(16) {
            let glyph = random();
            if glyph % 7 != 0 {
                for (dy, dx) in (0..12).flat_map(|dy| (0..8).map(move |dx| (dy, dx))) {
                    if (glyph >> ((dy * 5 + dx) % 64)) & 1 == 1 {
                        let at = (((line * 20 + 4 + dy) * width + x + dx) * 4) as usize;
                        pixels[at..at + 3].copy_from_slice(&colour);
                    }
                }
            }
            x += 10;
        }
    }
    pixels
}

/// The GPU's encoder or the CPU's.
enum Bench {
    Gpu(Box<Encoder>),
    Cpu(Box<CpuEncoder>),
}

impl Bench {
    fn encode(&mut self, frame: &Frame<'_>, keyframe: bool) -> Result<Packet> {
        match self {
            Self::Gpu(encoder) => encoder.encode(frame, keyframe),
            Self::Cpu(encoder) => encoder.encode(frame, keyframe),
        }
    }

    fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        match self {
            Self::Gpu(encoder) => {
                encoder.set_bitrate(bitrate);
                Ok(())
            }
            Self::Cpu(encoder) => encoder.set_bitrate(bitrate),
        }
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let (codec, cpu) = match std::env::var("BENCH_CODEC").as_deref() {
        Ok("av1") => (Codec::Av1, false),
        Ok("vp9") => (Codec::Vp9, false),
        Ok("vp9-cpu") => (Codec::Vp9, true),
        _ => (Codec::Hevc, false),
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let number = |index: usize, default: u32| -> u32 {
        args.get(index)
            .and_then(|arg| arg.parse().ok())
            .unwrap_or(default)
    };
    let (width, height) = (number(0, 1920), number(1, 1080));
    let frames = number(2, 300);
    let bitrate = number(5, 10_000) * 1000;
    let settings = Settings {
        full_chroma: number(3, 420) == 444,
        ten_bits: number(4, 8) == 10,
        ..Settings::new(codec, bitrate)
    };
    let fps = number(8, 30).max(1);
    let mut encoder = if cpu {
        println!("libvpx");
        Bench::Cpu(Box::new(CpuEncoder::new(settings)?))
    } else {
        let encoder = Encoder::open(None, settings)?;
        println!("{}", encoder.driver());
        Bench::Gpu(Box::new(encoder))
    };
    println!("{width}x{height}, {fps} frames a second, {settings:?}");

    let rows = height * 3;
    let page = page(width, rows);
    let stride = width as usize * 4;
    let mut output = Vec::new();
    let mut source = Vec::new();
    let mut quarters = [(Duration::ZERO, 0usize); 4];
    let begin = Instant::now();
    for index in 0..frames {
        let quarter = (index * 4 / frames) as usize;
        if index == frames / 2 {
            encoder.set_bitrate(bitrate / 2)?;
        } else if index == frames * 3 / 4 {
            encoder.set_bitrate(bitrate)?;
        }
        // Scrolls by 6 rows a frame, still for the last quarter.
        let top = if quarter == 3 {
            0
        } else {
            (index * 6) % (rows - height)
        };
        let frame = Frame {
            pixels: &page[top as usize * stride..][..height as usize * stride],
            format: PixelFormat::Bgrx,
            width,
            height,
            stride,
        };
        if args.get(7).is_some_and(|path| !path.is_empty()) {
            source.extend_from_slice(frame.pixels);
        }
        let due = begin + Duration::from_secs_f64(f64::from(index) / f64::from(fps));
        std::thread::sleep(due.saturating_duration_since(Instant::now()));
        let start = Instant::now();
        let packet = encoder.encode(&frame, false)?;
        quarters[quarter].0 += start.elapsed();
        quarters[quarter].1 += packet.data.len();
        if index > 0 && packet.keyframe {
            println!("frame {index}: an unexpected keyframe");
        }
        output.extend_from_slice(&packet.data);
    }
    for (quarter, (time, bytes)) in quarters.iter().enumerate() {
        let count = (frames / 4).max(1);
        println!(
            "quarter {}: {:.2} ms a frame, {:.0} kbit/s at {fps} frames a second",
            quarter + 1,
            time.as_secs_f64() * 1000.0 / f64::from(count),
            *bytes as f64 * 8.0 * f64::from(fps) / f64::from(count) / 1000.0,
        );
    }
    if let Some(path) = args.get(6).filter(|path| !path.is_empty()) {
        std::fs::write(path, &output)?;
        println!("{path}: {} bytes", output.len());
    }
    if let Some(path) = args.get(7).filter(|path| !path.is_empty()) {
        std::fs::write(path, &source)?;
    }
    Ok(())
}
