// SPDX-License-Identifier: MPL-2.0

//! HEVC on a real GPU: the streams are decoded by a reference decoder (FFmpeg's), and the
//! pictures compared with what went in. Skipped where no GPU encodes HEVC; without `ffmpeg`,
//! only what the encoder returns is checked.

use std::io::Write;
use std::process::{Command, Stdio};

use qdw_hw_encoder::{Codec, Encoder, Error, Frame, PixelFormat, Settings};

/// The frames of a test: coloured bars, saturated, over a gradient, moving to the right.
fn picture(width: u32, height: u32, index: u32) -> Vec<u8> {
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

/// The NAL unit types of an HEVC byte stream, in order.
fn nal_unit_types(stream: &[u8]) -> Vec<u8> {
    stream
        .windows(4)
        .filter(|window| window[..3] == [0, 0, 1])
        .map(|window| (window[3] >> 1) & 0x3f)
        .collect()
}

/// The stream decoded by FFmpeg, as BGRX pictures of `width`x`height`, or `None` without
/// FFmpeg.
fn decoded(stream: &[u8], width: u32, height: u32) -> Option<Vec<Vec<u8>>> {
    let mut ffmpeg = Command::new("ffmpeg")
        .args(["-v", "error", "-f", "hevc", "-i", "-"])
        .args(["-f", "rawvideo", "-pix_fmt", "bgr0", "-"])
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
        "the reference decoder complains: {errors}"
    );
    let size = (width * height * 4) as usize;
    assert_eq!(
        output.stdout.len() % size,
        0,
        "the decoded pictures are not {width}x{height}"
    );
    Some(output.stdout.chunks(size).map(<[u8]>::to_vec).collect())
}

/// The peak signal-to-noise ratio of the colours of `decoded` against `source`, in dB.
fn psnr(source: &[u8], decoded: &[u8]) -> f64 {
    let (mut error, mut count) = (0.0, 0.0);
    for (a, b) in source.chunks(4).zip(decoded.chunks(4)) {
        for channel in 0..3 {
            let difference = f64::from(a[channel]) - f64::from(b[channel]);
            error += difference * difference;
            count += 1.0;
        }
    }
    10.0 * (255.0 * 255.0 * count / error.max(1e-9)).log10()
}

/// Encode a few frames of `width`x`height` and check the stream.
fn encode(full_chroma: bool, ten_bits: bool, width: u32, height: u32, lowest: f64) {
    let settings = Settings {
        full_chroma,
        ten_bits,
        max_bitrate: 4_000_000,
        ..Settings::new(Codec::Hevc, 2_000_000)
    };
    let mut encoder = match Encoder::open(None, settings) {
        Ok(encoder) => encoder,
        Err(Error::Unsupported(what)) => {
            eprintln!("skipped: {what}");
            return;
        }
        Err(error) => panic!("{error}"),
    };
    const FRAMES: u32 = 12;
    const KEYFRAME: u32 = 7;
    let mut stream = Vec::new();
    let mut sources = Vec::new();
    for index in 0..FRAMES {
        let pixels = picture(width, height, index);
        let frame = Frame {
            pixels: &pixels,
            format: PixelFormat::Bgrx,
            width,
            height,
            stride: width as usize * 4,
        };
        if index == 4 {
            encoder.set_bitrate(4_000_000);
        }
        let packet = encoder.encode(&frame, index == KEYFRAME).unwrap();
        let types = nal_unit_types(&packet.data);
        // A keyframe when asked and at the start, and only then: the parameter sets and an
        // IDR picture; otherwise a single trailing picture, a bitrate change included.
        if index == 0 || index == KEYFRAME {
            assert!(packet.keyframe, "frame {index} is not a keyframe");
            assert_eq!(types, [32, 33, 34, 19], "frame {index}");
        } else {
            assert!(!packet.keyframe, "frame {index} is a keyframe");
            assert_eq!(types, [1], "frame {index}");
        }
        stream.extend_from_slice(&packet.data);
        sources.push(pixels);
    }
    // In 4:2:0, a decoder shows whole chroma samples: an odd size comes out one larger.
    let step = if full_chroma { 1 } else { 2 };
    let (shown_width, shown_height) = (width.next_multiple_of(step), height.next_multiple_of(step));
    let Some(pictures) = decoded(&stream, shown_width, shown_height) else {
        eprintln!("no ffmpeg: the stream is not decoded");
        return;
    };
    assert_eq!(pictures.len(), FRAMES as usize);
    for (index, (source, picture)) in sources.iter().zip(&pictures).enumerate() {
        // The frame's own pixels, out of the picture shown.
        let shown: Vec<u8> = picture
            .chunks((shown_width * 4) as usize)
            .take(height as usize)
            .flat_map(|row| &row[..(width * 4) as usize])
            .copied()
            .collect();
        let quality = psnr(source, &shown);
        assert!(
            quality >= lowest,
            "frame {index}: {quality:.1} dB, {lowest} expected"
        );
    }
}

