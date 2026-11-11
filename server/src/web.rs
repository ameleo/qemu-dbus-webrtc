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
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use qdw_protocol::v1::{
    ClientMessage, ControlMessage, Error, Hello, IceCandidate, Input, ServerMessage,
    SessionDescription, VideoPause, client_message, control_message, server_message,
};
use qdw_protocol::{Message, SIGNALLING_PATH};
use qdw_video::encode::Codec;

use std::sync::Arc;

use crate::audio::Mic;
use crate::clipboard::{Clipboard, ViewerClipboard, ViewerFollow};
use crate::input::ViewerInput;
use crate::pointer::{self, Pointer};
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
    let Some(client_message::Message::Hello(Hello { codecs })) = receive(&mut socket).await? else {
        anyhow::bail!("no hello");
    };
    let wanted = shared.codec.mime_type();
    if !codecs.iter().any(|c| c.eq_ignore_ascii_case(wanted)) {
        let message = format!("this client does not decode {wanted} in WebRTC");
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
        shared.mic.clone(),
        signals_tx,
        move || {
            let _ = requests.send(Request::Keyframe { console });
        },
    )?;
    let mut input = ViewerInput::new(shared.input.clone());
    let (mut clipboard, clip) = shared.clipboard.viewer();
    let ended = run(
        &mut socket,
        &session,
        &mut signals,
        &mut input,
        &mut clipboard,
        Follow {
            pointer: shared.pointer.clone(),
            clip,
        },
        || {
            let _ = shared.requests.send(Request::Keyframe {
                console: shared.console,
            });
        },
    )
    .await;
    // Whatever ended the session, leave no key or button pressed in the guest, and the
    // clipboard to the guest.
    input.release_all().await?;
    clipboard.release().await?;
    ended
}

/// What a viewer follows besides its connection.
struct Follow {
    pointer: watch::Receiver<Pointer>,
    clip: ViewerFollow,
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
    input: &mut ViewerInput,
    clipboard: &mut ViewerClipboard,
    mut follow: Follow,
    keyframe: impl Fn(),
) -> Result<()> {
    // Pointer updates wait for the pointer channel to open.
    let mut sent: Option<pointer::Sent> = None;
    loop {
        tokio::select! {
            signal = signals.recv() => match signal {
                Some(Signal::Offer(sdp)) => {
                    send(socket, server_message::Message::Offer(SessionDescription { sdp })).await?
                }
                Some(Signal::IceCandidate { mline_index, candidate }) => {
                    let ice = IceCandidate { candidate, sdp_mline_index: mline_index };
                    send(socket, server_message::Message::Ice(ice)).await?
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
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "undecodable control message"),
                },
                Some(Signal::Message(Channel::Pointer, _)) => {}
                Some(Signal::Open(Channel::Pointer)) => {
                    let mut fresh = pointer::Sent::default();
                    send_all(session, Channel::Pointer, fresh.update(&follow.pointer.borrow_and_update()));
                    sent = Some(fresh);
                }
                Some(Signal::Open(Channel::Clipboard)) => {
                    let guest = follow.clip.guest.borrow_and_update().clone();
                    send_all(session, Channel::Clipboard, clipboard.opened(guest));
                }
                Some(Signal::Open(Channel::Input | Channel::Control)) => {}
                Some(Signal::Closed) | None => return Ok(()),
            },
            Ok(()) = follow.pointer.changed() => {
                if let Some(sent) = &mut sent {
                    send_all(session, Channel::Pointer, sent.update(&follow.pointer.borrow_and_update()));
                }
            }
            Ok(()) = follow.clip.guest.changed() => {
                let guest = follow.clip.guest.borrow_and_update().clone();
                send_all(session, Channel::Clipboard, clipboard.guest_changed(guest));
            }
            Some(call) = follow.clip.calls.recv() => {
                send_all(session, Channel::Clipboard, clipboard.host_call(call));
            }
            Some(message) = follow.clip.outgoing.recv() => session.send(Channel::Clipboard, message),
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
