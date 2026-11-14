// SPDX-License-Identifier: MPL-2.0

//! HEVC on a real GPU: the streams are decoded by a reference decoder (FFmpeg's), and the
//! pictures compared with what went in. Skipped where no GPU encodes HEVC; without `ffmpeg`,
//! only what the encoder returns is checked.

mod common;

use common::{check_quality, decoded, picture};
use qdw_hw_encoder::{Codec, Encoder, Error, Frame, PixelFormat, Settings};

/// The NAL unit types of an HEVC byte stream, in order.
fn nal_unit_types(stream: &[u8]) -> Vec<u8> {
    stream
        .windows(4)
        .filter(|window| window[..3] == [0, 0, 1])
        .map(|window| (window[3] >> 1) & 0x3f)
        .collect()
}

/// Encode a few frames of `width`x`height` and check the stream.
fn encode(full_chroma: bool, ten_bits: bool, width: u32, height: u32, lowest: f64) {
    let settings = Settings {
        full_chroma,
        ten_bits,
        max_bitrate: 4_000_000,
        ..Settings::new(Codec::Hevc, 2_000_000)
    };
    let Some(mut encoder) = common::encoder(settings) else {
        return;
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
    let shown = (width.next_multiple_of(step), height.next_multiple_of(step));
    let Some(pictures) = decoded(&stream, "hevc", "hevc", shown, full_chroma, ten_bits) else {
        eprintln!("no ffmpeg: the stream is not decoded");
        return;
    };
    check_quality(&sources, &pictures, (width, height), full_chroma, lowest);
}

#[test]
fn main_profile() {
    encode(false, false, 322, 242, 0.0);
}

#[test]
fn main_10_profile() {
    encode(false, true, 322, 242, 0.0);
}

#[test]
fn main_444_profile() {
    encode(true, false, 322, 242, 0.0);
}

#[test]
fn main_444_10_profile() {
    encode(true, true, 322, 242, 0.0);
}

/// Sizes that are not whole coding blocks, nor whole chroma samples.
#[test]
fn odd_sizes() {
    encode(false, false, 321, 241, 0.0);
    encode(true, false, 321, 241, 0.0);
    encode(false, false, 1920, 1080, 0.0);
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
