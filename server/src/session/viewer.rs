// SPDX-License-Identifier: MPL-2.0

//! One viewer's session: its WebRTC peer connection, and what goes between it and the guest.

use anyhow::Result;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, warn};

use qdw_protocol::Message;
use qdw_protocol::v1::{
    BandwidthEstimate, ControlMessage, EncodeLatency, Ended, Input, Microphone, MonitorLayout,
    PointerMode, SessionDescription, VideoPause, client_message, control_message, ended,
    server_message,
};

use super::shared::{Request, Shared};
use super::signalling::{Socket, close, receive, send};
use crate::guest::cameras::{CamerasFollow, ViewerCameras};
use crate::guest::clipboard::{ViewerClipboard, ViewerFollow};
use crate::guest::drives::{DrivesFollow, ViewerDrives};
use crate::guest::printers::{PrintersFollow, ViewerPrinters};
use crate::session::input::ViewerInput;
use crate::session::layout::Size;
use crate::session::pointer::{self, Pointer};
use crate::session::transport::{Channel, Session, Signal, Start};
use crate::vms::Client;

/// The addresses of the host's public name or address, for a session's candidates: none when
/// it does not resolve, the session then reachable on the host's own addresses only.
async fn public_addresses(name: &str) -> Vec<std::net::IpAddr> {
    match tokio::net::lookup_host((name, 0)).await {
        Ok(addresses) => {
            let mut ips: Vec<std::net::IpAddr> = addresses.map(|a| a.ip()).collect();
            ips.dedup();
            ips
        }
        Err(e) => {
            warn!(name, error = %e, "public address not resolved: the session announces none");
            Vec::new()
        }
    }
}

/// The STUN server's address, the first its name resolves to, IPv4 first (the host's NAT is
/// on IPv4): none when it does not resolve, the session then without a reflexive address.
async fn stun_server(server: &str) -> Option<std::net::SocketAddr> {
    match tokio::net::lookup_host(server).await {
        Ok(addresses) => {
            let addresses: Vec<std::net::SocketAddr> = addresses.collect();
            addresses
                .iter()
                .find(|a| a.is_ipv4())
                .or(addresses.first())
                .copied()
        }
        Err(e) => {
            warn!(server, error = %e, "STUN server not resolved");
            None
        }
    }
}

