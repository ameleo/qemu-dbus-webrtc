# Architecture

qemu-dbus-webrtc runs on the host, next to QEMU. It attaches to running VMs through QMP and
QEMU's D-Bus display (`-display dbus`), encodes the guest display on the host GPU and streams it
to clients over WebRTC. It does not manage the VM lifecycle: the VM manager (Proxmox VE,
libvirt, plain QEMU) starts and stops VMs.

```
 browser ── WebRTC (video, audio, data channels) ── qemu-dbus-webrtc ── D-Bus / QMP ── QEMU
                                                          │
                                                          └── VA-API (host GPU encoder)
```

## Components

- **VM discovery:** one server handles all VMs of its host, discovered by providers, one per VM
  manager: libvirt through libvirt-dbus (all domains with a D-Bus display, running or not, by
  UUID and name; the display bus of a running domain from its live definition; starting a
  stopped domain when `[libvirt] start` allows it), Proxmox VE (QMP sockets and VM
  configurations), and a static list. A VM's page is `/vm/<uuid>/` and its signaling endpoint
  `/vm/<uuid>/signal`; the VM name can replace the UUID. The VM is only looked up once the
  client has authenticated and announced the codecs it decodes, so the list of VMs is never
  disclosed to anyone else. The server attaches to a VM when its first client connects, after
  checking the VM's UUID, and detaches when the last client leaves: 5 s after a client that
  said it leaves (enough to reload the page), a little over a minute after a lost connection
  (the client may reconnect, then the guest is told its user left), and immediately when the VM
  stops, which it detects by polling every second. A VM without clients costs nothing. On
  shutdown, the server ends all sessions, tells the guests their users left, and detaches from
  all VMs before exiting.
