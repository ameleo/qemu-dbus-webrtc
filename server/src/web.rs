// SPDX-License-Identifier: MPL-2.0

//! The web side: the viewer page, and the WebSocket that carries the WebRTC signalling.
//!
//! The signalling messages are those of `qdw-protocol`, in Protocol Buffers, one per binary
//! WebSocket message.

use std::net::SocketAddr;

use std::path::PathBuf;

use anyhow::{Context, Result};
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{info, warn};

use qdw_protocol::v1::{
    ClientMessage, ControlMessage, Error, Hello, IceCandidate, Input, MonitorLayout, ServerMessage,
    SessionDescription, VideoPause, client_message, control_message, server_message,
};
use qdw_protocol::{Message, SIGNALLING_PATH};
use qdw_video::encode::Codec;

use std::sync::Arc;

use crate::audio::Mic;
use crate::clipboard::{Clipboard, ViewerClipboard, ViewerFollow};
use crate::drives::{Drives, DrivesFollow, ViewerDrives};
use crate::input::ViewerInput;
use crate::layout::{Layout, Size};
use crate::pointer::{self, Pointer};
use crate::printers::{Printers, PrintersFollow, ViewerPrinters};
use crate::session::{AudioViewers, Channel, Session, Signal, Viewers};

const PAGE: &str = include_str!("../web/index.html");

/// A request from a viewer to the display loop.
#[derive(Debug)]
pub enum Request {
    /// The viewer needs a keyframe of this console.
    Keyframe { console: u32 },
}

#[derive(Clone)]
pub struct Shared {
    pub codec: Codec,
    /// The video is in 4:4:4.
    pub full_chroma: bool,
    /// The console shown to viewers, and its viewers.
    pub console: u32,
    pub viewers: Viewers,
    /// The viewers of the guest's sound.
    pub audio_viewers: AudioViewers,
    /// The guest's microphone.
    pub mic: Arc<Mic>,
    pub requests: mpsc::UnboundedSender<Request>,
    /// The keyboard and pointer of the shown console.
    pub input: qdw_qemu::input::Input,
    /// The guest pointer of the shown console.
    pub pointer: watch::Receiver<Pointer>,
    pub clipboard: Arc<Clipboard>,
    pub drives: Arc<Drives>,
    pub printers: Arc<Printers>,
    /// The size of the shown console's screen.
    pub layout: Arc<Layout>,
    /// The user of the session.
    pub user: Arc<User>,
}

/// The one client that uses the session; the others are shadow clients, which only watch.
#[derive(Default)]
pub struct User {
    /// Its id, and how to end its session when another takes it over.
    current: std::sync::Mutex<Option<(u64, oneshot::Sender<()>)>>,
    next: std::sync::atomic::AtomicU64,
}

impl User {
    /// A new user takes the session over: the last one's session ends. Returns the new user's
    /// id, and what tells it its own session was taken over.
    fn take_over(&self) -> (u64, oneshot::Receiver<()>) {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (end, ended) = oneshot::channel();
        if let Some((_, last)) = self.current.lock().unwrap().replace((id, end)) {
            info!("a new client takes the session over");
            let _ = last.send(());
        }
        (id, ended)
    }

    /// The user `id` left; nobody uses the session until the next one comes.
    fn leave(&self, id: u64) {
        let mut current = self.current.lock().unwrap();
        if current.as_ref().is_some_and(|(user, _)| *user == id) {
            *current = None;
        }
    }
}

/// A certificate and its private key, PEM files.
#[derive(Debug, Clone)]
pub struct Tls {
    pub certificate: PathBuf,
    pub key: PathBuf,
}