/// The session of a client of the VM: `codecs` it decodes, from its hello, and whether it
/// only watches.
pub(crate) async fn viewer(
    mut socket: Socket,
    client: &Client,
    codecs: Vec<String>,
    shadow: bool,
    server_stopping: watch::Receiver<bool>,
) -> Result<()> {
    let stopping = server_stopping.clone();
    let shared = client.shared().clone();

    // The video in 10 bits if this viewer and all the others decode it.
    let deep = codecs.iter().any(|c| shared.codec.deep_by(c));
    let (depth_id, ten_bits) = shared.depths.join(deep);
    let _ = shared.requests.send(Request::TenBits(ten_bits));
    let (signals_tx, mut signals) = mpsc::unbounded_channel();
    let requests = shared.requests.clone();
    let consoles: Vec<u32> = shared.screens.iter().map(|s| s.console).collect();
    let public = match &shared.network.public_address {
        Some(name) => public_addresses(name).await,
        None => Vec::new(),
    };
    let stun = match &shared.network.stun {
        Some(server) => stun_server(server).await,
        None => None,
    };
    let started = Session::start(
        Start {
            codec: shared.codec,
            full_chroma: shared.full_chroma,
            // The depth the encoders take now. A known limit: when it changes later (another
            // viewer comes or goes), the stream's HEVC profile changes and the offer, made once,
            // does not say it: the session does not renegotiate.
            ten_bits,
            viewers: shared.screens.iter().map(|s| s.viewers.clone()).collect(),
            audio_viewers: shared.audio_viewers.clone(),
            mic: (!shadow).then(|| shared.mic.clone()),
            bitrates: shared.bitrates,
            udp_ports: shared.network.udp_ports,
            public,
            stun,
        },
        signals_tx,
        move |screen| {
            if let Some(&console) = consoles.get(screen) {
                let _ = requests.send(Request::Keyframe { console });
            }
        },
    );
    // A session that does not start leaves like one that ends: its depth no longer counts (or
    // the VM would encode in 8 bits for all those after it), and the client is told a failure.
    let session = match started {
        Ok(session) => session,
        Err(e) => {
            let _ = shared
                .requests
                .send(Request::TenBits(shared.depths.leave(depth_id)));
            close(&mut socket, axum::extract::ws::close_code::ERROR).await;
            return Err(e);
        }
    };
    let user = (!shadow).then(|| shared.user.take_over());
    info!(shadow, "viewer session");
    let (clipboard, clip) = shared.clipboard.viewer();
    let (drives, drives_follow) = shared.drives.viewer();
    let (printers, printers_follow) = shared.printers.viewer();
    let (cameras, cameras_follow) = shared.cameras.viewer();
    let mut state = ViewerState {
        input: ViewerInput::new(
            shared.input.clone(),
            shared.agent_mouse.clone(),
            shared.screens.iter().map(|s| s.head).collect(),
        ),
        clipboard,
        drives,
        printers,
        cameras,
    };
    let (user_id, taken_over) = user.unzip();
    // The session waits on calls to the VM: when the VM stops, one may only fail when its bus
    // goes, seconds later. The session ends as soon as the display loop sees the VM stop,
    // whatever it waits on.
    let mut vm_stopped = shared.vm_stopped.clone();
    // A shadow client watches the user: it goes when the user does.
    let mut user_left = user_id.is_none().then(|| shared.user.left());
    let ended = tokio::select! {
        ended = run(
            &mut socket,
            &session,
            &mut signals,
            &mut state,
            Follow {
                pointer: shared.pointer.clone(),
                pointer_mode: shared.pointer_mode.clone(),
                encode_latency: shared.encode_latency.clone(),
                clip,
                drives: drives_follow,
                printers: printers_follow,
                cameras: cameras_follow,
                taken_over,
                server_stopping,
            },
            &shared,
        ) => ended,
        true = async { vm_stopped.wait_for(|stopped| *stopped).await.is_ok() } => {
            info!("the VM stopped: the session ends");
            Ok(())
        }
        () = async {
            match &mut user_left {
                Some(left) => {
                    let _ = left.changed().await;
                }
                None => std::future::pending().await,
            }
        } => {
            info!("the user left: the shadow session ends");
            Ok(())
        }
    };
    // Whatever ended the session, the WebSocket closes with a close frame, whose status tells
    // the client how: a normal end, the server going away (it may come back), or a failure.
    let code = match &ended {
        Ok(()) if *stopping.borrow() => axum::extract::ws::close_code::AWAY,
        Ok(()) => axum::extract::ws::close_code::NORMAL,
        Err(_) => axum::extract::ws::close_code::ERROR,
    };
    close(&mut socket, code).await;
    // Leave no key or button pressed in the guest, and the clipboard to the guest; of a VM that
    // stopped, there is nothing left to release, and calls to it would wait in vain.
    if !*shared.vm_stopped.borrow() {
        let release = async {
            if let Err(e) = state.input.release_all().await {
                debug!(error = %format!("{e:#}"), "keys not released");
            }
            if let Err(e) = state.clipboard.release().await {
                debug!(error = %format!("{e:#}"), "clipboard not released");
            }
            state.drives.release().await;
            state.printers.release().await;
            state.cameras.release().await;
        };
        if tokio::time::timeout(RELEASE_TIME, release).await.is_err() {
            debug!("the guest did not answer: the session's keys and devices not all released");
        }
    }
    if let Some(id) = user_id {
        shared.user.leave(id);
    }
    let _ = shared
        .requests
        .send(Request::TenBits(shared.depths.leave(depth_id)));
    ended
}

/// How long the guest has to release what a session that ends held: keys, the clipboard,
/// drives, printers, cameras.
const RELEASE_TIME: std::time::Duration = std::time::Duration::from_secs(2);

/// The client hears of the bandwidth estimate at most this often.
const ESTIMATE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// The sound's share of a connection, in kbit/s: Opus at 128 kbit/s, with its packet headers.
const AUDIO_BITRATE: u32 = 150;

