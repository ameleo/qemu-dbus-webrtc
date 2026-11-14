// SPDX-License-Identifier: MPL-2.0

//! What the tests on a real GPU share: the frames, a reference decoder (FFmpeg's), and the
//! comparison of what comes out of it with what went in.

#![allow(dead_code)]

use std::io::Write;
use std::process::{Command, Stdio};

use qdw_hw_encoder::{Encoder, Error, Settings};

/// The frames of a test: coloured bars, saturated, over a gradient, moving to the right.
pub fn picture(width: u32, height: u32, index: u32) -> Vec<u8> {
    const BARS: [[u8; 3]; 8] = [
        [255, 255, 255],
        [255, 255, 0],
        [0, 255, 255],
        [0, 255, 0],
        [255, 0, 255],
        [255, 0, 0],
        [0, 0, 255],
        [0, 0, 0],
    ];
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let [r, g, b] = if y < height / 2 {
                BARS[(((x + index * 4) / 40) % 8) as usize]
            } else {
                let level = (x * 255 / width) as u8;
                [level, (y * 255 / height) as u8, 255 - level]
            };
            pixels.extend_from_slice(&[b, g, r, 0xff]);
        }
    }
    pixels
}

/// An encoder for `settings`, or `None` when no GPU encodes them: the test is then skipped.
pub fn encoder(settings: Settings) -> Option<Encoder> {
    match Encoder::open(None, settings) {
        Ok(encoder) => Some(encoder),
        Err(Error::Unsupported(what)) => {
            eprintln!("skipped: {what}");
            None
        }
        Err(error) => panic!("{error}"),
    }
}

/// The planes of a picture in YUV, 8-bit values (a 10-bit decoder's are divided by 4).
pub struct Planes {
    pub width: usize,
    pub height: usize,
    pub chroma_width: usize,
    pub chroma_height: usize,
    pub y: Vec<f64>,
    pub u: Vec<f64>,
    pub v: Vec<f64>,
}

/// What the encoder should have made of a BGRX picture: BT.601 limited range (Rec. ITU-R
/// BT.601, as the GPU converts), the chroma of 2x2 samples averaged in 4:2:0, the last column
/// and row repeated past an odd size.
pub fn reference(bgrx: &[u8], width: u32, height: u32, full_chroma: bool) -> Planes {
    let (width, height) = (width as usize, height as usize);
    let rgb = |x: usize, y: usize| {
        let i = (y.min(height - 1) * width + x.min(width - 1)) * 4;
        (
            f64::from(bgrx[i + 2]),
            f64::from(bgrx[i + 1]),
            f64::from(bgrx[i]),
        )
    };
    let luma = |(r, g, b): (f64, f64, f64)| 16.0 + (65.481 * r + 128.553 * g + 24.966 * b) / 255.0;
    let cb = |(r, g, b): (f64, f64, f64)| 128.0 + (-37.797 * r - 74.203 * g + 112.0 * b) / 255.0;
    let cr = |(r, g, b): (f64, f64, f64)| 128.0 + (112.0 * r - 93.786 * g - 18.214 * b) / 255.0;
    let step = if full_chroma { 1 } else { 2 };
    let (chroma_width, chroma_height) = (width.div_ceil(step), height.div_ceil(step));
    let mut planes = Planes {
        width,
        height,
        chroma_width,
        chroma_height,
        y: Vec::with_capacity(width * height),
        u: Vec::with_capacity(chroma_width * chroma_height),
        v: Vec::with_capacity(chroma_width * chroma_height),
    };
    for y in 0..height {
        for x in 0..width {
            planes.y.push(luma(rgb(x, y)));
        }
    }
    for cy in 0..chroma_height {
        for cx in 0..chroma_width {
            let samples: Vec<_> = (0..step * step)
                .map(|i| rgb(cx * step + i % step, cy * step + i / step))
                .collect();
            let count = samples.len() as f64;
            planes
                .u
                .push(samples.iter().copied().map(cb).sum::<f64>() / count);
            planes
                .v
                .push(samples.iter().copied().map(cr).sum::<f64>() / count);
        }
    }
    planes
}

