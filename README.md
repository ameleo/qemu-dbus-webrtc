# qemu-dbus-webrtc

A WebRTC remote console for QEMU virtual machines. It runs on the host, attaches to running VMs
through QEMU's `-display dbus` interface and streams the display, keyboard, mouse and audio to a
web browser or a native client. No guest software is required; an optional guest agent adds
clipboard, file transfer, drive, printer and camera redirection.

| Windows 11 | Fedora 44, KDE Plasma |
|---|---|
| [![A Windows 11 VM in Chrome (video)](https://i.ytimg.com/vi/KdQXhpSnNgw/mqdefault.jpg)](https://youtu.be/KdQXhpSnNgw) | [![A Fedora 44 VM with KDE Plasma in Chrome (video)](https://i.ytimg.com/vi/ol17aShZBco/mqdefault.jpg)](https://youtu.be/ol17aShZBco) |
| *A Windows desktop in Chrome, recorded at 60 fps (YouTube).* | *A Linux desktop in Chrome, recorded at 60 fps (YouTube).* |

**Status: early development.** Tested with Windows 11 and Fedora guests on an Intel Arc A380.
Clients authenticate with an account of the server, which names the VMs it may open; tokens
from a connection broker, client certificates and FIDO security keys are planned. The guest
keeps its own logon.

## Why

I wanted a console that shows the whole life of a virtual machine, from power-on: the
firmware, the boot menu, the "Working on updates" screens. Remote desktops only connect once
the guest OS runs, so they cannot tell whether a VM is stuck or installing an update.

Recent changes in VMware's licensing pushed many users towards Proxmox VE, libvirt or plain
QEMU. They find everything there but a console suited to long-distance use: VNC sends the screen
as compressed rectangles of pixels, with no video codec, which takes a lot of bandwidth for video
and moving windows. SPICE needs a client to install, and Red Hat Enterprise Linux has dropped it.

I develop this project in my spare time, with the help of AI coding assistants; commits say so
with an `Assisted-by` trailer. I decide what goes in, test every change on real hardware, and
answer for it, but I do not master every line: reviews from experienced developers are
especially welcome. The project is young: it runs day to day on my setup, but is not yet
recommended for production. To give feedback or contribute, see
[CONTRIBUTING.md](CONTRIBUTING.md).

## Features

Without guest software:

- **Hardware video encoding** through VA-API, without a media framework, in AV1, HEVC or VP9.
  Zero-copy from the DMA-BUF QEMU exports when the VM uses GL; from shared memory otherwise
  (`gl=off`, 2D virtio-vga). One encoder per screen, shared by all viewers, up to 60 fps,
  driven by guest updates. Keyframe requests and bitrate changes apply on the next frame.
  Nothing is encoded without viewers, nor for screens the client does not display. 10-bit
  encoding (AV1, HEVC) for clients that support it, avoiding banding in gradients.
- **Software fallback:** VP9 with libvpx, in 4:2:0 or 4:4:4 (`--software`, selected
  automatically when the host has no hardware encoder). `--codecs` lists the codecs each GPU,
  and the CPU, can encode.
- **Congestion control:** the bitrate follows a Google Congestion Control (GCC) bandwidth
  estimate from the client's transport-wide feedback, between 4 and 30 Mbit/s (`--bitrate` sets
  the maximum). On a constrained link, frames are dropped rather than queued.
- **Through NATs and firewalls:** ICE, with a STUN server (Google's by default) so that clients
  outside the host's network connect without any port to forward, and TURN relays (`[[turn]]`:
  coturn or a TURN service) for the networks no direct path gets through, such as an office that
  lets only TCP or TLS to port 443 out. Relay credentials are computed for each session from a
  secret shared with the relay; the user never sees them. See [Usage](#usage).
- **Multi-monitor:** one guest screen per client monitor, with the client's layout, up to four
  screens, each on its own video track. The bitrate is shared between them. The browser client
  does it in Chrome and Edge (the Window Management API): its "All monitors" button shows the
  first screen full screen on the page's monitor and opens a window on each other monitor, full
  screen after a click in it, or at once for the sites Chrome's `AutomaticFullscreenAllowedForUrls`
  policy names. Other browsers display the first screen.
- **Session handling:** the guest resolution follows the client window. One client controls
  the session at a time; a new client takes it over. Shadow clients (`?shadow` in the URL) are
  view-only.
- **Browsers:** Chrome and Firefox, with packet loss recovery (NACK/RTX). Clients are told when
  they cannot decode the stream (Firefox does not support VP9 4:4:4).
- **Input:** keyboard by scancode, so the guest applies its own layout; lock key state
  synchronization; all keys released when a client disconnects. The cursor is rendered by the
  client, independently of video latency.
- **Audio** (Opus), and microphone input while a guest application records.
- **Pause:** a hidden client window receives no video and costs no encoding; audio continues.
- **HTTPS only:** the client page and signaling are never served over plain HTTP. Browsers also
  require HTTPS for clipboard, microphone and camera access.

| Codec | Option | Chroma | Royalties | Best for | Decoders |
|---|---|---|---|---|---|
| AV1 | default | 4:2:0 | royalty-free | low bitrates: about 20 % fewer bits than VP9 4:4:4 at equal quality | Chrome, Firefox |
| VP9 | `--codec vp9 --chroma 444` | 4:4:4 | royalty-free | sharp colored text and thin lines, given enough bandwidth | Chrome (software decoding), the native client; not Firefox |
| VP9 | `--codec vp9` | 4:2:0 | royalty-free | fallback | all current browsers |
| HEVC | `--codec hevc` | 4:2:0 or 4:4:4 (in 4:4:4, the Arc A380 corrupts the colors of the top 8 rows) | patent pools; disabled by default | licensed deployments | clients with an HEVC decoder (not yet tested in browsers) |

4:2:0 subsampling halves the chroma resolution: black-on-white text stays sharp, but colored
text and thin colored lines blur. See [ARCHITECTURE.md](ARCHITECTURE.md#codecs) for
measurements and the patent situation.

With a guest agent implementing the
[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol) D-Bus interface:

- **Clipboard:** text and images, in both directions.
- **File copy and paste** in both directions, with Explorer's progress dialog (native client).
- **Drive redirection:** client folders or disks as guest drive letters, as in RDP (native
  client).
- **Printer redirection:** client printers in the guest, with their driver's options and
  settings dialog; jobs print on the client (native client on Windows).
- **Camera redirection:** client cameras in the guest, with their controls (brightness, focus,
  zoom...). A camera streams, as H.264 encoded by the client, only while a guest application
  uses it (browser and native client).

Agents, both under the GPL version 3:
[ameleo-display-agent-linux](https://github.com/ameleo/ameleo-display-agent-linux) for Linux
guests, and [ameleo-display-agent-win](https://github.com/ameleo/ameleo-display-agent-win) for
Windows guests, where drives, printers and file copy and paste need a companion service (see its
README).

Clients: the web page served by the server, and a native client based on libwebrtc (in
development). The web page speaks English or French, as the browser does; its tools are in a bar
that comes down from the top (microphone, camera, all monitors, full screen, Ctrl+Alt+Del,
connection details, disconnect), files and folders dropped on it go to the guest's clipboard for
a paste there, and it connects again by itself after a lost connection. The client protocol is defined in Protocol Buffers
([qdw.proto](crates/protocol/proto/qdw.proto)), so other clients can be implemented from it.

## Usage

Runtime dependencies: libva with the GPU's VA driver (Intel `iHD` or Mesa for AMD), libvpx,
libopus, and libvirt-dbus for libvirt VMs. VMs must use `-display dbus` (libvirt:
`<graphics type='dbus'/>`). WebRTC is implemented in Rust (str0m), without a media framework.
[packaging/qemu](packaging/qemu) has the QEMU patches the author's hosts run with.
Building also requires the libva, libvpx and libopus development files, and libclang for the
generated bindings.

One server handles all VMs of its host. It discovers them through libvirt (domains with
`<graphics type='dbus'/>`, through libvirt-dbus, which can also start a stopped VM), Proxmox VE
(untested), or a static list. Build and start it with:

```
cargo build --release
target/release/qemu-dbus-webrtc --listen 0.0.0.0:8090 --tls cert.pem key.pem
```

A VM's console is at `https://<host>:8090/vm/<uuid>/`; the UUID can be replaced by the VM's
name (libvirt domain, or Proxmox VM ID or name). The list of VMs is only shown to authenticated
clients. The server attaches to a VM when its first client connects and detaches one minute
after the last one leaves, so that a client can reconnect.

The server only serves HTTPS and needs a certificate and its private key. `[acme]` obtains and
renews them from an ACME CA, public or private (External Account Binding supported). For
testing, a self-signed certificate works:
`openssl req -x509 -newkey rsa:2048 -nodes -days 365 -subj /CN=localhost -keyout key.pem -out cert.pem`

Clients authenticate with an account before the server starts or attaches to a VM. Each
`[[account]]` in the configuration has a user, a password stored as an Argon2id hash
(`qemu-dbus-webrtc hash-password`), and the VMs it may open, by UUID or name, or `"*"` for all of
them; `[auth] user` and `password` give one more account, for all the VMs. A VM that no account
names is open to none, and to a client that asks for a VM its account does not name, the server
answers as for a VM the host does not run. This protects access to the server; the guest still
requires its own logon. For many users, a connection broker that hands out tokens naming the VM
is planned: the accounts suit a single host.

Clients outside the host's network reach it through the router, which forwards the HTTPS port
(TCP) to the host. For the media, where STUN does not do (below), forward a range of UDP ports
too, keeping their numbers, and set them in `[listen]`: `udp_ports = "40000-40099"` and
`public_address`, the router's public address or a name for it. The sessions then take their
UDP ports in that range and announce the public address on them.

Most often, no port needs forwarding: each session asks a STUN server for the
public address and port its router gives it, the clients ask it for theirs, and both sides
open the way through their routers by sending to each other. The STUN server is Google's by
default (`stun` in `[listen]`; `""` for none, as the server and its clients then reveal their
addresses to no third party).

For the networks neither way gets through, such as an office that lets only TCP or TLS to port
443 out, the clients can relay the media through a TURN server (`[[turn]]`). The server runs no
relay itself: use coturn, or a TURN service. One relay serves all the hosts, which name it in
their settings. The media stay encrypted end to end, so the relay cannot read them, but they all
go through it: place it near the hosts, with the bandwidth of the sessions it relays. Clients
use it only when no direct path works; `?relay` in the page's address forces it, to check it.
With a secret shared with the relay (coturn's `use-auth-secret` and `static-auth-secret`),
each session gets its own credentials, valid for a day, and the secret stays on the hosts:

```toml
[[turn]]
urls = ["turns:turn.example.com:443?transport=tcp", "turn:turn.example.com:3478"]
secret = "the static-auth-secret of coturn"
```

Credentials let a client relay to any address the relay allows, for a day: restrict the relay to
the hosts, or a signed-in user could reach the rest of the network through it. With coturn:

```
use-auth-secret
static-auth-secret=<the same secret>
realm=vdi.example.com
# Relay only to the hosts (here 192.168.1.0/24), nowhere else.
denied-peer-ip=0.0.0.0-255.255.255.255
denied-peer-ip=::-ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff
allowed-peer-ip=192.168.1.0-192.168.1.255
no-multicast-peers
no-cli
```

The hosts can then stay on a private network for the media: only the relay, and the page and
signaling (through a reverse proxy), need to be reachable from the internet. With `stun = ""` on
the hosts, outside clients always go through the relay.

Behind a connection broker (`[broker]`), the host connects to the broker over gRPC in TLS and
keeps the connection: it announces its VMs, and serves the sessions the broker relays to it,
the broker having signed the user in and checked their access. The host then needs no inbound
port at all: without accounts or a certificate of its own, it serves no page itself.

Settings can be stored in a TOML file, one per host (`qemu-dbus-webrtc --config <file>`); see
[config.example.toml](config.example.toml). Command-line options override the file:
`--codec av1|vp9|hevc`, `--chroma 420|444`, `--software`, `--bitrate <max kbit/s>`,
`--listen <address:port>`, `--tls <certificate.pem> <key.pem>` (required unless set in the
file), `--record <path prefix>` (record each screen's stream to a file), and static VMs:
`--domain <libvirt domain>`, `--dbus <address>`, `--qmp <socket>`. `qemu-dbus-webrtc --codecs`
lists the host's encoders.

### As a systemd service (Enterprise Linux 10)

Each release provides an RPM for Enterprise Linux 10 (Rocky, Alma, RHEL) on the
[releases page](https://github.com/ameleo/qemu-dbus-webrtc/releases). It installs the
`qemu-dbus-webrtc` service, which runs as a dedicated `qemu-dbus-webrtc` user in the groups
`qemu` (VM QMP sockets and display buses), `libvirt` (libvirt-dbus) and `render` (GPU), with
systemd sandboxing. The configuration file is `/etc/qemu-dbus-webrtc/config.toml`. libvirt VMs
need no further setup; for VMs reached through their QMP socket (Proxmox VE, `[[vm]] qmp`), the
socket must be writable by the service user, as `connect()` requires write access.

```
dnf install ./qemu-dbus-webrtc-<version>-1.el10.x86_64.rpm
install -m 0600 cert.pem key.pem /etc/qemu-dbus-webrtc/tls/
systemctl enable --now qemu-dbus-webrtc
```

The certificate and private key stay readable by root only; systemd passes them to the service
(`LoadCredential=`). A renewed certificate is picked up on `systemctl restart qemu-dbus-webrtc`.
With a TPM 2.0, the key can be sealed to the machine, so that a copy of the disk or a backup
does not expose it:

```
systemd-creds encrypt --with-key=tpm2 --name=key.pem /etc/qemu-dbus-webrtc/tls/key.pem /etc/qemu-dbus-webrtc/tls/key.pem.cred
shred -u /etc/qemu-dbus-webrtc/tls/key.pem
systemctl edit qemu-dbus-webrtc
```

with this drop-in:

```
[Service]
LoadCredential=
LoadCredential=cert.pem:/etc/qemu-dbus-webrtc/tls/cert.pem
LoadCredentialEncrypted=key.pem:/etc/qemu-dbus-webrtc/tls/key.pem.cred
```

With `[acme]`, the service writes the certificate itself and answers HTTP-01 challenges on port
80. Replace the credentials with write access to those directories and the port capability:

```
[Service]
LoadCredential=
ExecStart=
ExecStart=/usr/bin/qemu-dbus-webrtc --config /etc/qemu-dbus-webrtc/config.toml
ReadWritePaths=/etc/qemu-dbus-webrtc/tls /etc/qemu-dbus-webrtc/acme
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
```

Both directories must be owned by `qemu-dbus-webrtc`.

## Goals

- Hardware video encoding on the host (VA-API first: Intel and AMD GPUs, integrated ones
  included), so that one inexpensive GPU serves several consoles, with a CPU fallback.
- Standard WebRTC in any modern browser: no plugin, no client installation required.
- Independence from the VM manager: the server attaches to running VMs (QMP and D-Bus) and does
  not manage their lifecycle, so it works with Proxmox VE, libvirt or plain QEMU.

## Design

See [ARCHITECTURE.md](ARCHITECTURE.md): video encoding through VA-API (or libvpx on the CPU),
WebRTC with str0m, royalty-free AV1 by default and VP9 4:4:4 for sharp colored text, and the
Remote Desktop Protocol's device redirection formats carried over WebRTC data channels.

## License

Mozilla Public License 2.0, see [LICENSE](LICENSE). In short: you may use this code in any
project, open or proprietary; modifications to this project's files must be published under
the same license. The QEMU patches in [packaging/qemu](packaging/qemu) are under QEMU's license,
GPL-2.0-or-later.
