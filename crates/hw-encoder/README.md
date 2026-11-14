# qdw-hw-encoder

Part of qemu-dbus-webrtc, and usable on its own.

Low-latency hardware video encoding of a desktop, straight on the GPU's own API, with no media
framework in between.

A frame goes in as it is in memory (RGB), and comes back encoded before the call returns. No
frame is held back and none refers to a later one. A keyframe is made when asked, and only
then; the bitrate changes with the next frame, without a keyframe.

```rust
use qdw_hw_encoder::{Codec, Encoder, Frame, PixelFormat, Settings};

let mut encoder = Encoder::open(None, Settings::new(Codec::Hevc, 10_000_000))?;
let packet = encoder.encode(&frame, false)?; // packet.data: an HEVC access unit, Annex B
encoder.set_bitrate(6_000_000);
```

## What it does today

| Codec | API | Chroma | Depth | Checked on |
|---|---|---|---|---|
| HEVC | VA-API (low-power encoder) | 4:2:0, 4:4:4 | 8, 10 bits | Intel Arc A380, iHD 25.2.6 |

The path of a frame: copied to an RGB surface in GPU memory (the only copy), converted to YUV
by the GPU's video processing (BT.601, limited range), coded by the GPU's encoder. The
parameter sets and slice headers are written here, from Rec. ITU-T H.265 (08/2021), and say
what the pictures are: their profile and level, and their colours.

Measured on the A380 at 3440x1440 in 4:4:4: about 2 ms for the copy and 8 ms for the
conversion and the encoding, 10 ms a frame.

## Not there yet

- AV1 and VP9 on VA-API.
- NVENC, for NVIDIA GPUs.
- Taking a DMA-BUF as it is, without the copy.
- Recovery from a lost frame without a keyframe (long-term reference pictures).

## Building

libva's development files (`libva-devel`, `libva-dev`) and libclang, which generates the
bindings to libva from the installed headers.

```sh
cargo test                      # the hardware tests are skipped where no GPU encodes HEVC
cargo run --release --example bench -- 1920 1080 300 444 8 10000 out.h265
```

The hardware tests decode the streams with `ffmpeg`, when it is installed, and compare the
pictures with what went in.

## Licence

MPL-2.0, as the rest of the project.