#[test]
fn main_profile() {
    encode(false, false, 322, 242, 30.0);
}

#[test]
fn main_10_profile() {
    encode(false, true, 322, 242, 30.0);
}

#[test]
fn main_444_profile() {
    encode(true, false, 322, 242, 36.0);
}

#[test]
fn main_444_10_profile() {
    encode(true, true, 322, 242, 36.0);
}

/// Sizes that are not whole coding blocks, nor whole chroma samples.
#[test]
fn odd_sizes() {
    encode(false, false, 321, 241, 30.0);
    encode(true, false, 321, 241, 36.0);
    encode(false, false, 1920, 1080, 30.0);
}

/// A new size starts a new sequence, with a keyframe.
#[test]
fn a_size_change_starts_again() {
    let Ok(mut encoder) = Encoder::open(None, Settings::new(Codec::Hevc, 2_000_000)) else {
        eprintln!("skipped: no HEVC encoder");
        return;
    };
    let mut stream = Vec::new();
    for (index, (width, height)) in [(320, 240), (320, 240), (640, 480), (640, 480)]
        .into_iter()
        .enumerate()
    {
        let pixels = picture(width, height, index as u32);
        let frame = Frame {
            pixels: &pixels,
            format: PixelFormat::Rgbx,
            width,
            height,
            stride: width as usize * 4,
        };
        let packet = encoder.encode(&frame, false).unwrap();
        assert_eq!(packet.keyframe, index % 2 == 0, "frame {index}");
        stream.extend_from_slice(&packet.data);
    }
    assert_eq!(
        nal_unit_types(&stream),
        [32, 33, 34, 19, 1, 32, 33, 34, 19, 1]
    );
}

/// Frames that cannot be encoded are refused, and the encoder goes on.
#[test]
fn bad_frames_are_refused() {
    let Ok(mut encoder) = Encoder::open(None, Settings::new(Codec::Hevc, 2_000_000)) else {
        eprintln!("skipped: no HEVC encoder");
        return;
    };
    let pixels = picture(320, 240, 0);
    let frame = Frame {
        pixels: &pixels,
        format: PixelFormat::Bgrx,
        width: 320,
        height: 240,
        stride: 320 * 4,
    };
    // Fewer pixels than the size says, rows shorter than their pixels, no pixels at all.
    let short = Frame {
        pixels: &pixels[..pixels.len() - 1],
        ..frame
    };
    assert!(matches!(
        encoder.encode(&short, false),
        Err(Error::Frame(_))
    ));
    let narrow = Frame {
        stride: 320 * 4 - 1,
        ..frame
    };
    assert!(matches!(
        encoder.encode(&narrow, false),
        Err(Error::Frame(_))
    ));
    let empty = Frame { width: 0, ..frame };
    assert!(matches!(
        encoder.encode(&empty, false),
        Err(Error::Frame(_))
    ));
    assert!(encoder.encode(&frame, false).unwrap().keyframe);
}
