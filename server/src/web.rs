// SPDX-License-Identifier: MPL-2.0

//! The web side: the viewer page, and the WebSocket that carries the WebRTC signalling.
//!
//! The signalling messages are those of `qdw-protocol`, in Protocol Buffers, one per binary
//! WebSocket message.

use std::net::SocketAddr;

use anyhow::Result;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

use qdw_protocol::v1::{
    ClientMessage, Error, Hello, IceCandidate, Input, ServerMessage, SessionDescription,
    client_message, server_message,
};
use qdw_protocol::{Message, SIGNALLING_PATH};
use qdw_video::encode::Codec;

use crate::input::ViewerInput;
use crate::pointer::{self, Pointer};
use crate::session::{Session, Signal, Viewers};

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
    /// The console shown to viewers, and its viewers.
    pub console: u32,
    pub viewers: Viewers,
    pub requests: mpsc::UnboundedSender<Request>,
    /// The keyboard and pointer of the shown console.
    pub input: qdw_qemu::input::Input,
    /// The guest pointer of the shown console.
    pub pointer: watch::Receiver<Pointer>,
}

/// Serve the page and the signalling on `listen` until the program ends.
pub async fn serve(listen: SocketAddr, shared: Shared) -> Result<()> {
    let app = axum::Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route(SIGNALLING_PATH, get(upgrade))
        .with_state(shared);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    info!(%listen, "serving the viewer page");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
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
        shared.viewers.clone(),
        signals_tx,
        move || {
            let _ = requests.send(Request::Keyframe { console });
        },
    )?;
    let mut input = ViewerInput::new(shared.input.clone());
    let ended = run(
        &mut socket,
        &session,
        &mut signals,
        &mut input,
        shared.pointer.clone(),
    )
    .await;
    // Whatever ended the session, leave no key or button pressed in the guest.
    input.release_all().await?;
    ended
}

async fn run(
    socket: &mut WebSocket,
    session: &Session,
    signals: &mut mpsc::UnboundedReceiver<Signal>,
    input: &mut ViewerInput,
    mut pointer: watch::Receiver<Pointer>,
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
                Some(Signal::Input(bytes)) => match Input::decode(bytes.as_slice()) {
                    Ok(events) => input.apply(events).await?,
                    Err(e) => warn!(error = %e, "undecodable input"),
                },
                Some(Signal::PointerOpen) => {
                    let mut fresh = pointer::Sent::default();
                    for message in fresh.update(&pointer.borrow_and_update()) {
                        session.send_pointer(message);
                    }
                    sent = Some(fresh);
                }
                Some(Signal::Closed) | None => return Ok(()),
            },
            Ok(()) = pointer.changed() => {
                if let Some(sent) = &mut sent {
                    for message in sent.update(&pointer.borrow_and_update()) {
                        session.send_pointer(message);
                    }
                }
            }
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
