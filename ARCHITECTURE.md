# Architecture

qemu-dbus-webrtc runs on the host, next to QEMU. It attaches to a running VM through QMP and
QEMU's D-Bus display (`-display dbus`), encodes the guest display on the host GPU and serves it
to a browser over WebRTC. It does not manage the VM's lifecycle: the VM manager (Proxmox VE,
libvirt, plain QEMU) starts and stops it.

```
 browser ── WebRTC (video, audio, data channels) ── qemu-dbus-webrtc ── D-Bus / QMP ── QEMU
                                                          │
                                                          └── VA-API (host GPU encoder)
```

## Components

- **QEMU attachment:** QMP to find and follow the VM; the D-Bus display for scanouts (DMA-BUF,
  `ScanoutDMABUF2`), cursor, keyboard, mouse, audio and clipboard.
- **Video:** GStreamer, VA-API hardware encoding. The guest framebuffer is a linear DMA-BUF
  that VA-API cannot import as RGB, so `vapostproc` uploads it (one CPU copy) and converts it to
  YUV on the GPU, where the encoder reads it.
- **Transport:** GStreamer `webrtcbin`, with congestion control (GCC) driving the encoder bitrate.
- **Signalling:** a WebSocket carrying the SDP offer and answer and ICE candidates, behind
  token authentication supplied by the deployment (a broker or the VM manager).

## Source layout

| Directory | Crate | Role |
|---|---|---|
| `crates/qemu` | `qdw-qemu` | attachment to QEMU: QMP, the D-Bus display |
| `crates/video` | `qdw-video` | hardware encoding of a framebuffer, recording |
| `server` | `qemu-dbus-webrtc` | the server |

A native client will sit next to them.

## Clients

Two clients, one protocol: the browser, and a native client built on libwebrtc, the WebRTC
engine of Chromium and Safari. Both then share the same transport behaviour (feedback for
congestion control, retransmission, jitter buffer, SCTP data channels), so a problem seen in one
is seen in the other. Bitrate estimation runs on the sending side, in this server, whatever the
client.

What differs is decoding: the native client brings its own decoders and can take what browsers
may not (VP9 4:4:4). The codec is negotiated in SDP, from the best the client offers down to
VP9 4:2:0, which every current browser decodes. H.264 is left out: the project targets current
hardware, and the royalty-free codecs cover it.

## Codecs

| Codec | Chroma | Royalties | Use |
|---|---|---|---|
| AV1 | 4:2:0 | royalty-free licence | default: the most efficient at low bitrates |
| VP9 | 4:4:4 (profile 1) or 4:2:0 (profile 0) | royalty-free licence | 4:4:4 as the text quality mode, when the bandwidth allows; 4:2:0 as the fallback every browser decodes |
| HEVC (H.265) | 4:4:4 or 4:2:0 | patent pools | optional, off by default |

4:2:0 keeps the luma, and so black-on-white text, at full resolution, but halves the chroma
resolution: coloured text and thin coloured lines blur. Measured on an Intel Arc A380 with a
scrolling code editor at fixed quantizers, the chroma of every 4:2:0 codec stops improving at
about 43 dB however high the bitrate, while 4:4:4 keeps climbing; at low bitrates, AV1 4:2:0
needs about a fifth fewer bits than VP9 4:4:4 for the same RGB quality. Hence AV1 when the
bandwidth is short, VP9 4:4:4 above. Full chroma comes from the codec's own 4:4:4 profile,
encoded in hardware: Intel GPUs encode VP9 profile 1, but AV1 in 4:2:0 only.

The VA encoders do not write the colour space into the stream, and decoders then assume BT.601,
so the conversion from the guest's RGB uses BT.601.

> **Patent royalties.** AV1 and VP9 come with royalty-free patent licences (from the Alliance
> for Open Media and Google); third-party pools also claim patents on them, aimed mainly at
> device makers. HEVC is covered by patents licensed through patent pools, whose licensing can
> apply to services that stream it. HEVC is therefore off by default,
> and turning it on is the deployer's decision. This project ships no encoder or decoder of its
> own: encoding uses the host GPU's hardware encoder through VA-API, and decoding happens in the
> user's browser or operating system. Whether your deployment needs a patent license depends on
> your use and jurisdiction; this is not legal advice.

Audio is Opus.

## Devices: RDP channel formats over data channels

WebRTC carries video and audio natively, including the browser's camera and microphone
(`getUserMedia`). For everything else, the project reuses the device redirection formats of the
Remote Desktop Protocol, unchanged, each on its own reliable, ordered data channel. They are
specified, proven, and implemented on the client side by the IronRDP crates (MIT/Apache-2.0).
Only channels with no patent listed in Microsoft's Open Specifications patent map are used.

| Function | Format | Data channel | Browser | Native client (later) |
|---|---|---|---|---|
| Clipboard | MS-RDPECLIP | `cliprdr` | text, images | full, files included |
| Drives | MS-RDPEFS | `rdpdr` | a chosen folder (Chromium) | full |
| Printers | MS-RDPEPC, MS-RDPEXPS | `rdpdr` | no | yes |
| Monitor layout, resize | MS-RDPEDISP | `disp` | yes | yes |
| Smart cards | MS-RDPESC | `rdpdr` | no | yes |
| USB | MS-RDPEUSB | `urbdrc` | no | yes |
| Keyboard, mouse | project format | `input` | yes | yes |

Touch input does not use MS-RDPEI, which has a listed patent.

Only the channel crates of IronRDP are used: `ironrdp-cliprdr`, `ironrdp-rdpdr`,
`ironrdp-displaycontrol` (and `ironrdp-rdpeusb` later), with their base crates. They do no I/O:
bytes received on a data channel go in, the messages to send come out. The RDP server, connection
and graphics crates are not needed: WebRTC provides the transport, security and media.

In the guest, the optional Ameleo guest agent (D-Bus over a QEMU character device) provides what
QEMU alone cannot: automatic logon, printers, drives and cameras as native Windows devices.

## Milestones

Each milestone is validated on real hardware before the next one starts.

1. **Attach:** QMP and D-Bus connection to a running VM, scanouts received, resolution changes
   followed.
2. **Encode:** VA-API AV1, VP9 (4:4:4 and 4:2:0) and HEVC; streams checked with a reference
   decoder, frame rate measured.
3. **Browser:** signalling and WebRTC video in Chrome and Firefox.
4. **Input:** keyboard and mouse over the `input` data channel.
5. **Audio:** Opus playback, then the microphone.
6. **Network and display:** congestion control, pause and resume, multi-monitor, resize,
   clipboard.
7. **Devices:** drives, then the guest agent.
8. **Deployment:** token authentication, packages, Proxmox VE integration.
