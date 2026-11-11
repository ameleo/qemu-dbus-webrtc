// SPDX-License-Identifier: MPL-2.0

//! The clipboard between the guest (through the guest agent) and the viewers.
//!
//! Whoever copied last owns the clipboard and announces the types it holds; the data move only
//! when someone pastes. When the guest copies, every viewer is told; when a viewer copies, the
//! guest is told, and asks that viewer for the data when an application pastes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use futures_util::StreamExt;
use qdw_protocol::v1::{
    ClipboardData, ClipboardGrab, ClipboardMessage, ClipboardRelease, ClipboardRequest,
    clipboard_message::Message,
};
use qdw_protocol::{MAX_CHUNK, MAX_CLIPBOARD, Message as _};
use qdw_qemu::agent::{self, ClipboardProxy, mime};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, warn};

/// How long the guest waits for a viewer's data before giving up on a paste.
const PASTE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long before trying the agent again, when it went away.
const AGENT_RETRY: Duration = Duration::from_secs(3);

/// The types that go between the guest and the viewers.
const SHARED: [&str; 2] = [mime::TEXT, mime::PNG];

fn shared(mimes: &[String]) -> Vec<String> {
    mimes
        .iter()
        .filter(|m| SHARED.contains(&m.as_str()))
        .cloned()
        .collect()
}

/// Clipboard data and its type, or nothing when the owner had nothing to give.
type Reply = oneshot::Sender<Option<(String, Vec<u8>)>>;

/// A paste in the guest, waiting for the data of the viewer that owns the clipboard.
pub struct Paste {
    mimes: Vec<String>,
    reply: Reply,
}

/// The viewer that copied last.
struct Owner {
    viewer: u64,
    serial: u32,
    pastes: mpsc::UnboundedSender<Paste>,
}

/// The clipboard state shared by the agent and the viewers.
pub struct Clipboard {
    /// The guest agent's clipboard, while the agent is connected.
    agent: Mutex<Option<ClipboardProxy<'static>>>,
    /// What the guest clipboard holds, among the shared types: empty for nothing.
    guest: watch::Sender<Vec<String>>,
    owner: Mutex<Option<Owner>>,
    serial: AtomicU32,
    viewers: AtomicU64,
}

impl Clipboard {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            agent: Mutex::new(None),
            guest: watch::Sender::new(Vec::new()),
            owner: Mutex::new(None),
            serial: AtomicU32::new(0),
            viewers: AtomicU64::new(0),
        })
    }

    fn agent(&self) -> Option<ClipboardProxy<'static>> {
        self.agent.lock().unwrap().clone()
    }

    /// Keep a connection to the guest agent, and follow the guest clipboard. Runs until the
    /// program ends; without an agent device on the VM, there is no clipboard.
    pub async fn run_agent(self: Arc<Self>, vm: zbus::Connection) {
        loop {
            match agent::connect(
                &vm,
                HostClipboard {
                    clipboard: self.clone(),
                },
            )
            .await
            {
                Ok(connection) => {
                    info!("guest agent connected");
                    if let Err(e) = self.follow(&connection).await {
                        warn!(error = %format!("{e:#}"), "guest agent");
                    }
                    info!("guest agent gone");
                    *self.agent.lock().unwrap() = None;
                    self.guest.send_replace(Vec::new());
                }
                Err(e) => {
                    let e = format!("{e:#}");
                    if e.contains("no fr.ameleo.agent.0") {
                        info!("no guest agent device on this VM: no clipboard");
                        return;
                    }
                    warn!(error = %e, "guest agent not reachable");
                }
            }
            tokio::time::sleep(AGENT_RETRY).await;
        }
    }

    async fn follow(&self, connection: &zbus::Connection) -> Result<()> {
        let proxy = ClipboardProxy::new(connection).await?;
        let mut grabbed = proxy.receive_grabbed().await?;
        let mut released = proxy.receive_released().await?;
        *self.agent.lock().unwrap() = Some(proxy);
        loop {
            tokio::select! {
                signal = grabbed.next() => {
                    let Some(signal) = signal else { return Ok(()) };
                    let mimes = shared(&signal.args()?.mimes);
                    debug!(?mimes, "guest copied");
                    // The guest owns the clipboard now, not a viewer.
                    *self.owner.lock().unwrap() = None;
                    self.guest.send_replace(mimes);
                }
                signal = released.next() => {
                    if signal.is_none() {
                        return Ok(());
                    }
                    self.guest.send_replace(Vec::new());
                }
            }
        }
    }

    /// A viewer's side of the clipboard, with what it must follow: the guest clipboard, and
    /// the pastes in the guest of what the viewer copied.
    pub fn viewer(
        self: &Arc<Self>,
    ) -> (
        ViewerClipboard,
        watch::Receiver<Vec<String>>,
        mpsc::UnboundedReceiver<Paste>,
    ) {
        let (pastes_tx, pastes) = mpsc::unbounded_channel();
        let viewer = ViewerClipboard {
            clipboard: self.clone(),
            id: self.viewers.fetch_add(1, Ordering::Relaxed),
            pastes_tx,
            waiting: HashMap::new(),
            next_request: 0,
            open: false,
        };
        (viewer, self.guest.subscribe(), pastes)
    }
}

/// `fr.ameleo.Host.Clipboard1`: the guest asks for the data of the clipboard a viewer owns.
struct HostClipboard {
    clipboard: Arc<Clipboard>,
}