/// The share of what the sound leaves that the video encoders aim at: the rest is room for
/// what they send beyond their target and for the retransmissions. Measured on a 10 Mbit/s
/// link, HEVC 4:4:4 at 3440x1440 sent 10 to 25 % more than its target, and the client asked
/// thousands of packets again: aimed at the whole estimate, the link's queue filled.
const VIDEO_SHARE: f64 = 0.85;

/// The video bitrate for a bandwidth estimate, in kbit/s.
fn video_bitrate(estimate: u32) -> u32 {
    ((f64::from(estimate.saturating_sub(AUDIO_BITRATE)) * VIDEO_SHARE) as u32).max(1)
}

/// What a viewer has of the guest: its keyboard and pointer, the clipboard, its drives and
/// printers.
struct ViewerState {
    input: ViewerInput,
    clipboard: ViewerClipboard,
    drives: ViewerDrives,
    printers: ViewerPrinters,
    cameras: ViewerCameras,
}

/// What a viewer follows besides its connection.
struct Follow {
    pointer: watch::Receiver<Vec<Pointer>>,
    pointer_mode: watch::Receiver<bool>,
    encode_latency: watch::Receiver<Vec<Option<(std::time::Duration, std::time::Duration)>>>,
    clip: ViewerFollow,
    drives: DrivesFollow,
    printers: PrintersFollow,
    cameras: CamerasFollow,
    /// For the user: another client took the session over. `None` for a shadow client.
    taken_over: Option<oneshot::Receiver<()>>,
    /// Turns true when the server stops.
    server_stopping: watch::Receiver<bool>,
}

fn send_all(session: &Session, channel: Channel, messages: Vec<Vec<u8>>) {
    for message in messages {
        session.send(channel, message);
    }
}

