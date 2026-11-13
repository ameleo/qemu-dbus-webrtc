// SPDX-License-Identifier: MPL-2.0

//! The web side: the viewer page, and the WebSocket that carries the WebRTC signalling.
//!
//! The signalling messages are those of `qdw-protocol`, in Protocol Buffers, one per binary
//! WebSocket message.

use std::collections::HashMap;
use std::net::SocketAddr;

use std::path::PathBuf;

use anyhow::{Context, Result};
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{Instrument, debug, info, warn};

use qdw_protocol::Message;
use qdw_protocol::v1::{
    BandwidthEstimate, ClientMessage, ControlMessage, Ended, Error, IceCandidate, Input,
    Microphone, MonitorLayout, ServerMessage, SessionDescription, VideoPause, client_message,
    control_message, ended, server_message,
};
use qdw_video::encode::Codec;

use std::sync::Arc;

use crate::audio::Mic;
use crate::cameras::{Cameras, CamerasFollow, ViewerCameras};
use crate::clipboard::{Clipboard, ViewerClipboard, ViewerFollow};
use crate::config::Auth;
use crate::drives::{Drives, DrivesFollow, ViewerDrives};
use crate::input::{AgentMouse, ViewerInput};
use crate::layout::{Layout, Size};
use crate::pointer::{self, Pointer};
use crate::printers::{Printers, PrintersFollow, ViewerPrinters};
use crate::session::{AudioViewers, Bitrates, Channel, Session, Signal, Viewers};
use crate::vms::{Client, Vms};

const PAGE: &str = include_str!("../web/index.html");

/// A request from a viewer to the display loop.
#[derive(Debug)]
pub enum Request {
    /// The viewer needs a keyframe of this console.
    Keyframe { console: u32 },
    /// The user's connection takes this video bitrate, in kbit/s, for all the screens.
    Bitrate { bitrate: u32 },
    /// Whether every viewer decodes the video in 10 bits.
    TenBits(bool),
}

#[derive(Clone)]
pub struct Shared {
    /// Turns true when the VM stops: its viewers' sessions end.
    pub vm_stopped: watch::Receiver<bool>,
    pub codec: Codec,
    /// The video is in 4:4:4.
    pub full_chroma: bool,
    /// The guest's screens, in order: what the viewers get, one video track each.
    pub screens: Vec<Screen>,
    /// The viewers of the guest's sound.
    pub audio_viewers: AudioViewers,
    /// The guest's microphone.
    pub mic: Arc<Mic>,
    pub requests: mpsc::UnboundedSender<Request>,
    /// The keyboard, and the QEMU tablet of the first screen.
    pub input: qdw_qemu::input::Input,
    /// The pointer through the guest agent, on every screen.
    pub agent_mouse: AgentMouse,
    /// The guest pointer on each screen.
    pub pointer: watch::Receiver<Vec<Pointer>>,
    pub clipboard: Arc<Clipboard>,
    pub drives: Arc<Drives>,
    pub printers: Arc<Printers>,
    pub cameras: Arc<Cameras>,
    /// The size and place of the guest's screens.
    pub layout: Arc<Layout>,
    /// The user of the session.
    pub user: Arc<User>,
    /// The bounds of the bandwidth estimation.
    pub bitrates: Bitrates,
    /// Which viewers decode the video in 10 bits.
    pub depths: Arc<Depths>,
}

/// A guest screen: its QEMU console, its head (the virtio-gpu scanout), and the viewers its
/// encoder feeds.
#[derive(Clone)]
pub struct Screen {
    pub console: u32,
    pub head: u32,
    pub viewers: Viewers,
}

/// Which viewers decode the video in 10 bits. The video is shared: it is in 10 bits only when
/// they all do.
#[derive(Default)]
pub struct Depths {
    viewers: std::sync::Mutex<HashMap<u64, bool>>,
    next: std::sync::atomic::AtomicU64,
}

impl Depths {
    /// Every viewer decodes 10 bits, and there is one at least.
    fn all_deep(viewers: &HashMap<u64, bool>) -> bool {
        !viewers.is_empty() && viewers.values().all(|&deep| deep)
    }

    /// A viewer comes: its id, and whether the video may be in 10 bits now.
    fn join(&self, deep: bool) -> (u64, bool) {
        let id = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut viewers = self.viewers.lock().unwrap();
        viewers.insert(id, deep);
        (id, Self::all_deep(&viewers))
    }

    /// A viewer goes: whether the video may be in 10 bits now.
    fn leave(&self, id: u64) -> bool {
        let mut viewers = self.viewers.lock().unwrap();
        viewers.remove(&id);
        Self::all_deep(&viewers)
    }
}

