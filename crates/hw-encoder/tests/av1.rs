// SPDX-License-Identifier: MPL-2.0

//! AV1 on a real GPU: the streams are decoded by two decoders, dav1d (the browsers') and
//! libaom (the Alliance for Open Media's reference), and the pictures compared with what went
//! in. Skipped where no GPU encodes AV1; without `ffmpeg`, only what the encoder returns is
//! checked.

mod common;

use common::{check_quality, decoded, picture};
use qdw_hw_encoder::{Codec, Frame, PixelFormat, Settings};

/// The OBU types of a temporal unit in the low overhead format (AV1 5.3), in order: 1 the
/// sequence header, 2 the temporal delimiter, 3 a frame header, 4 a tile group, 6 a frame.
fn obu_types(mut unit: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    while let Some(&header) = unit.first() {
        assert_eq!(header & 0x80, 0, "obu_forbidden_bit");
        assert_ne!(header & 0x02, 0, "an OBU without its size");
        types.push((header >> 3) & 0x0f);
        let mut at = 1 + usize::from(header & 0x04 != 0);
        // leb128()
        let (mut size, mut shift) = (0usize, 0);
        loop {
            let byte = unit[at];
            at += 1;
            size |= usize::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
        }
        unit = &unit[at + size..];
    }
    types
}

/// Encode a few frames of `width`x`height` and check the stream with both decoders.
fn encode(ten_bits: bool, width: u32, height: u32, lowest: f64) {
    let settings = Settings {
        ten_bits,
        max_bitrate: 4_000_000,
        ..Settings::new(Codec::Av1, 2_000_000)
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
        let types = obu_types(&packet.data);
        // A key frame when asked and at the start, and only then, with the sequence header;
        // otherwise a frame predicted from the one before, a bitrate change included.
        if index == 0 || index == KEYFRAME {
            assert!(packet.keyframe, "frame {index} is not a key frame");
            assert_eq!(types, [2, 1, 3, 4], "frame {index}");
        } else {
            assert!(!packet.keyframe, "frame {index} is a key frame");
            assert_eq!(types, [2, 3, 4], "frame {index}");
        }
        stream.extend_from_slice(&packet.data);
        sources.push(pixels);
    }
    // AV1 shows the frame's own size, odd or not.
    for decoder in ["libdav1d", "libaom-av1"] {
        let Some(pictures) = decoded(&stream, "obu", decoder, (width, height), false, ten_bits)
        else {
            eprintln!("no ffmpeg: the stream is not decoded");
            return;
        };
        check_quality(&sources, &pictures, (width, height), false, lowest);
    }
}

#[test]
fn eight_bits() {
    encode(false, 322, 242, 0.0);
}

#[test]
fn ten_bits() {
    encode(true, 322, 242, 0.0);
}

/// Sizes that are not whole blocks, nor whole chroma samples.
#[test]
fn odd_sizes() {
    encode(false, 321, 241, 0.0);
    encode(false, 1920, 1080, 0.0);
}

/// A frame wider than a tile may be (4096 samples): two tile columns.
#[test]
fn several_tiles() {
    encode(false, 4352, 256, 0.0);
}

/// A new size starts a new sequence, with a key frame.
#[test]
fn a_size_change_starts_again() {
    let Some(mut encoder) = common::encoder(Settings::new(Codec::Av1, 2_000_000)) else {
        return;
    };
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
    }
}

/// 4:4:4 is profile 1, which this encoder does not make.
#[test]
fn no_full_chroma() {
    let settings = Settings {
        full_chroma: true,
        ..Settings::new(Codec::Av1, 2_000_000)
    };
    assert!(common::encoder(settings).is_none());
}
