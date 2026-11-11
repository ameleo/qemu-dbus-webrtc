# qemu-dbus-webrtc

A WebRTC console for QEMU virtual machines, running on the host. It attaches to a live VM
through QEMU's `-display dbus` interface and streams its display to a web browser, with keyboard,
mouse and audio, without any software in the guest.

Status: **early development**, nothing usable yet. Design discussions and contributions are
welcome, see [CONTRIBUTING.md](CONTRIBUTING.md).

## Goals

- Hardware video encoding on the host (VA-API first: Intel and AMD GPUs, including integrated
  ones), so that one inexpensive GPU serves several consoles.
- Standard WebRTC in any modern browser: no plugin, no client to install.
- Independent of the VM manager: it attaches to a running VM (QMP and D-Bus) and does not manage
  its lifecycle, so it can serve Proxmox VE, libvirt or plain QEMU alike.

## Design

See [ARCHITECTURE.md](ARCHITECTURE.md): WebRTC through GStreamer `webrtcbin`, HEVC by default
(see the note on patent royalties there), and the device redirection formats of the Remote
Desktop Protocol carried over WebRTC data channels.

## License

Mozilla Public License 2.0, see [LICENSE](LICENSE). In short: you may use this code in any
project, open or closed; changes to the files of this project must be published under the same
license.