- **Connectivity (ICE):** each session binds its own UDP sockets (in `udp_ports` when set) and
  offers their host candidates, `public_address` on them when set, and the address a STUN
  server sees each socket at (server-reflexive, RFC 8489; Google's server by default). The
  offer also gives the client the STUN server, for its own reflexive address, and the TURN
  relays of `[[turn]]` (RFC 8656), with fixed credentials or credentials computed for the
  session from a secret shared with the relay (the TURN REST API draft, as coturn's
  `use-auth-secret`: valid for a day). The server runs no relay and needs no TURN client: a
  relayed client's candidates are ordinary remote candidates to it. The client uses a relay
  only when no direct pair works (`?relay` in the page's address forces it).
- **Connection broker:** with `[broker]`, the host serves its VMs through the broker only,
  and opens no port: the configuration refuses, beside it, whatever would open its own pages
  (`[auth]`, `[[account]]`, `[acme]`, `[listen]`'s address and certificate) or add a relay of
  its own (`[[turn]]`). The host keeps a gRPC connection to the broker
  (`qdw_protocol::broker`, `proto/broker.proto`), which it opens in mutual TLS, with the
  certificate the broker's ACME server issued it (`attestation`: its key in the TPM, proven by
  device-attest-01; renewed two thirds of the way through its validity, then connected again
  with it), and opens again when it drops (1 s, doubled up to 30 s). On the `Control`
  stream it sends its Hello, its inventory (every VM its providers see, with its UUID, sent again
  when it changes, checked every 5 s) and its page; the broker sends `Connect` requests. For each,
  the host calls `Relay`, which carries the user's signalling messages as its WebSocket would
  (`web::Socket`), and serves it as one of its own clients, the broker's word standing for the
  account (`web::Admission::Broker`): the VM it names only, view-only when it says. The host
  ends a relayed session with a `Close` carrying the WebSocket status, then waits for the
  broker to end the call, since dropping it would cancel the stream and lose the last messages.
  Without accounts or a certificate, a host with a broker serves no page itself.
- **Session termination:** the server always ends a session with a clean WebSocket close, so
  that clients treat a clean close as a deliberate end (the native client closes its windows)
  and a lost connection as a failure to recover from. An `Ended` message gives the reason when
  there is one: another client took over the session, or the server is shutting down (and may
  come back). Clients also close cleanly when they leave.
- **QEMU attachment:** the D-Bus display (on a libvirt bus, or peer-to-peer through QMP) for
  scanouts (DMA-BUF, `ScanoutDMABUF2`), cursor, keyboard, mouse, audio and clipboard.
- **Video encoding:** the `qdw-hw-encoder` crate, without a media framework: VA-API on the GPU
  (AV1, HEVC, VP9) and libvpx on the CPU (VP9, for hosts without a GPU encoder). On the GPU, the
  guest framebuffer, a linear DMA-BUF, is imported as a VA surface and converted to YUV in
  place. With virgl, it lives in GPU memory, which the CPU could only read over the bus at one
  or two frames per second. A framebuffer in memory shared with QEMU is uploaded to the GPU
  first. The crate writes the AV1 and HEVC headers itself (the VP9 driver writes its own), so
  keyframes and bitrate changes apply on the next frame and the streams signal their color
  description. Up to 60 fps per screen: the encoders measure the guest's frame rate and spread
  the bitrate over it.
- **Transport:** str0m (`session/peer.rs`), a Rust WebRTC implementation without a media framework,
  with Google Congestion Control (GCC) driving the encoder bitrate. Each session has a task that
  owns the str0m state and its UDP sockets (one host candidate per host address, loopback excluded).
  str0m packetizes the frames, paces them at its GCC estimate (with probing), handles
  retransmissions (NACK, RTX) and the SCTP data channels. The session decodes the microphone's Opus
  with libopus and relays the client cameras' H.264.
- **Per-viewer sessions:** each viewer has its own session, fed with the encoded frames of the
  screens it watches, shared without copying: one encoder per screen, whatever the number of
  viewers. A viewer that loses a frame requests a keyframe (PLI), which is forwarded to the
  encoder.
- **Pause:** a client whose window is hidden pauses its video (`control` data channel): it is
  removed from the screen's viewers, and a screen without viewers is not encoded. Audio
  continues. On resume, a keyframe restores the picture immediately.
- **Signaling:** a WebSocket carrying the SDP offer and answer and the ICE candidates. The
  client's hello carries a username and password (`[auth] method = "password"`, the default),
  checked against the configuration's accounts (Argon2id hashes, one check whatever the user, so
  that the time tells nothing), then whether the account may open the VM it asks for, by UUID or
  name. Both happen before the server starts or attaches to the VM, whatever the VM manager; a
  wrong password is answered after one second, and a VM the account may not open gets the same
  answer as a VM the host does not run.
  This protects access to the server; the guest still requires its own logon. Other methods are
  planned: client certificates, FIDO security keys, broker tokens. The server never logs on to
  Windows.
- **Client protocol:** messages between the server and its clients are Protocol Buffers,
  defined in `crates/protocol/proto/qdw.proto`. Clients in any language generate their code from
  it; the browser page embeds a small hand-written codec for the few messages it needs. Device
  channels reuse the RDP formats (see below).

## Source layout

| Directory | Crate | Role |
|---|---|---|
| `crates/protocol` | `qdw-protocol` | the `.proto` file and the generated Rust types |
| `crates/qemu` | `qdw-qemu` | QEMU attachment: QMP, D-Bus display and audio, the guest agent |
| `crates/video` | `qdw-video` | per-screen encoding on a dedicated thread, frame dropping, recording |
| `crates/hw-encoder` | `qdw-hw-encoder` | VA-API encoding, with the codec headers written from their specifications |
| `crates/audio` | `qdw-audio` | Opus encoding of the guest audio, microphone decoding |
| `crates/web` | `qdw-web` | the browser page, which the server and a broker serve |
| `server` | `qemu-dbus-webrtc` | the server, in modules: `vms` (the VM managers, attaching to a VM, its display loop), `session` (the HTTPS server and signaling, each viewer's peer connection, input, screens' layout, sound), `guest` (what goes through the guest agent: clipboard, drives, printers, cameras), `network` (STUN, TURN), `host` (accounts, the server's certificate, the connection broker and the host's attested certificate to it) |

The native client lives in a separate repository and implements the same protocol.

## Clients

Two clients, one protocol: the browser, and a native client based on libwebrtc, the WebRTC
engine of Chromium and Safari. Both have the same transport behavior (congestion control
feedback, retransmission, jitter buffer, SCTP data channels), so a problem seen with one is seen
with the other. Bandwidth estimation runs on the sender side, in the server, whatever the
client.

Each connection estimates its bandwidth with str0m's GCC implementation, from the client's
transport-wide congestion control feedback, which also paces the packets and probes up to the
maximum bitrate. The video bitrate is 85 % of the user's estimate minus the audio, between 4
and 30 Mbit/s (`--bitrate` sets the maximum), starting at 10 Mbit/s, and is shared by all
viewers. When an encoder runs ahead of its bitrate, a leaky-bucket frame dropper skips frames
rather than letting the sender queue grow. Each client receives its own estimate on the
`control` channel and, once per second, the server-side encoding latency of each screen
(`EncodeLatency`), from which it computes the guest-to-display latency. Bitrate changes apply on
the next frame, without a keyframe.

Decoding is where clients differ: the native client brings its own decoders and supports formats
browsers may not (VP9 4:4:4). The codec is negotiated in SDP, from the best one the client
supports down to VP9 4:2:0, which all current browsers decode. H.264 is not offered: the project
targets current hardware, which the royalty-free codecs cover.

One client controls the session at a time, as with RDP: its keyboard and pointer, clipboard,
microphone, drives and printers are connected to the guest, and the guest resolution follows its
window. A new client takes over the session, and the previous one is ended with a reason. Shadow
clients (`shadow` in their hello) are view-only, unlimited in number, and are disconnected when
the user leaves (not on a takeover); handing them keyboard and mouse control is planned.

When the user leaves, the server tells the guest agent (`Agent1.Reset`), which lets go of what
the client brought (clipboard, drives, printers, cameras) and locks the guest's session if a user
is logged on: whether and how to lock is the agent's to decide, not the server's. A client that
leaves on purpose (its disconnect button, its window closed) says so (`Leave`) before it closes
the signalling; a session that ends without it is a lost connection, which the client may
restore. The guest is told at once after a `Leave` or when the server stops, and a minute after
a lost connection if no client took the session meanwhile; not when another client takes the
session over, nor when the VM stops. Telling the guest and a client taking the session run one
at a time, so that a Reset never lets go of what a client that just took the session brought.
The server cannot tell one person from another: someone who takes the session within the minute
after a lost connection finds it as the last user left it, as after a takeover. Without an agent,
nothing is done.

The user may also reset the VM, for a guest that no longer answers, from the menu of the toolbar's
power button (with leaving), when allowed to: an account's `power = true`, or the broker's
`Connect.power` for the users it relays; never a shadow client, nor anyone without accounts. A
guest that answers restarts or shuts down from its own menu, so the console offers nothing gentler,
which would also need the guest to follow a request (the ACPI power button, which Windows' power
settings may have it ignore, or the QEMU guest agent). The server asks the VM's manager: libvirt,
when libvirt-dbus knows the domain; otherwise QEMU's QMP socket, when the server attaches through
it (`system_reset`). A VM on a D-Bus bus that libvirt does not manage cannot be reset. The offer
lists the actions the user may ask (`power_actions`); the client asks one (`Power`), which the
server takes only if listed, and answers when the manager took it or failed (`PowerDone`; why it
failed only in the server's log, as it may name the host's paths).

The guest resolution follows the user's window: once the size has been stable for 300 ms, the
server requests it from the guest (`SetUIInfo`, rounded to even values between 200 and 8192
pixels). A request sent while the guest boots is lost, as its display driver restores the mode
Windows remembered, so the server requests it again if the guest still shows another size 3 s
later, up to three times. The guest keeps the resolution after the user leaves. Each guest screen
is given the physical size of 96 dpi at its resolution, whatever the user's monitor: Windows then
stays at 100 % scale, and only the client scales.

With several monitors (native client), the client sends its monitor layout with each monitor's
position (and the physical size it may send, unused). The guest gets one screen per monitor, in order, at
its size and position (`SetUIInfo` with offsets), and its other screens are disabled: the guest
desktop mirrors the client's layout, and the pointer moves between screens as on the client.
Each guest screen (a graphic console per head, up to four) has its own encoder and video track,
all with the same payload type and distinguished by their mid; the offer lists the screens
first, in order. Audio shares the screens' media stream (msid), so the client keeps it in sync
with the video, as video calls in the guest require. The bitrate is shared between the screens
that are enabled and displayed by the client: a screen without guest updates for 2 s keeps
500 kbit/s, and active screens share the rest in proportion to their area (all screens by area
when none is active). A screen outside the client's layout is not encoded, even if the guest
keeps it enabled (Windows' viogpu driver ignores disabled screens); it resumes with a keyframe
when the client displays it again. The pointer goes through the guest agent (`Mouse1`), which
places it on the head named by the client; the QEMU tablet only knows a single surface and
serves the first screen when there is no agent. The cursor shape carries its screen, and the
client draws it on that screen only. The browser page displays the first screen, or, in
browsers with the Window Management API, each screen in its own window on its own monitor.

## Codecs

| Codec | Chroma | Royalties | Use |
|---|---|---|---|
| AV1 | 4:2:0 | royalty-free license | default: most efficient at low bitrates |
| VP9 | 4:4:4 (profile 1) or 4:2:0 (profile 0) | royalty-free license | 4:4:4 for text quality when bandwidth allows; 4:2:0 as the fallback all browsers decode |
| HEVC (H.265) | 4:4:4 or 4:2:0 | patent pools | optional, disabled by default |

4:2:0 subsampling keeps the luma, and therefore black-on-white text, at full resolution but
halves the chroma resolution: colored text and thin colored lines blur. Measured on an Intel Arc
A380 with a scrolling code editor at fixed quantizers, the chroma PSNR of every 4:2:0 codec
plateaus around 43 dB regardless of bitrate, while 4:4:4 keeps improving; at low bitrates, AV1
4:2:0 needs about 20 % fewer bits than VP9 4:4:4 for the same RGB quality. Hence AV1 when
bandwidth is scarce and VP9 4:4:4 above that. Full chroma uses the codec's own 4:4:4 profile,
encoded in hardware: Intel GPUs encode VP9 profile 1, but AV1 in 4:2:0 only.

RGB is converted to YUV with the BT.601 matrix in limited range, which decoders assume when a
stream does not signal otherwise. The AV1 and HEVC streams signal it (with BT.709 primaries and
the sRGB transfer function, the desktop's), as does libvpx's VP9; Intel's VA driver leaves the
color space of its VP9 streams unspecified (measured on the A380), which decoders treat as
BT.601.

> **Patent royalties.** AV1 and VP9 come with royalty-free patent licenses (from the Alliance
> for Open Media and Google); third-party pools also claim patents on them, mostly targeting
> device makers. HEVC is covered by patents licensed through patent pools, whose terms can apply
> to streaming services. HEVC is therefore disabled by default, and enabling it is the
> deployer's decision. This project does not encode pictures itself: the host GPU's hardware
> encoder does, through VA-API (the project writes the AV1 and HEVC headers that describe the
> stream), or libvpx for VP9 on the CPU; decoding happens in the user's browser or operating
> system. Whether a deployment needs a patent license depends on its use and jurisdiction; this
> is not legal advice.

Audio is Opus at 128 kbit/s in 10 ms frames, encoded once for all viewers with libopus on a
dedicated thread that emits a frame every 10 ms, filling gaps with silence to keep the stream
flowing (a WebRTC sender needs audio flowing before it can make its offer). Guest samples are
converted to 48 kHz stereo (rubato resamples other rates) and buffered 20 ms to absorb QEMU's
jitter. Frames are timestamped by sample count rather than on arrival, and these timestamps
become the RTP timestamps the receiver uses to place the samples. Browsers only play audio after
a user interaction: the page starts muted and unmutes on the first click or key press.

The client's microphone comes back on the bidirectional audio track: the server decodes it and
feeds the guest's microphone through QEMU (`AudioInListener`). QEMU reads the samples from its
main loop and waits for them, so the server keeps them ready in the guest's format, buffering
at most 100 ms to bound the delay. One viewer speaks at a time: the one sending audio, until it
has been silent for one second.

As for cameras, the client only opens its microphone while the guest records: QEMU reports when
a guest application opens or closes it (`SetEnabled`), and the server notifies the user's client
(`Microphone` on the control channel), waiting two seconds before reporting a stop, since
applications often close and reopen the microphone within moments. The audio track stays
negotiated throughout. In the browser page, a button grants microphone access (browsers require
a click and a permission); the microphone then only opens while the guest records.

## Devices: RDP channel formats over data channels

WebRTC carries video and audio natively, including the browser's camera and microphone
(`getUserMedia`). For everything else, the project reuses the Remote Desktop Protocol's device
redirection formats, each on its own reliable, ordered data channel. They are specified, proven
in production, and implemented on the client side by the IronRDP crates (MIT/Apache-2.0). Only
channels without a patent listed in Microsoft's Open Specifications patent map are used.

| Function | Format | Data channel | Browser | Native client |
|---|---|---|---|---|
| Clipboard | project format, modeled on MS-RDPECLIP | `clipboard` | text, images (HTTPS) | full, files included |
| Drives | project format, modeled on MS-RDPEFS | `drive` | no | full |
| Printers | project format, modeled on MS-RDPEPC and MS-RDPEXPS | `printer` | no | full |
| Cameras | project format, modeled on MS-RDPECAM; video on WebRTC tracks | `camera` | yes | full |
| Monitor layout, resize | project format, modeled on MS-RDPEDISP | `control` | one monitor | several |
| Smart cards | MS-RDPESC | `rdpdr` | no | planned |
| USB | MS-RDPEUSB | `urbdrc` | no | planned |
| Keyboard, mouse | project format, modeled on RDP | `input` | yes | yes |
| Cursor shape | project format | `pointer` | yes | yes |

Keyboard and mouse input follows RDP: keys are sent as scancodes, by position, and the guest
applies its own layout; lock key state is synchronized when the client gains focus; all pressed
keys are released when it loses focus, and by the server when a viewer disconnects; wheel
movement is counted in 1/120 of a notch. The cursor is not part of the video (virtio-gpu renders
it on its own plane): its shape is sent to the client, which uses it as its own cursor,
independently of video latency.

Touch input does not use MS-RDPEI, which has a listed patent.

The clipboard follows RDP's model: whoever copied last owns the clipboard and announces the
formats it holds, and the data are only transferred on paste. In the guest, the guest agent owns
the guest's clipboard. A browser page cannot monitor the local clipboard: it reads it when it gains
focus and offers it to the guest if it changed. Browsers only expose the clipboard to pages
served over HTTPS, which the server always uses.

Drive redirection follows RDP: a client announces its drives, the guest agent mounts each one on
a drive letter (with Dokany), and every file operation of a guest application is forwarded to
the client as it happens, with Windows' own parameters and the client's NTSTATUS in return;
none of the client's files are cached in the guest. The server rejects paths containing `..`
before forwarding them, and removes a viewer's drives when it disconnects. A read or write
carries up to 1 MiB, fragmented across data channel messages.

Printer redirection follows RDP's Easy Print: a client announces its printers with their
driver's PrintCapabilities and default PrintTicket (Print Schema XML, as returned by Windows'
PrintTicket API), and the guest agent creates a matching virtual printer in its Print Support
App, on Windows' inbox driver. A guest print job is sent to the client as it is printed, as
OpenXPS with its PrintTicket, for the client's own driver, with a 1 MiB window ahead of the
client's acknowledgments; the guest is told whether the client could print it. The driver's
settings dialog opens on the client, and the guest keeps the user's choices. Capabilities,
tickets and job data are fragmented like drive reads.

Camera redirection follows RDP: the user's client announces its cameras with their H.264
formats and controls (numbered as in MS-RDPECAM), and the guest agent exposes each one in the
guest. The video itself is sent as WebRTC media: the offer reserves four receive-only H.264
tracks for the cameras, which the client sets to send in its answer. When a guest application
opens a camera, the server tells the client which track and format to use, and relays the H.264
as received, without transcoding. The client keeps the camera resolution from the first frame
(WebRTC would otherwise lower it while its bandwidth estimate ramps up, which the guest decoder,
configured for the announced format, rejects).

Of IronRDP, only the channel crates are used: `ironrdp-displaycontrol` (and later
`ironrdp-rdpeusb`) with their base crates. They do no I/O: bytes received on a data channel go
in, messages to send come out. The RDP server, connection and graphics crates are not needed:
WebRTC provides the transport, security and media.

In the guest, the optional guest agent (D-Bus over a QEMU character device, its contract defined
in [ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol)) provides what
QEMU alone cannot: clipboard, printer, drive and camera redirection as native Windows devices.

## Milestones

Each milestone is validated on real hardware before the next one starts.

1. **Attach:** QMP and D-Bus connection to a running VM, scanouts received, resolution changes
   followed.
2. **Encode:** VA-API AV1, VP9 (4:4:4 and 4:2:0) and HEVC; streams checked with a reference
   decoder, frame rate measured.
3. **Browser:** signaling and WebRTC video in Chrome and Firefox.
4. **Input:** keyboard and mouse over the `input` data channel.
5. **Audio:** Opus playback, then the microphone.
6. **Network and display:** congestion control, pause and resume, multi-monitor, resize,
   clipboard.
7. **Devices:** drives, then the guest agent.
8. **Deployment:** token authentication, packages, Proxmox VE integration.