/// Serve the page and the signalling on `listen` until the program ends, in HTTPS with `tls`.
/// Browsers give the clipboard to pages served in HTTPS (or from localhost) only.
pub async fn serve(listen: SocketAddr, tls: Option<Tls>, shared: Shared) -> Result<()> {
    let app = axum::Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route(SIGNALLING_PATH, get(upgrade))
        .with_state(shared)
        .into_make_service_with_connect_info::<SocketAddr>();
    match tls {
        Some(tls) => {
            // One crypto provider for the process: ring.
            let _ = rustls::crypto::ring::default_provider().install_default();
            let config =
                axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.certificate, &tls.key)
                    .await
                    .with_context(|| {
                        format!(
                            "loading {} and {}",
                            tls.certificate.display(),
                            tls.key.display()
                        )
                    })?;
            info!(%listen, "serving the viewer page in HTTPS");
            axum_server::bind_rustls(listen, config).serve(app).await?;
        }
        None => {
            let listener = tokio::net::TcpListener::bind(listen).await?;
            info!(%listen, "serving the viewer page");
            axum::serve(listener, app).await?;
        }
    }
    Ok(())
}

async fn upgrade(
    ws: WebSocketUpgrade,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(shared): State<Shared>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| async move {
        info!(%peer, "viewer connected");
        if let Err(e) = viewer(socket, shared).await {
            warn!(%peer, error = %format!("{e:#}"), "viewer session ended");
        } else {
            info!(%peer, "viewer left");
        }
    })
}

/// One message to the client, as a binary WebSocket message.
async fn send(socket: &mut WebSocket, message: server_message::Message) -> Result<()> {
    let bytes = ServerMessage {
        message: Some(message),
    }
    .encode_to_vec();
    socket.send(WsMessage::Binary(bytes.into())).await?;
    Ok(())
}