/// The stream decoded by FFmpeg's `decoder` from its `format` (an FFmpeg demuxer), as YUV
/// pictures of `width`x`height` in their own chroma format and depth, or `None` without
/// FFmpeg. No conversion to RGB: FFmpeg's, for odd sizes, moves the chroma by a sample.
pub fn decoded(
    stream: &[u8],
    format: &str,
    decoder: &str,
    (width, height): (u32, u32),
    full_chroma: bool,
    ten_bits: bool,
) -> Option<Vec<Planes>> {
    let pix_fmt = match (full_chroma, ten_bits) {
        (false, false) => "yuv420p",
        (false, true) => "yuv420p10le",
        (true, false) => "yuv444p",
        (true, true) => "yuv444p10le",
    };
    let mut ffmpeg = Command::new("ffmpeg")
        .args(["-v", "error", "-f", format, "-c:v", decoder, "-i", "-"])
        .args(["-f", "rawvideo", "-pix_fmt", pix_fmt, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stdin = ffmpeg.stdin.take().unwrap();
    let stream = stream.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&stream));
    let output = ffmpeg.wait_with_output().unwrap();
    writer.join().unwrap().unwrap();
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && errors.trim().is_empty(),
        "{decoder} complains: {errors}"
    );
    let (width, height) = (width as usize, height as usize);
    let step = if full_chroma { 1 } else { 2 };
    let (chroma_width, chroma_height) = (width.div_ceil(step), height.div_ceil(step));
    let samples = width * height + 2 * chroma_width * chroma_height;
    let bytes = if ten_bits { 2 } else { 1 };
    let size = samples * bytes;
    assert_eq!(
        output.stdout.len() % size,
        0,
        "{decoder}: the decoded pictures are not {width}x{height}"
    );
    let value = |data: &[u8], i: usize| {
        if ten_bits {
            f64::from(u16::from_le_bytes([data[2 * i], data[2 * i + 1]])) / 4.0
        } else {
            f64::from(data[i])
        }
    };
    Some(
        output
            .stdout
            .chunks(size)
            .map(|data| {
                let plane = |start: usize, len: usize| {
                    (start..start + len).map(|i| value(data, i)).collect()
                };
                let luma = width * height;
                let chroma = chroma_width * chroma_height;
                Planes {
                    width,
                    height,
                    chroma_width,
                    chroma_height,
                    y: plane(0, luma),
                    u: plane(luma, chroma),
                    v: plane(luma + chroma, chroma),
                }
            })
            .collect(),
    )
}

/// The peak signal-to-noise ratio of the region of `decoded` that `reference` covers, in dB:
/// the luma, and both chroma planes together.
pub fn psnr(reference: &Planes, decoded: &Planes) -> (f64, f64) {
    let mse = |planes: &[(&[f64], &[f64])], width: usize, height: usize, stride: usize| {
        let (mut error, mut count) = (0.0, 0.0);
        for (expected, got) in planes {
            for y in 0..height {
                for x in 0..width {
                    let difference = expected[y * width + x] - got[y * stride + x];
                    error += difference * difference;
                    count += 1.0;
                }
            }
        }
        error / count
    };
    let db = |mse: f64| 10.0 * (255.0 * 255.0 / mse.max(1e-9)).log10();
    let luma = mse(
        &[(&reference.y, &decoded.y)],
        reference.width,
        reference.height,
        decoded.width,
    );
    let chroma = mse(
        &[(&reference.u, &decoded.u), (&reference.v, &decoded.v)],
        reference.chroma_width,
        reference.chroma_height,
        decoded.chroma_width,
    );
    (db(luma), db(chroma))
}

/// Every decoded picture holds its source in its top left corner, to at least `lowest` dB
/// in luma and in chroma.
pub fn check_quality(
    sources: &[Vec<u8>],
    pictures: &[Planes],
    (width, height): (u32, u32),
    full_chroma: bool,
    lowest: f64,
) {
    assert_eq!(pictures.len(), sources.len());
    let (mut worst_luma, mut worst_chroma) = (f64::MAX, f64::MAX);
    for (index, (source, picture)) in sources.iter().zip(pictures).enumerate() {
        let (luma, chroma) = psnr(&reference(source, width, height, full_chroma), picture);
        assert!(
            luma >= lowest && chroma >= lowest,
            "frame {index}: luma {luma:.1} dB, chroma {chroma:.1} dB, {lowest} expected"
        );
        worst_luma = worst_luma.min(luma);
        worst_chroma = worst_chroma.min(chroma);
    }
    // Seen with --nocapture.
    eprintln!("{width}x{height}: at worst luma {worst_luma:.1} dB, chroma {worst_chroma:.1} dB");
}