/// The one client that uses the session; the others are shadow clients, which only watch.
#[derive(Default)]
pub struct User {
    /// Its id, and how to end its session when another takes it over.
    current: std::sync::Mutex<Option<(u64, oneshot::Sender<()>)>>,
    next: std::sync::atomic::AtomicU64,
    /// Counts the users that left (not those taken over): the shadow clients, who watch the
    /// user, go with it.
    left: watch::Sender<u64>,
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
            self.left.send_modify(|left| *left += 1);
        }
    }

    /// Changes when the user leaves: the shadow sessions end.
    fn left(&self) -> watch::Receiver<u64> {
        self.left.subscribe()
    }
}

/// A certificate and its private key, PEM files.
#[derive(Debug, Clone)]
pub struct Tls {
    pub certificate: PathBuf,
    pub key: PathBuf,
}

/// Serve the VMs' pages and their signalling on `listen` until the program ends, in HTTPS
/// with `tls`: never in plain HTTP, which would carry the session in clear (and browsers give
/// the clipboard, the microphone and the camera to pages served in HTTPS only). A VM's page is
/// `/vm/<uuid>/`, its signalling `/vm/<uuid>/signal` (`qdw_protocol::signalling_path`).
pub async fn serve(listen: SocketAddr, tls: Tls, vms: Arc<Vms>, auth: Auth) -> Result<()> {
    let state = AppState {
        vms,
        auth,
        checks: Arc::new(tokio::sync::Semaphore::new(CHECKS_AT_ONCE)),
    };
    let app =
        axum::Router::new()
            .route(
                "/vm/{uuid}",
                get(|Path(uuid): Path<String>| async move {
                    Redirect::permanent(&format!("/vm/{uuid}/"))
                }),
            )
            .route("/vm/{uuid}/", get(page))
            .route("/vm/{uuid}/signal", get(upgrade))
            .with_state(state)
            .into_make_service_with_connect_info::<SocketAddr>();
    // One crypto provider for the process: ring.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.certificate, &tls.key)
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
    Ok(())
}

/// A VM's page, by its UUID or its name. The same page whatever the VM, whether it exists or
/// not: which VMs the host runs is said only to a client that logged in (on the signalling).
/// With the headers that keep it from being framed by another site (its keys go to the guest),
/// and that let it run its own script and style only, and talk to this server only.
async fn page() -> impl IntoResponse {
    use axum::http::header;
    (
        [
            (header::CONTENT_SECURITY_POLICY, page_policy()),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Html(PAGE),
    )
}

/// The page's Content Security Policy: its inline script and style only, by their SHA-256
/// (CSP 3), the guest pointer's images as data URLs, connections to this server only (the
/// signalling), and no framing.
fn page_policy() -> &'static str {
    static POLICY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    POLICY.get_or_init(|| {
        let digest = |open: &str, close: &str| {
            let start = PAGE.find(open).map(|i| i + open.len()).unwrap_or(0);
            let end = PAGE[start..].find(close).map_or(start, |i| start + i);
            let hash = ring::digest::digest(&ring::digest::SHA256, &PAGE.as_bytes()[start..end]);
            use base64::Engine;
            format!(
                "'sha256-{}'",
                base64::engine::general_purpose::STANDARD.encode(hash.as_ref())
            )
        };
        format!(
            "default-src 'none'; script-src {script}; style-src {style}; img-src data:; \
             connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
            script = digest("<script>", "</script>"),
            style = digest("<style>", "</style>"),
        )
    })
}

/// Whether a WebSocket upgrade comes from this server's own page, or from no page: a browser
/// says the page's origin, which must be this server's; a native client says none. Another
/// site's page, in the user's browser, must not reach the VMs (cross-site WebSocket hijacking).
fn same_origin(headers: &axum::http::HeaderMap) -> bool {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        return true;
    };
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok());
    match (origin.to_str().ok(), host) {
        (Some(origin), Some(host)) => origin
            .strip_prefix("https://")
            .is_some_and(|o| o.eq_ignore_ascii_case(host)),
        _ => false,
    }
}

/// What the pages and the signalling share.
#[derive(Clone)]
struct AppState {
    vms: Arc<Vms>,
    auth: Auth,
    /// Password checks at once: each takes 64 MiB and a CPU for a while (Argon2), so that many
    /// logins at once, an attacker's, wait their turn rather than exhaust the host.
    checks: Arc<tokio::sync::Semaphore>,
}

/// How many password checks run at once.
const CHECKS_AT_ONCE: usize = 2;

/// The error that asks a client for an account: the page shows its login form on it.
const ACCOUNT_REQUIRED: &str = "an account is required";

