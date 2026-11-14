# qdw-hw-encoder

Part of qemu-dbus-webrtc, and usable on its own.

Low-latency video encoding of a desktop, straight on the GPU's own API, or with libvpx on the
CPU, with no media framework in between.

A frame goes in, and comes back encoded before the call returns. No frame is held back and
none refers to a later one. A keyframe is made when asked, and only then; the bitrate changes
with the next frame, without a keyframe. The frames may come at any rate: the rate control
follows the one they really come at.

```rust
use qdw_hw_encoder::{Codec, Encoder, Frame, PixelFormat, Settings};

let mut encoder = Encoder::open(None, Settings::new(Codec::Av1, 10_000_000))?;
let packet = encoder.encode(&frame, false)?; // packet.data: an AV1 temporal unit
encoder.set_bitrate(6_000_000);
```

## What it does today

| Codec | Where | Chroma | Depth | Headers | Checked with |
|---|---|---|---|---|---|
| AV1 | VA-API, low-power encoder | 4:2:0 | 8, 10 bits | written here, from the AV1 specification 1.0.0 errata 1 | dav1d, libaom |
| HEVC | VA-API, low-power encoder | 4:2:0, 4:4:4 | 8, 10 bits | written here, from Rec. ITU-T H.265 (08/2021) | FFmpeg |
| VP9 | VA-API, low-power encoder | 4:2:0, 4:4:4 | 8 bits | the driver's | libvpx, FFmpeg |
| VP9 | CPU, libvpx (`CpuEncoder`) | 4:2:0, 4:4:4 | 8 bits | libvpx's | libvpx, FFmpeg |

On the GPU, checked on an Intel Arc A380 (iHD 25.2.6). A frame in a DMA-BUF (`Encoder::import`)
is read by the GPU where it is; a frame in memory is copied to GPU memory first. Either is
converted to YUV by the GPU's video processing (BT.601, limited range), then coded. On the
CPU, the conversion is the same, on several threads.

Measured at 3440x1440: HEVC 4:4:4 from memory, about 10 ms a frame on the A380, 2 of them the
copy; VP9 4:4:4 about 20 ms on the A380, 40 to 50 on the CPU (five or six cores).

## Not there yet

- NVENC, for NVIDIA GPUs.
- Recovery from a lost frame without a keyframe (long-term reference pictures).

## Building

The development files of libva (`libva-devel`, `libva-dev`) and libvpx (`libvpx-devel`,
`libvpx-dev`), and libclang, which generates the bindings to both from the installed headers.

```sh
cargo test        # the GPU's tests are skipped where no GPU encodes
BENCH_CODEC=av1 cargo run --release --example bench -- 1920 1080 300 420 8 10000 out.obu
```

The tests decode the streams with `ffmpeg`, when it is installed, and compare the pictures
with what went in, in YUV.

## Licence

MPL-2.0, as the rest of the project.
