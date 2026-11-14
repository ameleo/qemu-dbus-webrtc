// SPDX-License-Identifier: MPL-2.0

//! VP9 on a real GPU, and on the CPU with libvpx: the streams are decoded by two decoders,
//! libvpx (the reference) and FFmpeg's own, and the pictures compared with what went in. The
//! GPU's tests are skipped where no GPU encodes VP9; without `ffmpeg`, only what the encoder
//! returns is checked.

mod common;

use common::{check_quality, decoded, picture};
use qdw_hw_encoder::{Codec, CpuEncoder, Frame, Packet, PixelFormat, Settings};

/// The frames in an IVF file, the container FFmpeg reads VP9 from: a 32-byte header, then
/// each frame behind its size and its timestamp.
fn ivf(frames: &[Vec<u8>], width: u32, height: u32) -> Vec<u8> {
    let mut file = Vec::new();
    file.extend_from_slice(b"DKIF");
    file.extend_from_slice(&0u16.to_le_bytes()); // version
    file.extend_from_slice(&32u16.to_le_bytes()); // header size
    file.extend_from_slice(b"VP90");
    file.extend_from_slice(&(width as u16).to_le_bytes());
    file.extend_from_slice(&(height as u16).to_le_bytes());
    file.extend_from_slice(&30u32.to_le_bytes()); // timebase denominator
    file.extend_from_slice(&1u32.to_le_bytes()); // numerator
    file.extend_from_slice(&(frames.len() as u32).to_le_bytes());
    file.extend_from_slice(&0u32.to_le_bytes());
    for (index, frame) in frames.iter().enumerate() {
        file.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        file.extend_from_slice(&(index as u64).to_le_bytes());
        file.extend_from_slice(frame);
    }
    file
}

/// `frame_type` and `show_frame` of a VP9 frame's uncompressed header (VP9 bitstream
/// specification 6.2), for profiles 0 and 1: after frame_marker (2 bits), the profile (2),
/// show_existing_frame (1).
fn key_and_shown(frame: &[u8]) -> (bool, bool) {
    let bits = frame[0];
    assert_eq!(bits >> 6, 0b10, "frame_marker");
    assert_eq!(bits & 0b0000_1000, 0, "show_existing_frame");
    ((bits & 0b0000_0100) == 0, (bits & 0b0000_0010) != 0)
}

/// The GPU's encoder or the CPU's, as one.
enum Either {
    Gpu(Box<qdw_hw_encoder::Encoder>),
    Cpu(Box<CpuEncoder>),
}

impl Either {
    fn encode(&mut self, frame: &Frame<'_>, keyframe: bool) -> Packet {
        match self {
            Self::Gpu(encoder) => encoder.encode(frame, keyframe).unwrap(),
            Self::Cpu(encoder) => encoder.encode(frame, keyframe).unwrap(),
        }
    }

    fn set_bitrate(&mut self, bitrate: u32) {
        match self {
            Self::Gpu(encoder) => encoder.set_bitrate(bitrate),
            Self::Cpu(encoder) => encoder.set_bitrate(bitrate).unwrap(),
        }
    }
}

fn encode(full_chroma: bool, width: u32, height: u32, lowest: f64) {
    let settings = Settings {
        full_chroma,
        max_bitrate: 4_000_000,
        ..Settings::new(Codec::Vp9, 2_000_000)
    };
    let Some(encoder) = common::encoder(settings) else {
        return;
    };
    check(
        Either::Gpu(Box::new(encoder)),
        full_chroma,
        width,
        height,
        lowest,
    );
}

/// Encode a few frames of `width`x`height` with `encoder` and check the stream.
fn check(mut encoder: Either, full_chroma: bool, width: u32, height: u32, lowest: f64) {
    const FRAMES: u32 = 12;
    const KEYFRAME: u32 = 7;
    let (mut frames, mut sources) = (Vec::new(), Vec::new());
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
        let packet = encoder.encode(&frame, index == KEYFRAME);
        let key = index == 0 || index == KEYFRAME;
        assert_eq!(packet.keyframe, key, "frame {index}");
        // One frame, shown: no hidden frame, no superframe.
        assert_eq!(key_and_shown(&packet.data), (key, true), "frame {index}");
        frames.push(packet.data);
        sources.push(pixels);
    }
    let file = ivf(&frames, width, height);
    for decoder in ["libvpx-vp9", "vp9"] {
        let Some(pictures) = decoded(&file, "ivf", decoder, (width, height), full_chroma, false)
        else {
            eprintln!("no ffmpeg: the stream is not decoded");
            return;
        };
        check_quality(&sources, &pictures, (width, height), full_chroma, lowest);
    }
}

#[test]
fn profile_0() {
    encode(false, 322, 242, 40.0);
}

#[test]
fn profile_1() {
    encode(true, 322, 242, 40.0);
}

/// Sizes that are not whole blocks, nor whole chroma samples, and a frame of two tile columns.
#[test]
fn other_sizes() {
    encode(false, 321, 241, 40.0);
    // 1080p in 4:4:4 at 2 Mbit/s: the bitrate, not the encoder, holds the chroma back.
    encode(true, 1920, 1080, 35.0);
    encode(false, 4352, 256, 40.0);
}

/// VP9 in 10 bits (profiles 2 and 3) is not made.
#[test]
fn no_ten_bits() {
    let settings = Settings {
        ten_bits: true,
        ..Settings::new(Codec::Vp9, 2_000_000)
    };
    assert!(common::encoder(settings).is_none());
}

/// The CPU's encoder: profiles 0 and 1, odd sizes included.
#[test]
fn on_the_cpu() {
    for (full_chroma, width, height) in [(false, 322, 242), (true, 322, 242), (false, 321, 241)] {
        let settings = Settings {
            full_chroma,
            ..Settings::new(Codec::Vp9, 2_000_000)
        };
        let encoder = CpuEncoder::new(settings).unwrap();
        // At 2 Mbit/s, real time: the CPU's encoder is less sharp than the GPU's here.
        check(
            Either::Cpu(Box::new(encoder)),
            full_chroma,
            width,
            height,
            38.0,
        );
    }
}

/// The CPU's encoder makes VP9 only.
#[test]
fn only_vp9_on_the_cpu() {
    assert!(CpuEncoder::new(Settings::new(Codec::Av1, 2_000_000)).is_err());
}
