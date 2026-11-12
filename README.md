# qemu-dbus-webrtc

A WebRTC console for QEMU virtual machines, running on the host. It attaches to a live VM
through QEMU's `-display dbus` interface and streams its display to a web browser or a native
client, with keyboard, mouse and sound, without any software in the guest; an optional guest
agent adds the clipboard, files, drives and printers.

Status: **early development**. It works day to day on its developers' setup (Windows 11 guest,
Intel Arc A380), but there is **no authentication yet**: anyone who reaches its address sees and
drives the VM. Keep it on localhost or a trusted network. Design discussions and contributions
are welcome, see [CONTRIBUTING.md](CONTRIBUTING.md).

## Features

Without anything in the guest:

- **Video:** the guest's screen, encoded on the host GPU with VA-API, in the codecs below; from
  the DMA-BUF QEMU shares when the VM has GL (no copy through the CPU), from memory otherwise
  (`gl=off`, virtio-vga 2D). One encoder per screen, shared by every viewer, at most 30 frames
  a second; nothing is encoded when nobody watches. In 10 bits (AV1, HEVC) for clients that
  say they decode it: no banding in gradients.
- **Without a GPU:** VP9 encoded on the CPU (`--software`, taken by itself when the host has no
  GPU encoder), in 4:2:0 or 4:4:4. `--codecs` lists what each GPU, and the CPU, encodes.
- **The network:** the bitrate follows the user's bandwidth, estimated with Google Congestion
  Control from the client's reports (between 4 and 30 Mbit/s, `--bitrate` for the ceiling);
  each client is told its estimate.
- **One user, watchers:** the guest's screen takes the size of the user's window. One client
  uses the session at a time, as with RDP: a new one takes it over. Shadow clients (`?shadow` in
  the page's address) only watch and listen.
- **Browsers:** Chrome and Firefox, with lost packets sent again (NACK/RTX). A client is told why
  when it cannot decode the stream (Firefox does not take VP9 in 4:4:4).
- **Keyboard and mouse,** as RDP handles them: keys by position (scancodes), the guest applying
  its own layout; lock keys synchronised; nothing left pressed when a client goes. The guest's
  pointer shape is drawn by the client, free of the video's latency.
- **Sound** (Opus) and the client's **microphone**.
- **Pause:** a hidden window gets no video, and costs no encoding; the sound goes on.
- **HTTPS** (`--tls`): browsers give the clipboard to pages served in HTTPS only.

| Codec | Option | Chroma | Royalties | Best for | Decoded by |
|---|---|---|---|---|---|
| AV1 | default | 4:2:0 | royalty-free | low bitrates: about a fifth fewer bits than VP9 4:4:4 for the same quality | Chrome, Firefox |
| VP9 | `--codec vp9 --chroma 444` | 4:4:4 | royalty-free | sharp coloured text and thin lines, when the bandwidth allows | Chrome (in software), the native client; not Firefox |
| VP9 | `--codec vp9` | 4:2:0 | royalty-free | the fallback | every current browser |
| HEVC | `--codec hevc` | 4:2:0 or 4:4:4 (in 4:4:4, the Arc A380 gets the colours of the top 8 rows wrong) | patent pools: off by default | deployments that license it | clients with an HEVC decoder (not tested in browsers yet) |

4:2:0 halves the resolution of the colours: black-on-white text stays sharp, coloured text and
thin coloured lines blur. Measurements and the patent question are in
[ARCHITECTURE.md](ARCHITECTURE.md#codecs).

With the optional Ameleo guest agent in a Windows guest:

- **Clipboard:** text and images, both ways.
- **Files and folders** copied and pasted both ways, with Explorer's progress dialog (native
  client).
- **Drives:** the client's folders or disks shown in the guest as drive letters, as RDP does
  (native client).
- **Printers:** the client's printers shown in the guest, with their driver's options and
  settings dialog; jobs print on the client (native client, Windows).
- **Cameras:** the client's cameras shown in the guest, with their controls (brightness,
  focus, zoom...); a camera streams, in H.264 as the client encodes it, only while a guest
  application uses it (browser and native client).

Clients: the page served by the program itself; a native client built on libwebrtc is in the
works. All messages are Protocol Buffers ([qdw.proto](crates/protocol/proto/qdw.proto)), so
other clients can be written from it.

Coming next: several monitors, automatic logon, and authentication.

## Running it

It needs GStreamer 1.26 (the `va` elements of gst-plugins-bad for a VA-API GPU, `vp9enc` for the
CPU, and `rtpgccbwe`, `rtpvp9pay2` of gst-plugins-rs) and a VM started with `-display dbus`
(libvirt: `<graphics type='dbus'/>`). Two fixes to GStreamer, to be offered upstream, make the
congestion control smoother: as released, the VA encoders send a keyframe at each change of
bitrate, and `rtpgccbwe` raises its estimate far above the link while the desktop sends little.
Build it with Cargo, then attach it to the VM, through its QMP socket (peer to peer) or the D-Bus
bus libvirt runs for it:

```
cargo build --release
target/release/qemu-dbus-webrtc --dbus unix:path=/run/libvirt/qemu/dbus/<id>-<vm>-dbus.sock \
    --listen 127.0.0.1:8090
```

and open `http://127.0.0.1:8090/`. Options: `--codec av1|vp9|hevc`, `--chroma 420|444`,
`--software`, `--bitrate <max kbit/s>`, `--listen <address:port>`,
`--tls <certificate.pem> <key.pem>`, `--record <path prefix>` (to keep each screen's stream in a
file). `qemu-dbus-webrtc --codecs` lists what the host encodes.

## Goals

- Hardware video encoding on the host (VA-API first: Intel and AMD GPUs, including integrated
  ones), so that one inexpensive GPU serves several consoles; the CPU as a fallback.
- Standard WebRTC in any modern browser: no plugin, no client to install.
- Independent of the VM manager: it attaches to a running VM (QMP and D-Bus) and does not manage
  its lifecycle, so it can serve Proxmox VE, libvirt or plain QEMU alike.

## Design

See [ARCHITECTURE.md](ARCHITECTURE.md): WebRTC through GStreamer `webrtcbin`, royalty-free AV1
by default and VP9 4:4:4 for sharp coloured text, and the device redirection formats of the Remote
Desktop Protocol carried over WebRTC data channels.

## License

Mozilla Public License 2.0, see [LICENSE](LICENSE). In short: you may use this code in any
project, open or closed; changes to the files of this project must be published under the same
license.
