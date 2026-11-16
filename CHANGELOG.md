# Changelog

What each release brings. The packages are on the
[releases page](https://github.com/ameleo/qemu-dbus-webrtc/releases).

## 0.2.0 — 2026-11-16

A web page for daily use, accounts per VM, clients outside the host's network, and guests on a
plain virtio-vga.

### Added

- **A web page for daily use.** A toolbar comes down from a tab at the top: the VM's name, a
  quality dot and the latency, then the microphone, the camera, all monitors, full screen,
  Ctrl+Alt+Del, the connection details and disconnecting. Cards over the screen for connecting,
  signing in, and the end of a session. The page connects again by itself after a lost
  connection (up to five tries), and speaks English or French, as the browser does.
- **Connection details a user understands:** the quality and what holds it back (data lost on
  the network, a slow or unsteady network, slow decoding on this computer, a busy server, low
  bandwidth), the responsiveness, the picture, the bitrate, the codec and whether the graphics
  card decodes it, the sound. The figures stay under "Technical details", folded.
- **All monitors in the browser** (Chrome and Edge, the Window Management API): one guest
  screen on each monitor, at its size and place; the other windows go full screen on a click,
  or at once for the sites Chrome's `AutomaticFullscreenAllowedForUrls` policy names. A guest
  window dragged from one monitor to the next follows the pointer.
- **Keys in full screen:** every key goes to the guest, the Windows key and Alt+Tab included
  (Keyboard Lock API); holding Esc leaves. In a window, the Windows keys stay local.
- **Files dropped on the page** go to the guest clipboard, for a paste there.
- **Accounts per VM:** `[[account]]` entries, each with its user, its password's Argon2id hash
  and the VMs it may open, by UUID or name, or `"*"`. A VM that no account names is open to
  none, and to a user it does not name the server answers as for a VM the host does not run.
  `[auth] user` and `password` stay an account for every VM.
- **Clients outside the host's network.** Each session asks a STUN server for the public
  address its router gives it, and gives the page the same server: both sides open the way
  through most routers, with no port to forward. Google's server by default; `[listen] stun`
  names another, `""` none. Where STUN does not get through, `udp_ports` and `public_address`
  for forwarded ports. TURN is not supported yet.
- **Plain virtio-vga with blob scanouts:** a guest's framebuffer, a udmabuf given without a
  modifier, is imported by the GPU as it is, and its import kept for when the guest flips back
  to it. For a Windows guest playing a video, the server's share of a CPU went from 18 % to 9 %
  (i915 no longer empties every CPU's caches at each flip). With QEMU 11.1, this mode needs
  QEMU's commit 5378d1da44 ("ui/dbus: disable scanout iff dmabuf is current"), in QEMU's
  master: without it, the picture freezes.

### Fixed

- **OpenGL framebuffers upside down.** A `virtio-vga-gl` guest without blob scanouts (Windows
  and Linux boot screens included) showed upside down: QEMU's `y0_top` flag, which says the rows
  go from the bottom despite its name, was not read. The GPU now turns such a picture over in
  the pass that converts it.
- **A screen layout the guest missed was not asked again** after a screen went off: with a
  Fedora guest, two screens asked for could show one.

## 0.1.3 — 2026-11-15

A clipboard fix, and the guest agents published.

### Fixed

- **Locked clipboard files stay readable after a new guest copy.** When the guest pasted files
  copied on a client, and copied something else before the paste ended, its reads of the locked
  files failed: the server sent them to the current clipboard owner, and there was none. As
  MS-RDPECLIP 2.2.4.1 requires, the data the guest locks now stays readable from the viewer it
  was locked on until it is unlocked, whatever the guest copies in the meantime.

### Changed

- The README says why the project exists, and links to the guest agents,
  [ameleo-display-agent-linux](https://github.com/ameleo/ameleo-display-agent-linux) and
  [ameleo-display-agent-win](https://github.com/ameleo/ameleo-display-agent-win), and to the
  D-Bus contract they implement,
  [ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol).

## 0.1.2 — 2026-11-14

Variable bitrate encoding, which fixes the frame rate collapsing to 4 fps after a still screen,
most visible with Linux guests.

### Fixed

- **Frame rate collapse after a still screen.** Intel's constant bitrate mode pads every frame
  up to bitrate / frame rate. On a still screen the measured frame rate drops to the still-frame
  rate (4 fps), so the encoder sent up to 25 Mbit/s of filler data, then oversized frames once
  motion resumed, which the frame dropper skipped, keeping the stream at 4 fps. GPU encoding now
  uses variable bitrate (VBR), which adds no filler, and dropped frames count in the measured
  frame rate. Measured on an Arc A380, HEVC 4:4:4 at 3440×1440: 0.5 Mbit/s on a still screen
  instead of 25, and 47–53 fps on a simulated 10 Mbit/s WAN link with a GNOME guest.

### Changed

- The guest agent's D-Bus contract now comes from
  [ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol), shared with the
  agent, instead of a copy in the server. Nothing changes on the wire.
- Identical scanouts (sent by virgl with every frame) are logged at debug level; only changes
  are logged at info level.
- The server logs its version and git commit at startup.
- Documentation rewritten in conventional technical English; automatic Windows logon dropped from
  the roadmap in favor of client certificates and FIDO security keys.

## 0.1.1 — 2026-11-14

Behaviour on a narrow or congested link. Measured on a simulated link of 10 Mbit/s with 30 ms of
delay and 0.5 % of losses, HEVC 4:4:4 at 3440×1440: the round trip went from 854 ms to 73 ms, the
client's sound buffer from 1.2 s to 113 ms, and the video from frozen to 18 frames a second with
no keyframe asked.

### Fixed

- **The sender's queue no longer fills on a narrow link.** At its coarsest, an encoder may still
  produce more than its target (HEVC 4:4:4 at 3440×1440 stays above about 6 Mbit/s at 24 frames
  a second); what the link could not take waited up to 1.7 s, delaying the picture and the
  sound. Frames are now dropped while the encoder is ahead of its bitrate, as libwebrtc's frame
  dropper does: fewer frames, but recent ones.
- **The video leaves room on the link:** it aims at 85 % of what the bandwidth estimate leaves
  after the sound, for the encoders' overshoot and the retransmissions.
- **A bandwidth estimate is never lost.** One that came within 500 ms of the last bitrate change
  was dropped, and a steady estimate is not sent again: the video could stay at its starting
  bitrate (10 Mbit/s on a 30 Mbit/s link), or above a link that had narrowed.

### Added

- The rates report gives the bandwidth estimate, the bitrate asked of each screen beside what
  it produced, and the frames dropped, every 5 s.

## 0.1.0 — 2026-11-14

The first release: the console of QEMU virtual machines over WebRTC, from QEMU's D-Bus display,
for Enterprise Linux 10 as an RPM and a systemd service.

- **Video encoded on the host's GPU** straight through VA-API, without a media framework: AV1
  (the default), HEVC and VP9, in 4:2:0 or 4:4:4, in 10 bits for AV1 and HEVC where the clients
  decode it; or VP9 on the CPU with libvpx. Up to 60 frames a second per screen, from the
  DMA-BUF QEMU shares when the VM has GL. A keyframe or a new bitrate takes effect with the next
  frame. The time a frame takes on the server is sent to the clients (`EncodeLatency`).
- **WebRTC in Rust (str0m):** Google Congestion Control sets the bitrate, frames go out in
  bursts paced to the estimate, lost packets are sent again (NACK, RTX); the sound is kept in
  step with the video.
- **Sound** in Opus with libopus, and the client's **microphone**.
- **Several monitors:** one guest screen per client monitor, at its size and place, up to
  four; idle screens and screens the client does not show cost little or nothing.
- **Keyboard and mouse** by scancode, the guest's pointer drawn by the client; **pause** of a
  hidden window; one user at a time, with **shadow** clients that only watch.
- With a guest agent (see [ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol)) in a Windows guest: **clipboard** (text, images, files),
  **drives**, **printers** and **cameras** of the client.
- **Security:** HTTPS only, a user and an Argon2id password hash, ACME certificates. As a
  service, it runs under an account of its own, confined by systemd, its certificate's key
  handed to it by systemd (optionally sealed by a TPM 2.0).
- **VM managers:** libvirt (through libvirt-dbus), Proxmox VE (not tried yet), or VMs given by
  hand.
