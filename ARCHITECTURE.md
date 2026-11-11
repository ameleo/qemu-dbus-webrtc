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
- **Transport, per viewer:** each viewer has its own `webrtcbin`, fed with the encoded frames
  of the console it watches: one encoder per console, whatever the number of viewers. A viewer
  that loses a frame asks for a keyframe (PLI), which goes back to the encoder.
- **Signalling:** a WebSocket carrying the SDP offer and answer and ICE candidates, behind
  token authentication supplied by the deployment (a broker or the VM manager).
- **Protocol:** the messages between the server and its clients are Protocol Buffers, defined
  in `crates/protocol/proto/qdw.proto`. Clients in any language generate their code from it;
  the browser page carries a small hand-written codec for the few messages it needs. Device
  channels keep their RDP formats (see below).

## Source layout

| Directory | Crate | Role |
|---|---|---|
| `crates/protocol` | `qdw-protocol` | the `.proto` file and the Rust types generated from it |
| `crates/qemu` | `qdw-qemu` | attachment to QEMU: QMP, the D-Bus display and sound, the guest agent |
| `crates/video` | `qdw-video` | hardware encoding of a framebuffer, recording |
| `crates/audio` | `qdw-audio` | Opus encoding of the guest's sound |
| `server` | `qemu-dbus-webrtc` | the server: viewer sessions, signalling, the browser page |

A native client will sit next to them and share `qdw-protocol`.

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

Audio is Opus, 128 kbit/s in 10 ms frames, encoded once for every viewer. A silent source keeps
the stream going while the guest plays nothing: a WebRTC sender needs the audio to flow before
it can make its offer. The samples are stamped by their count, not on arrival, and each viewer
keeps the encoder's timestamps: they become the RTP timestamps, from which the receiver places
the samples. Browsers play sound only after the user did something on the page: the page
starts muted and unmutes on the first click or key.

The client's microphone comes back on the sound's track, which is two-way: the server decodes it
and feeds the guest's microphone through QEMU (`AudioInListener`). QEMU reads the samples from
its main loop and waits for the answer, so the server keeps them ready in the guest's format,
100 ms at most so that the delay cannot grow. One viewer speaks at a time: the one sending
sound, until it has been silent for a second. In the browser page, a button turns the
microphone on and off: browsers want a click and a permission.

## Devices: RDP channel formats over data channels

WebRTC carries video and audio natively, including the browser's camera and microphone
(`getUserMedia`). For everything else, the project reuses the device redirection formats of the
Remote Desktop Protocol, unchanged, each on its own reliable, ordered data channel. They are
specified, proven, and implemented on the client side by the IronRDP crates (MIT/Apache-2.0).
Only channels with no patent listed in Microsoft's Open Specifications patent map are used.

| Function | Format | Data channel | Browser | Native client (later) |
|---|---|---|---|---|
| Clipboard | project format, modelled on MS-RDPECLIP | `clipboard` | text, images (HTTPS) | full, files later |
| Drives | MS-RDPEFS | `rdpdr` | a chosen folder (Chromium) | full |
| Printers | MS-RDPEPC, MS-RDPEXPS | `rdpdr` | no | yes |
| Monitor layout, resize | MS-RDPEDISP | `disp` | yes | yes |
| Smart cards | MS-RDPESC | `rdpdr` | no | yes |
| USB | MS-RDPEUSB | `urbdrc` | no | yes |
| Keyboard, mouse | project format, modelled on RDP | `input` | yes | yes |
| Guest pointer shape | project format | `pointer` | yes | yes |

Keyboard and mouse follow what RDP does: keys travel as scancodes, by their position, and the
guest applies its own layout; the lock keys are synchronised when the client gets the focus;
everything pressed is released when it loses it, and by the server when a viewer goes; the
wheel counts in 1/120 of a notch. The guest pointer is not in the video (virtio-gpu draws it
on a plane of its own): its shape goes to the client, which draws it as its own cursor, free of
the video latency.

Touch input does not use MS-RDPEI, which has a listed patent.

The clipboard follows RDP's model: whoever copied last owns it and announces the types it holds,
and the data move only when someone pastes. In the guest, the Ameleo guest agent holds the
Windows clipboard. A browser page cannot watch the local clipboard: it reads it when it gets the
focus, and offers it to the guest if it changed; and browsers give the clipboard only to pages
served in HTTPS (`--tls`) or from localhost.

Only the channel crates of IronRDP are used: `ironrdp-rdpdr`,
`ironrdp-displaycontrol` (and `ironrdp-rdpeusb` later), with their base crates. They do no I/O:
bytes received on a data channel go in, the messages to send come out. The RDP server, connection
and graphics crates are not needed: WebRTC provides the transport, security and media.

In the guest, the optional Ameleo guest agent (D-Bus over a QEMU character device) provides what
QEMU alone cannot: the clipboard, automatic logon, printers, drives and cameras as native Windows
devices.

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