/// How long a client has to say hello, its account in it: the page and the native client say
/// it as soon as the WebSocket opens, and a connection that says nothing is not kept.
const HELLO_TIME: std::time::Duration = std::time::Duration::from_secs(10);
/// The longest VM id taken: a UUID, or a VM's name.
const MAX_ID: usize = 255;
/// How long a wrong account waits for its answer: guessing passwords takes that long each.
const REFUSAL_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

async fn upgrade(
    ws: WebSocketUpgrade,
    headers: axum::http::HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
    State(AppState { vms, auth, checks }): State<AppState>,
) -> Response {
    if !same_origin(&headers) {
        warn!(%peer, origin = ?headers.get(axum::http::header::ORIGIN), "WebSocket from another site refused");
        return (StatusCode::FORBIDDEN, "not from this server's page").into_response();
    }
    // Which VM is asked only once the client logged in: before, the answer would tell anyone
    // which VMs the host runs. The id must look like one, a UUID or a name.
    if id.is_empty() || id.len() > MAX_ID || id.contains(['/', '\\', '\0']) {
        return (StatusCode::NOT_FOUND, "no such VM on this host").into_response();
    }
    let stopping = vms.stopping();
    ws.on_upgrade(move |socket| async move {
        info!(%peer, id, "viewer connected");
        if let Err(e) = open(socket, &vms, &auth, &checks, &id, stopping).await {
            warn!(%peer, id, error = %format!("{e:#}"), "viewer session ended");
        }
    })
    .into_response()
}

/// A client came for the VM `id` (a UUID or a name): its hello, its account checked, whether
/// it decodes the video, then the VM, found, started or attached to if need be, and the
/// session.
async fn open(
    mut socket: WebSocket,
    vms: &Vms,
    auth: &Auth,
    checks: &tokio::sync::Semaphore,
    id: &str,
    stopping: watch::Receiver<bool>,
) -> Result<()> {
    let hello = match tokio::time::timeout(HELLO_TIME, receive(&mut socket)).await {
        Ok(Ok(Some(client_message::Message::Hello(hello)))) => hello,
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(e)) => return Err(e),
        Ok(Ok(Some(_))) | Err(_) => anyhow::bail!("no hello"),
    };
    let shadow = hello.shadow;
    match auth {
        Auth::None => {}
        Auth::Password(expected) => {
            // No account given: the client asks for one, with no delay (nothing is guessed).
            if hello.user.is_empty() {
                refuse(&mut socket, ACCOUNT_REQUIRED).await;
                return Ok(());
            }
            let (expected, user, password) =
                (expected.clone(), hello.user.clone(), hello.password.clone());
            // Argon2 takes its time and memory: off the async threads, a few at once.
            let granted = {
                let _turn = checks.acquire().await?;
                tokio::task::spawn_blocking(move || expected.check(&user, &password)).await?
            };
            if !granted {
                info!(id, user = hello.user, "wrong user or password");
                tokio::time::sleep(REFUSAL_DELAY).await;
                refuse(&mut socket, "wrong user or password").await;
                return Ok(());
            }
            info!(id, user = hello.user, "logged in");
        }
    }
    // A client that does not decode the video is told so before the VM starts for it.
    let config = vms.config();
    let (codec, full_chroma) = (config.settings.codec, config.settings.full_chroma);
    if !hello
        .codecs
        .iter()
        .any(|c| codec.decoded_by(full_chroma, c))
    {
        let wanted = codec.mime_type();
        let message = if full_chroma {
            format!("this client does not decode {wanted} in 4:4:4 in WebRTC")
        } else {
            format!("this client does not decode {wanted} in WebRTC")
        };
        info!(id, message, "client refused");
        refuse(&mut socket, &message).await;
        return Ok(());
    }
    // Which VM, now the client may know.
    let uuid = match vms.uuid_of(id).await {
        Ok(Some(uuid)) => uuid,
        Ok(None) => {
            refuse(&mut socket, "no such VM on this host").await;
            return Ok(());
        }
        Err(e) => {
            refuse(&mut socket, &format!("{e:#}")).await;
            return Ok(());
        }
    };
    let uuid = uuid.as_str();
    // The VM now: started if it is off and its manager may, attached if need be.
    let attached = match vms.client(uuid).await {
        Ok(Some(client)) => Ok(client),
        Ok(None) => Err("no such VM on this host".to_string()),
        Err(e) => Err(format!("{e:#}")),
    };
    let mut client = match attached {
        Ok(client) => client,
        Err(message) => {
            warn!(uuid, error = message, "VM not attached");
            refuse(&mut socket, &message).await;
            return Ok(());
        }
    };
    let span = client.span().clone();
    let codecs = hello.codecs;
    let ended = viewer(socket, &client, codecs, shadow, stopping)
        .instrument(span.clone())
        .await;
    let _entered = span.enter();
    match &ended {
        Ok(()) => {
            info!("viewer left");
            client.leaves();
        }
        Err(e) => warn!(error = %format!("{e:#}"), "viewer session ended"),
    }
    Ok(())
}