async fn run(
    socket: &mut Socket,
    session: &Session,
    signals: &mut mpsc::UnboundedReceiver<Signal>,
    state: &mut ViewerState,
    mut follow: Follow,
    shared: &Shared,
) -> Result<()> {
    let keyframe = || {
        for screen in &shared.screens {
            let _ = shared.requests.send(Request::Keyframe {
                console: screen.console,
            });
        }
    };
    // The last estimate sent to the client, and when.
    let mut estimate_sent: Option<(std::time::Instant, u32)> = None;
    // A shadow client only watches: what it sends to the guest is ignored, and it gets nothing
    // of the guest's clipboard.
    let mut server_stopping = follow.server_stopping.clone();
    let taken_over = follow.taken_over.take();
    let user = taken_over.is_some();
    let taken_over = async move {
        match taken_over {
            Some(taken_over) => {
                let _ = taken_over.await;
            }
            None => std::future::pending().await,
        }
    };
    tokio::pin!(taken_over);
    let ViewerState {
        input,
        clipboard,
        drives,
        printers,
        cameras,
    } = state;
    // Pointer updates wait for the pointer channel to open.
    let mut sent: Option<pointer::Sent> = None;
    // Whether the guest records, for the user's client once its control channel is open.
    let mut mic_wanted = shared.mic.wanted();
    let mut control_open = false;
    let send_microphone = |wanted: bool| {
        let message = ControlMessage {
            message: Some(control_message::Message::Microphone(Microphone { wanted })),
        };
        session.send(Channel::Control, message.encode_to_vec());
    };
    let send_pointer_mode = |relative: bool| {
        let message = ControlMessage {
            message: Some(control_message::Message::PointerMode(PointerMode {
                relative,
            })),
        };
        session.send(Channel::Control, message.encode_to_vec());
    };
    let mut reception = tokio::time::interval(std::time::Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = reception.tick() => session.report_reception(),
            () = &mut taken_over => {
                info!("another client took the session over");
                send(socket, server_message::Message::Ended(Ended { reason: ended::Reason::TakenOver.into() })).await?;
                return Ok(());
            }
            true = async { server_stopping.wait_for(|stopping| *stopping).await.is_ok() } => {
                send(socket, server_message::Message::Ended(Ended { reason: ended::Reason::ServerStopping.into() })).await?;
                return Ok(());
            }
            signal = signals.recv() => match signal {
                Some(Signal::Offer(sdp)) => {
                    send(socket, server_message::Message::Offer(SessionDescription {
                        sdp,
                        vm_name: shared.name.clone(),
                        // The client asks the same STUN server for its own address.
                        ice_servers: shared.network.stun.iter().map(|s| format!("stun:{s}")).collect(),
                        turn_servers: shared.network.turn.iter()
                            .map(|t| t.for_session(std::time::SystemTime::now())).collect(),
                        // A broker in front decides it, in the offer it relays.
                        relay_only: false,
                    })).await?
                }
                Some(Signal::Message(Channel::Input | Channel::Clipboard | Channel::Drive | Channel::Printer | Channel::Camera, _)) if !user => {}
                Some(Signal::Message(Channel::Camera, bytes)) => {
                    if let Err(e) = cameras.message(&bytes).await {
                        warn!(error = %format!("{e:#}"), "camera");
                    }
                }
                Some(Signal::CameraFrame { mid, timestamp_us, data }) => {
                    if user {
                        cameras.frame(&mid, timestamp_us, data, session);
                    }
                }
                Some(Signal::Message(Channel::Input, bytes)) => match Input::decode(bytes.as_slice()) {
                    Ok(events) => input.apply(events).await?,
                    Err(e) => warn!(error = %e, "undecodable input"),
                },
                Some(Signal::Message(Channel::Clipboard, bytes)) => match clipboard.message(&bytes).await {
                    Ok(messages) => send_all(session, Channel::Clipboard, messages),
                    Err(e) => warn!(error = %format!("{e:#}"), "clipboard"),
                },
                Some(Signal::Message(Channel::Control, bytes)) => match ControlMessage::decode(bytes.as_slice()) {
                    Ok(ControlMessage { message: Some(control_message::Message::VideoPause(VideoPause { paused })) }) => {
                        if session.pause_video(paused) {
                            keyframe();
                        }
                    }
                    Ok(ControlMessage { message: Some(control_message::Message::MonitorLayout(MonitorLayout { monitors })) }) => {
                        // One guest screen per monitor, as many as the guest has; the
                        // others off.
                        let screens: Vec<Size> = monitors
                            .iter()
                            .filter_map(Size::of)
                            .take(shared.screens.len())
                            .collect();
                        if user && !screens.is_empty() {
                            shared.layout.want(screens);
                        }
                    }
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "undecodable control message"),
                },
                Some(Signal::Message(Channel::Drive, bytes)) => {
                    if let Err(e) = drives.message(&bytes).await {
                        warn!(error = %format!("{e:#}"), "drive");
                    }
                }
                Some(Signal::Message(Channel::Printer, bytes)) => {
                    if let Err(e) = printers.message(&bytes).await {
                        warn!(error = %format!("{e:#}"), "printer");
                    }
                }
                Some(Signal::Message(Channel::Pointer, _)) => {}
                Some(Signal::Open(Channel::Pointer)) => {
                    let mut fresh = pointer::Sent::default();
                    let relative = *follow.pointer_mode.borrow();
                    send_all(session, Channel::Pointer, fresh.update(&follow.pointer.borrow_and_update(), relative));
                    sent = Some(fresh);
                }
                Some(Signal::Open(Channel::Clipboard)) if !user => {}
                Some(Signal::Open(Channel::Clipboard)) => {
                    let guest = follow.clip.guest.borrow_and_update().clone();
                    send_all(session, Channel::Clipboard, clipboard.opened(guest));
                }
                Some(Signal::Open(Channel::Control)) => {
                    control_open = true;
                    send_pointer_mode(*follow.pointer_mode.borrow_and_update());
                    if user {
                        send_microphone(*mic_wanted.borrow_and_update());
                    }
                }
                Some(Signal::Open(Channel::Input | Channel::Drive | Channel::Printer | Channel::Camera)) => {}
                Some(Signal::Estimate(estimate)) => {
                    // The user's bandwidth sets the bitrate of the video, which all viewers
                    // share: a shadow client on a slower connection gets what it can.
                    if user {
                        let _ = shared.requests.send(Request::Bitrate {
                            bitrate: video_bitrate(estimate),
                        });
                    }
                    let due = estimate_sent.is_none_or(|(at, sent)| {
                        sent != estimate && at.elapsed() >= ESTIMATE_INTERVAL
                    });
                    if due {
                        estimate_sent = Some((std::time::Instant::now(), estimate));
                        let message = ControlMessage {
                            message: Some(control_message::Message::BandwidthEstimate(
                                BandwidthEstimate { kbps: estimate },
                            )),
                        };
                        session.send(Channel::Control, message.encode_to_vec());
                    }
                }
                Some(Signal::Closed) | None => return Ok(()),
            },
            Ok(()) = follow.pointer.changed() => {
                if let Some(sent) = &mut sent {
                    let relative = *follow.pointer_mode.borrow();
                    send_all(session, Channel::Pointer, sent.update(&follow.pointer.borrow_and_update(), relative));
                }
            }
            Ok(()) = follow.encode_latency.changed() => {
                let latencies = follow.encode_latency.borrow_and_update().clone();
                if control_open {
                    for (screen, latency) in latencies.into_iter().enumerate() {
                        let Some((mean, max)) = latency else { continue };
                        let micros = |d: std::time::Duration| u32::try_from(d.as_micros()).unwrap_or(u32::MAX);
                        let message = ControlMessage {
                            message: Some(control_message::Message::EncodeLatency(EncodeLatency {
                                screen: screen as u32,
                                mean_us: micros(mean),
                                max_us: micros(max),
                            })),
                        };
                        session.send(Channel::Control, message.encode_to_vec());
                    }
                }
            }
            Ok(()) = follow.pointer_mode.changed() => {
                let relative = *follow.pointer_mode.borrow_and_update();
                if control_open {
                    send_pointer_mode(relative);
                }
                // The positions go, or stop, with the mode.
                if let Some(sent) = &mut sent {
                    send_all(session, Channel::Pointer, sent.update(&follow.pointer.borrow(), relative));
                }
            }
            Ok(()) = mic_wanted.changed(), if user && control_open => {
                send_microphone(*mic_wanted.borrow_and_update());
            }
            Ok(()) = follow.clip.guest.changed(), if user => {
                let guest = follow.clip.guest.borrow_and_update().clone();
                send_all(session, Channel::Clipboard, clipboard.guest_changed(guest));
            }
            Some(call) = follow.clip.calls.recv() => {
                send_all(session, Channel::Clipboard, clipboard.host_call(call));
            }
            Some(message) = follow.clip.outgoing.recv() => session.send(Channel::Clipboard, message),
            Some(call) = follow.drives.calls.recv() => send_all(session, Channel::Drive, drives.call(call)),
            Some(message) = follow.drives.outgoing.recv() => session.send(Channel::Drive, message),
            Some(call) = follow.printers.calls.recv() => send_all(session, Channel::Printer, printers.call(call)),
            Some(message) = follow.printers.outgoing.recv() => session.send(Channel::Printer, message),
            Some(call) = follow.cameras.calls.recv() => send_all(session, Channel::Camera, cameras.call(call, session)),
            Some(message) = follow.cameras.outgoing.recv() => session.send(Channel::Camera, message),
            message = receive(socket) => match message? {
                Some(client_message::Message::Answer(SessionDescription { sdp, .. })) => session.apply_answer(&sdp)?,
                // An empty candidate marks the end of the client's candidates.
                Some(client_message::Message::Ice(ice)) if !ice.candidate.is_empty() => {
                    session.add_ice_candidate(ice.sdp_mline_index, &ice.candidate)
                }
                Some(client_message::Message::Ice(_)) => {}
                Some(client_message::Message::Hello(_)) => anyhow::bail!("a second hello"),
                None => return Ok(()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The video aims below the estimate, after the sound.
    #[test]
    fn the_video_leaves_room() {
        assert_eq!(video_bitrate(10_150), 8_500);
        assert_eq!(video_bitrate(100), 1);
    }
}
