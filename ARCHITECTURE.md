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
- **Video:** GStreamer, VA-API hardware encoding without copying the frame through the CPU.
- **Transport:** GStreamer `webrtcbin`, with congestion control (GCC) driving the encoder bitrate.
- **Signalling:** a WebSocket carrying the SDP offer and answer and ICE candidates, behind
  token authentication supplied by the deployment (a broker or the VM manager).

## Codecs

HEVC (H.265) is the default video codec, H.264 the fallback for browsers that do not decode HEVC
in WebRTC. AV1 is planned as a royalty-free option.

> **Patent royalties.** HEVC and H.264 are covered by patents licensed through patent pools.
> This project ships no encoder or decoder of its own: encoding uses the host GPU's hardware
> encoder through VA-API, and decoding happens in the user's browser or operating system.
> Whether your deployment needs a patent license depends on your use and jurisdiction; this is
> not legal advice.

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

In the guest, the optional Ameleo guest agent (D-Bus over a QEMU character device) provides what
QEMU alone cannot: automatic logon, printers, drives and cameras as native Windows devices.

## Milestones

Each milestone is validated on real hardware before the next one starts.

1. **Attach:** QMP and D-Bus connection to a running VM, scanouts received, resolution changes
   followed.
2. **Encode:** VA-API HEVC and H.264 without a copy; stream checked with a reference decoder,
   frame rate measured.
3. **Browser:** signalling and WebRTC video in Chrome and Firefox.
4. **Input:** keyboard and mouse over the `input` data channel.
5. **Audio:** Opus playback, then the microphone.
6. **Network and display:** congestion control, pause and resume, multi-monitor, resize,
   clipboard.
7. **Devices:** drives, then the guest agent.
8. **Deployment:** token authentication, packages, Proxmox VE integration.