/// Close the WebSocket cleanly, with the status `code` (RFC 6455 7.4.1): 1000 for an end the
/// session meant (the client leaves, the VM stops, a refusal its Error explains), 1001 when
/// the server goes away, 1011 when it failed. Whether the close frame went or not, the
/// session is over.
async fn close(socket: &mut WebSocket, code: u16) {
    let frame = axum::extract::ws::CloseFrame {
        code,
        reason: "".into(),
    };
    let _ = socket.send(WsMessage::Close(Some(frame))).await;
}

/// Refuse the client, saying why (an Error), then close: the end is a normal one (1000), the
/// refusal its reason.
async fn refuse(socket: &mut WebSocket, message: &str) {
    let message = message.to_owned();
    let _ = send(socket, server_message::Message::Error(Error { message })).await;
    close(socket, axum::extract::ws::close_code::NORMAL).await;
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

/// The session of a client of the VM: `codecs` it decodes, from its hello, and whether it
/// only watches.
async fn viewer(
    mut socket: WebSocket,
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
    let session = Session::start(
        shared.codec,
        shared.full_chroma,
        shared.screens.iter().map(|s| s.viewers.clone()).collect(),
        shared.audio_viewers.clone(),
        (!shadow).then(|| shared.mic.clone()),
        shared.bitrates,
        signals_tx,
        move |screen| {
            if let Some(&console) = consoles.get(screen) {
                let _ = requests.send(Request::Keyframe { console });
            }
        },
    )?;
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

/// The video bitrate for a bandwidth estimate, in kbit/s: what the sound leaves.
fn video_bitrate(estimate: u32) -> u32 {
    estimate.saturating_sub(AUDIO_BITRATE).max(1)
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
    socket: &mut WebSocket,
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
                    send(socket, server_message::Message::Offer(SessionDescription { sdp })).await?
                }
                Some(Signal::IceCandidate { mline_index, candidate }) => {
                    let ice = IceCandidate { candidate, sdp_mline_index: mline_index };
                    send(socket, server_message::Message::Ice(ice)).await?
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
                    send_all(session, Channel::Pointer, fresh.update(&follow.pointer.borrow_and_update()));
                    sent = Some(fresh);
                }
                Some(Signal::Open(Channel::Clipboard)) if !user => {}
                Some(Signal::Open(Channel::Clipboard)) => {
                    let guest = follow.clip.guest.borrow_and_update().clone();
                    send_all(session, Channel::Clipboard, clipboard.opened(guest));
                }
                Some(Signal::Open(Channel::Control)) => {
                    control_open = true;
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
                    send_all(session, Channel::Pointer, sent.update(&follow.pointer.borrow_and_update()));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websockets_from_this_server_or_no_page() {
        use axum::http::{HeaderMap, HeaderValue, header};
        let headers = |origin: Option<&str>| {
            let mut h = HeaderMap::new();
            h.insert(
                header::HOST,
                HeaderValue::from_static("vdi.example.com:8090"),
            );
            if let Some(origin) = origin {
                h.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
            }
            h
        };
        assert!(same_origin(&headers(None)));
        assert!(same_origin(&headers(Some("https://vdi.example.com:8090"))));
        assert!(same_origin(&headers(Some("https://VDI.example.com:8090"))));
        assert!(!same_origin(&headers(Some("https://evil.example.org"))));
        assert!(!same_origin(&headers(Some("http://vdi.example.com:8090"))));
        assert!(!same_origin(&headers(Some("https://vdi.example.com:8091"))));
        assert!(!same_origin(&headers(Some("null"))));
    }

    #[test]
    fn the_page_runs_its_own_script_and_style_only() {
        let policy = page_policy();
        assert!(policy.contains("script-src 'sha256-"), "{policy}");
        assert!(policy.contains("style-src 'sha256-"), "{policy}");
        assert!(policy.contains("frame-ancestors 'none'"));
        assert!(!policy.contains("unsafe"));
    }

    #[test]
    fn ten_bits_when_every_viewer_decodes_them() {
        let depths = Depths::default();
        let (deep, ten_bits) = depths.join(true);
        assert!(ten_bits);
        let (shallow, ten_bits) = depths.join(false);
        assert!(!ten_bits);
        assert!(depths.leave(shallow));
        assert!(!depths.leave(deep));
    }
}