/// The next message from the client; `None` when it has gone.
async fn receive(socket: &mut WebSocket) -> Result<Option<client_message::Message>> {
    loop {
        match socket.recv().await {
            None | Some(Ok(WsMessage::Close(_))) => return Ok(None),
            Some(Ok(WsMessage::Binary(bytes))) => {
                let message = ClientMessage::decode(bytes)?.message;
                // An empty message is one of a newer client that this server does not know.
                if message.is_some() {
                    return Ok(message);
                }
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
        }
    }
}

async fn viewer(mut socket: WebSocket, shared: Shared) -> Result<()> {
    // The client first says which codecs it decodes.
    let Some(client_message::Message::Hello(Hello { codecs, shadow })) =
        receive(&mut socket).await?
    else {
        anyhow::bail!("no hello");
    };
    if !codecs
        .iter()
        .any(|c| shared.codec.decoded_by(shared.full_chroma, c))
    {
        let wanted = shared.codec.mime_type();
        let message = if shared.full_chroma {
            format!("this client does not decode {wanted} in 4:4:4 in WebRTC")
        } else {
            format!("this client does not decode {wanted} in WebRTC")
        };
        send(
            &mut socket,
            server_message::Message::Error(Error {
                message: message.clone(),
            }),
        )
        .await?;
        anyhow::bail!(message);
    }

    let (signals_tx, mut signals) = mpsc::unbounded_channel();
    let (requests, console) = (shared.requests.clone(), shared.console);
    let session = Session::start(
        shared.codec,
        shared.full_chroma,
        shared.viewers.clone(),
        shared.audio_viewers.clone(),
        (!shadow).then(|| shared.mic.clone()),
        signals_tx,
        move || {
            let _ = requests.send(Request::Keyframe { console });
        },
    )?;
    let user = (!shadow).then(|| shared.user.take_over());
    info!(shadow, "viewer session");
    let (clipboard, clip) = shared.clipboard.viewer();
    let (drives, drives_follow) = shared.drives.viewer();
    let (printers, printers_follow) = shared.printers.viewer();
    let mut state = ViewerState {
        input: ViewerInput::new(shared.input.clone()),
        clipboard,
        drives,
        printers,
    };
    let (user_id, taken_over) = user.unzip();
    let ended = run(
        &mut socket,
        &session,
        &mut signals,
        &mut state,
        Follow {
            pointer: shared.pointer.clone(),
            clip,
            drives: drives_follow,
            printers: printers_follow,
            taken_over,
        },
        &shared.layout,
        || {
            let _ = shared.requests.send(Request::Keyframe {
                console: shared.console,
            });
        },
    )
    .await;
    // Whatever ended the session, leave no key or button pressed in the guest, and the
    // clipboard to the guest.
    state.input.release_all().await?;
    state.clipboard.release().await?;
    state.drives.release().await;
    state.printers.release().await;
    if let Some(id) = user_id {
        shared.user.leave(id);
    }
    ended
}

/// What a viewer has of the guest: its keyboard and pointer, the clipboard, its drives and
/// printers.
struct ViewerState {
    input: ViewerInput,
    clipboard: ViewerClipboard,
    drives: ViewerDrives,
    printers: ViewerPrinters,
}

/// What a viewer follows besides its connection.
struct Follow {
    pointer: watch::Receiver<Pointer>,
    clip: ViewerFollow,
    drives: DrivesFollow,
    printers: PrintersFollow,
    /// For the user: another client took the session over. `None` for a shadow client.
    taken_over: Option<oneshot::Receiver<()>>,
}

fn send_all(session: &Session, channel: Channel, messages: Vec<Vec<u8>>) {
    for message in messages {
        session.send(channel, message);
    }
}

async fn run(
    socket: &mut WebSocket,
    session: &Session,
    signals: &mut mpsc::UnboundedReceiver<Signal>,
    state: &mut ViewerState,
    mut follow: Follow,
    layout: &Layout,
    keyframe: impl Fn(),
) -> Result<()> {
    // A shadow client only watches: what it sends to the guest is ignored, and it gets nothing
    // of the guest's clipboard.
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
    } = state;
    // Pointer updates wait for the pointer channel to open.
    let mut sent: Option<pointer::Sent> = None;
    let mut reception = tokio::time::interval(std::time::Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = reception.tick() => session.report_reception(),
            () = &mut taken_over => {
                let message = "another client took the session over".to_string();
                send(socket, server_message::Message::Error(Error { message: message.clone() })).await?;
                info!("{message}");
                return Ok(());
            }
            signal = signals.recv() => match signal {
                Some(Signal::Offer(sdp)) => {
                    send(socket, server_message::Message::Offer(SessionDescription { sdp })).await?
                }
                Some(Signal::IceCandidate { mline_index, candidate }) => {
                    let ice = IceCandidate { candidate, sdp_mline_index: mline_index };
                    send(socket, server_message::Message::Ice(ice)).await?
                }
                Some(Signal::Message(Channel::Input | Channel::Clipboard | Channel::Drive | Channel::Printer, _)) if !user => {}
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
                        if user && let Some(size) = monitors.first().and_then(Size::of) {
                            layout.want(size);
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
                    send_all(session, Channel::Pointer, fresh.update(&follow.pointer.borrow_and_update()));
                    sent = Some(fresh);
                }
                Some(Signal::Open(Channel::Clipboard)) if !user => {}
                Some(Signal::Open(Channel::Clipboard)) => {
                    let guest = follow.clip.guest.borrow_and_update().clone();
                    send_all(session, Channel::Clipboard, clipboard.opened(guest));
                }
                Some(Signal::Open(Channel::Input | Channel::Control | Channel::Drive | Channel::Printer)) => {}
                Some(Signal::Closed) | None => return Ok(()),
            },
            Ok(()) = follow.pointer.changed() => {
                if let Some(sent) = &mut sent {
                    send_all(session, Channel::Pointer, sent.update(&follow.pointer.borrow_and_update()));
                }
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
            message = receive(socket) => match message? {
                Some(client_message::Message::Answer(SessionDescription { sdp })) => session.apply_answer(&sdp)?,
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