#[zbus::interface(name = "fr.ameleo.Host.Clipboard1")]
impl HostClipboard {
    async fn request(
        &self,
        serial: u32,
        mimes: Vec<String>,
    ) -> zbus::fdo::Result<(String, Vec<u8>)> {
        debug!(serial, ?mimes, "guest pastes");
        let no_data = || zbus::fdo::Error::Failed("no clipboard data".into());
        let pastes = match &*self.clipboard.owner.lock().unwrap() {
            Some(owner) if owner.serial == serial => owner.pastes.clone(),
            _ => return Err(no_data()),
        };
        let (reply, answer) = oneshot::channel();
        pastes.send(Paste { mimes, reply }).map_err(|_| no_data())?;
        match tokio::time::timeout(PASTE_TIMEOUT, answer).await {
            Ok(Ok(Some(data))) => Ok(data),
            _ => Err(no_data()),
        }
    }
}

/// One viewer's side of the clipboard.
pub struct ViewerClipboard {
    clipboard: Arc<Clipboard>,
    id: u64,
    pastes_tx: mpsc::UnboundedSender<Paste>,
    /// Requests to the client waiting for its data, with what came so far.
    waiting: HashMap<u32, (Reply, Vec<u8>)>,
    next_request: u32,
    /// Whether the clipboard data channel is open.
    open: bool,
}

/// Messages to send to the client.
pub type Outgoing = Vec<Vec<u8>>;

fn encode(message: Message) -> Vec<u8> {
    ClipboardMessage {
        message: Some(message),
    }
    .encode_to_vec()
}

impl ViewerClipboard {
    /// The data channel opened: tell the client what the guest holds.
    pub fn opened(&mut self, guest: Vec<String>) -> Outgoing {
        self.open = true;
        self.guest_changed(guest)
    }

    /// The guest clipboard changed: tell the client.
    pub fn guest_changed(&mut self, mimes: Vec<String>) -> Outgoing {
        if !self.open {
            return Vec::new();
        }
        vec![encode(if mimes.is_empty() {
            Message::Release(ClipboardRelease {})
        } else {
            Message::Grab(ClipboardGrab { mimes })
        })]
    }

    /// The guest pastes what this viewer copied: ask the client for it.
    pub fn paste(&mut self, paste: Paste) -> Outgoing {
        let id = self.next_request;
        self.next_request = self.next_request.wrapping_add(1);
        self.waiting.insert(id, (paste.reply, Vec::new()));
        vec![encode(Message::Request(ClipboardRequest {
            id,
            mimes: paste.mimes,
        }))]
    }

    /// A message from the client.
    pub async fn message(&mut self, bytes: &[u8]) -> Result<Outgoing> {
        let Some(message) = ClipboardMessage::decode(bytes)?.message else {
            return Ok(Vec::new());
        };
        match message {
            Message::Grab(ClipboardGrab { mimes }) => {
                let mimes = shared(&mimes);
                let Some(agent) = self.clipboard.agent() else {
                    return Ok(Vec::new());
                };
                if mimes.is_empty() {
                    return Ok(Vec::new());
                }
                let serial = self
                    .clipboard
                    .serial
                    .fetch_add(1, Ordering::Relaxed)
                    .wrapping_add(1);
                *self.clipboard.owner.lock().unwrap() = Some(Owner {
                    viewer: self.id,
                    serial,
                    pastes: self.pastes_tx.clone(),
                });
                debug!(?mimes, "viewer copied");
                agent.grab(serial, mimes).await?;
            }
            Message::Release(_) => self.release().await?,
            Message::Request(ClipboardRequest { id, mimes }) => {
                let data = match self.clipboard.agent() {
                    Some(agent) => agent.request(shared(&mimes)).await.ok(),
                    None => None,
                };
                return Ok(answer(id, data.filter(|(_, d)| d.len() <= MAX_CLIPBOARD)));
            }
            Message::Data(ClipboardData {
                id,
                mime,
                data,
                last,
            }) => {
                let Some((_, received)) = self.waiting.get_mut(&id) else {
                    return Ok(Vec::new());
                };
                received.extend_from_slice(&data);
                if received.len() > MAX_CLIPBOARD {
                    if let Some((reply, _)) = self.waiting.remove(&id) {
                        let _ = reply.send(None);
                    }
                } else if last && let Some((reply, received)) = self.waiting.remove(&id) {
                    let _ = reply.send((!mime.is_empty()).then_some((mime, received)));
                }
            }
        }
        Ok(Vec::new())
    }

    /// The viewer gave up the clipboard, or went away.
    pub async fn release(&mut self) -> Result<()> {
        let owned = {
            let mut owner = self.clipboard.owner.lock().unwrap();
            let owned = owner.as_ref().is_some_and(|o| o.viewer == self.id);
            if owned {
                *owner = None;
            }
            owned
        };
        if owned && let Some(agent) = self.clipboard.agent() {
            agent.release().await?;
        }
        Ok(())
    }
}

/// The answer to a client's request, in chunks.
fn answer(id: u32, data: Option<(String, Vec<u8>)>) -> Outgoing {
    let Some((mime, data)) = data else {
        return vec![encode(Message::Data(ClipboardData {
            id,
            mime: String::new(),
            data: Vec::new(),
            last: true,
        }))];
    };
    let chunks: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(MAX_CHUNK).collect()
    };
    let count = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            encode(Message::Data(ClipboardData {
                id,
                mime: mime.clone(),
                data: chunk.to_vec(),
                last: i + 1 == count,
            }))
        })
        .collect()
}
