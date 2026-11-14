# qemu-dbus-webrtc

A WebRTC console for QEMU virtual machines, running on the host. It attaches to a live VM
through QEMU's `-display dbus` interface and streams its display to a web browser or a native
client, with keyboard, mouse and sound, without any software in the guest; an optional guest
agent adds the clipboard, files, drives and printers.

Status: **early development**. It works day to day on its developers' setup (Windows 11 guest,
Intel Arc A380). The consoles take the server's user and password; single sign-on and the
automatic logon to Windows are to come. Design discussions and contributions are welcome, see
[CONTRIBUTING.md](CONTRIBUTING.md).

## Features

Without anything in the guest:

- **Video:** the guest's screen, encoded on the host GPU straight through VA-API, with no
  media framework, in the codecs below; from the DMA-BUF QEMU shares when the VM has GL, which
  the GPU reads where it is, from memory otherwise (`gl=off`, virtio-vga 2D). One encoder per
  screen, shared by every viewer, up to 60 frames a second, as fast as the guest draws; a
  keyframe or a change of bitrate takes effect with the next frame. Nothing is encoded when
  nobody watches. In 10 bits (AV1, HEVC) for clients that
  say they decode it: no banding in gradients.
- **Without a GPU:** VP9 encoded on the CPU with libvpx (`--software`, taken by itself when the
  host has no GPU encoder), in 4:2:0 or 4:4:4. `--codecs` lists what each GPU, and the CPU, encodes.
- **The network:** the bitrate follows the user's bandwidth, estimated with Google Congestion
  Control from the client's reports (between 4 and 30 Mbit/s, `--bitrate` for the ceiling);
  each client is told its estimate.
- **Several monitors,** as with RDP (native client): one guest screen per client monitor, at its
  size and place, so the pointer and windows go from one screen to the next as on the client's
  own desktop. Four screens at most, each a video track of its own; the bitrate is shared among
  them. The browser shows the first screen.
- **One user, watchers:** the guest's screen takes the size of the user's window. One client
  uses the session at a time, as with RDP: a new one takes it over. Shadow clients (`?shadow` in
  the page's address) only watch and listen.
- **Browsers:** Chrome and Firefox, with lost packets sent again (NACK/RTX). A client is told why
  when it cannot decode the stream (Firefox does not take VP9 in 4:4:4).
- **Keyboard and mouse,** as RDP handles them: keys by position (scancodes), the guest applying
  its own layout; lock keys synchronised; nothing left pressed when a client goes. The guest's
  pointer shape is drawn by the client, free of the video's latency.
- **Sound** (Opus) and the client's **microphone**, open only while an application in the guest
  records.
- **Pause:** a hidden window gets no video, and costs no encoding; the sound goes on.
- **HTTPS only:** the page and its signalling are never served in plain HTTP, which would carry
  the session in clear; browsers also give the clipboard, the microphone and the camera to pages
  served in HTTPS only.

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

Coming next: automatic logon to the guest, and single sign-on (OpenBao, OIDC).

## Running it

It needs libva with the GPU's VA driver (Intel's `iHD`, or Mesa's for AMD) and libvpx for the
video, libopus for the sound; GStreamer 1.26 for the transport (`webrtcbin`, and `rtpgccbwe`,
`rtpvp9pay2` of gst-plugins-rs; it also decodes the microphone's Opus for now);
libvirt-dbus for libvirt's VMs; and VMs started with `-display dbus` (libvirt:
`<graphics type='dbus'/>`). Building it also needs the development files of libva, libvpx and
libopus, and libclang, which generates the bindings to them. A fix to
GStreamer, to be offered upstream, makes the congestion control smoother: as released,
`rtpgccbwe` raises its estimate far above the link while the desktop sends little.
One server serves every VM of its host. It finds them through their VM manager: libvirt (the
domains with `<graphics type='dbus'/>`, through libvirt-dbus, which also lets it start a VM
that is off), Proxmox VE (not tried yet), or VMs given by hand. Build it with Cargo and start
it:

```
cargo build --release
target/release/qemu-dbus-webrtc --listen 0.0.0.0:8090 --tls cert.pem key.pem
```

A VM's page is `https://<host>:8090/vm/<uuid>/`, or its name in place of the UUID (a libvirt
domain, a Proxmox VM id or name). Which VMs the host runs is said only to a client that logged
in. The server attaches to a VM when its first
client comes, and detaches when the last one left (a minute after a client cut off, which may
come back). It serves in HTTPS only: it needs a certificate and its key. `[acme]` gets them
from an ACME authority, public or the organization's own (External Account Binding included),
and renews them; for a test, a self-signed one does,
`openssl req -x509 -newkey rsa:2048 -nodes -days 365 -subj /CN=localhost -keyout key.pem -out cert.pem`
(the browser asks to accept it once).

The consoles take the server's user and password, asked by the page, and checked before the
server starts a VM or attaches to it: `[auth] user` and `password` in the configuration, the
password as an Argon2id hash, never in clear, which `qemu-dbus-webrtc hash-password` makes.
They open the server's door; the guest's own logon still guards the guest.

The settings can live in a TOML file, one per host, `qemu-dbus-webrtc --config <file>`:
[config.example.toml](config.example.toml) describes them all. The command line's options
override the file's: `--codec av1|vp9|hevc`, `--chroma 420|444`, `--software`,
`--bitrate <max kbit/s>`, `--listen <address:port>`, `--tls <certificate.pem> <key.pem>`
(required, unless in the file), `--record <path prefix>` (to keep each screen's stream in a
file), and VMs by hand: `--domain <libvirt domain>`, `--dbus <address>`, `--qmp <socket>`.
`qemu-dbus-webrtc --codecs` lists what the host encodes.

## Goals

- Hardware video encoding on the host (VA-API first: Intel and AMD GPUs, including integrated
  ones), so that one inexpensive GPU serves several consoles; the CPU as a fallback.
- Standard WebRTC in any modern browser: no plugin, no client to install.
- Independent of the VM manager: it attaches to a running VM (QMP and D-Bus) and does not manage
  its lifecycle, so it can serve Proxmox VE, libvirt or plain QEMU alike.

## Design

See [ARCHITECTURE.md](ARCHITECTURE.md): video encoded straight through VA-API (or libvpx on the
CPU), WebRTC through GStreamer `webrtcbin`, royalty-free AV1 by default and VP9 4:4:4 for sharp
coloured text, and the device redirection formats of the Remote
Desktop Protocol carried over WebRTC data channels.

## License

Mozilla Public License 2.0, see [LICENSE](LICENSE). In short: you may use this code in any
project, open or closed; changes to the files of this project must be published under the same
license.
