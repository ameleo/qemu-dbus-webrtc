// SPDX-License-Identifier: MPL-2.0

//! The clipboard between the guest (through the guest agent) and the viewers.
//!
//! Whoever copied last owns the clipboard and announces the types it holds; the data move only
//! when someone pastes. When the guest copies, every viewer is told; when a viewer copies, the
//! guest is told, and asks that viewer for the data when an application pastes.
//!
//! Files and directories go as RDP sends them: the owner gives the file list, then the contents
//! of each file piece by piece, on request; a long paste locks the content while it reads. The
//! agent speaks the Windows format of the list (CLIPRDR_FILELIST), the viewers our
//! ClipboardFileList: the server translates.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use futures_util::StreamExt;
use qdw_protocol::v1::{
    ClipboardData, ClipboardFile, ClipboardFileData, ClipboardFileList, ClipboardFileRequest,
    ClipboardGrab, ClipboardLock, ClipboardMessage, ClipboardRelease, ClipboardRequest,
    ClipboardUnlock, clipboard_message::Message,
};
use qdw_protocol::{FILE_LIST_MIME, MAX_CHUNK, MAX_CLIPBOARD, MAX_FILE_PIECE, Message as _};
use qdw_qemu::agent::{self, ClipboardProxy, FileClipboardProxy, file_contents, mime};
use qdw_qemu::files::{self, File};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, warn};

/// How long the guest waits for a viewer's data before giving up on a paste.
const PASTE_TIMEOUT: Duration = Duration::from_secs(10);

/// The types that go between the guest and the viewers: the agent's name, ours.
const SHARED: [(&str, &str); 3] = [
    (mime::TEXT, mime::TEXT),
    (mime::PNG, mime::PNG),
    (mime::FILE_LIST, FILE_LIST_MIME),
];

/// The agent's types, as the viewers know them.
fn from_agent(mimes: &[String]) -> Vec<String> {
    let known = |m: &String| {
        SHARED
            .iter()
            .find(|(a, _)| a == m)
            .map(|(_, ours)| ours.to_string())
    };
    mimes.iter().filter_map(known).collect()
}

/// The viewers' types, as the agent knows them.
fn to_agent(mimes: &[String]) -> Vec<String> {
    let known = |m: &String| {
        SHARED
            .iter()
            .find(|(_, o)| o == m)
            .map(|(agent, _)| agent.to_string())
    };
    mimes.iter().filter_map(known).collect()
}

/// The guest agent's clipboard interfaces, while it is connected.
#[derive(Clone)]
struct Agent {
    clipboard: ClipboardProxy<'static>,
    files: FileClipboardProxy<'static>,
}

/// Clipboard data and its type (the agent's), or nothing when the owner had nothing to give.
type DataReply = oneshot::Sender<Option<(String, Vec<u8>)>>;

/// A file's size (8 bytes, little endian, as the agent returns it) or piece, or nothing.
type FileReply = oneshot::Sender<Option<Vec<u8>>>;

/// What the guest asks of the viewer that owns the clipboard.
pub enum HostCall {
    /// The data, in the first of these types (the agent's) the viewer has.
    Data {
        mimes: Vec<String>,
        reply: DataReply,
    },
    /// The size or a piece of a file of the list.
    File {
        request: FilePiece,
        reply: FileReply,
    },
    Lock(u32),
    Unlock(u32),
}

/// A file request, as the agent and RDP make it.
pub struct FilePiece {
    index: u32,
    size: bool,
    offset: u64,
    length: u32,
    clip_data_id: Option<u32>,
}

/// The viewer that copied last.
struct Owner {
    viewer: u64,
    serial: u32,
    calls: mpsc::UnboundedSender<HostCall>,
}

impl Owner {
    /// Ask the owner, and wait for the answer.
    async fn ask<T>(
        calls: mpsc::UnboundedSender<HostCall>,
        call: impl FnOnce(oneshot::Sender<Option<T>>) -> HostCall,
    ) -> Option<T> {
        let (reply, answer) = oneshot::channel();
        if calls.send(call(reply)).is_err() {
            warn!("the viewer that owns the clipboard is gone");
            return None;
        }
        match tokio::time::timeout(PASTE_TIMEOUT, answer).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => {
                warn!("the request to the viewer was dropped");
                None
            }
            Err(_) => {
                warn!("the viewer did not answer in time");
                None
            }
        }
    }
}

/// The clipboard state shared by the agent and the viewers.
pub struct Clipboard {
    agent: Mutex<Option<Agent>>,
    /// What the guest clipboard holds, as the viewers name the types: empty for nothing.
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

    fn agent(&self) -> Option<Agent> {
        self.agent.lock().unwrap().clone()
    }

    /// The calls channel of the viewer that owns the clipboard under `serial` (any serial when
    /// `None`).
    fn owner_calls(&self, serial: Option<u32>) -> Option<mpsc::UnboundedSender<HostCall>> {
        match &*self.owner.lock().unwrap() {
            Some(owner) if serial.is_none_or(|s| s == owner.serial) => Some(owner.calls.clone()),
            _ => None,
        }
    }

    /// Add the interfaces the guest calls back for the clipboard to an agent connection.
    pub fn serve(
        self: &Arc<Self>,
        builder: zbus::connection::Builder<'static>,
    ) -> zbus::Result<zbus::connection::Builder<'static>> {
        builder
            .serve_at(
                agent::HOST_PATH,
                HostClipboard {
                    clipboard: self.clone(),
                },
            )?
            .serve_at(
                agent::HOST_PATH,
                HostFileClipboard {
                    clipboard: self.clone(),
                },
            )
    }

    /// The agent went away: no guest clipboard until it comes back.
    pub fn detach(&self) {
        *self.agent.lock().unwrap() = None;
        self.guest.send_replace(Vec::new());
    }

    /// Follow the guest clipboard through the agent connection, until it closes.
    pub async fn follow(&self, connection: &zbus::Connection) -> Result<()> {
        let clipboard = ClipboardProxy::new(connection).await?;
        let files = FileClipboardProxy::new(connection).await?;
        let mut grabbed = clipboard.receive_grabbed().await?;
        let mut released = clipboard.receive_released().await?;
        *self.agent.lock().unwrap() = Some(Agent { clipboard, files });
        loop {
            tokio::select! {
                signal = grabbed.next() => {
                    let Some(signal) = signal else { return Ok(()) };
                    // A malformed signal is the agent's fault: skipped, the clipboard still
                    // followed.
                    let mimes = match signal.args() {
                        Ok(args) => from_agent(&args.mimes),
                        Err(e) => {
                            warn!(error = %e, "malformed clipboard grab from the agent");
                            continue;
                        }
                    };
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

    /// A viewer's side of the clipboard, with what its loop follows.
    pub fn viewer(self: &Arc<Self>) -> (ViewerClipboard, ViewerFollow) {
        let (calls_tx, calls) = mpsc::unbounded_channel();
        let (outgoing_tx, outgoing) = mpsc::unbounded_channel();
        let viewer = ViewerClipboard {
            clipboard: self.clone(),
            id: self.viewers.fetch_add(1, Ordering::Relaxed),
            calls_tx,
            outgoing: outgoing_tx,
            data: HashMap::new(),
            files: HashMap::new(),
            file_index: None,
            locks: std::collections::HashSet::new(),
            next_request: 0,
            open: false,
        };
        (
            viewer,
            ViewerFollow {
                guest: self.guest.subscribe(),
                calls,
                outgoing,
            },
        )
    }
}

/// What a viewer's loop follows for the clipboard.
pub struct ViewerFollow {
    /// What the guest clipboard holds.
    pub guest: watch::Receiver<Vec<String>>,
    /// What the guest asks of this viewer, when it owns the clipboard.
    pub calls: mpsc::UnboundedReceiver<HostCall>,
    /// Messages to send to the client, from work done apart.
    pub outgoing: mpsc::UnboundedReceiver<Vec<u8>>,
}

fn no_data() -> zbus::fdo::Error {
    zbus::fdo::Error::Failed("no clipboard data".into())
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
        let Some(calls) = self.clipboard.owner_calls(Some(serial)) else {
            debug!(serial, "guest pastes from no viewer's clipboard");
            return Err(no_data());
        };
        let data = Owner::ask(calls, |reply| HostCall::Data { mimes, reply }).await;
        match &data {
            Some((mime, data)) => debug!(
                serial,
                mime,
                bytes = data.len(),
                "viewer gave the guest its data"
            ),
            None => warn!(
                serial,
                "the viewer gave the guest no data (none, refused, or too late)"
            ),
        }
        data.ok_or_else(no_data)
    }
}

/// `fr.ameleo.Host.FileClipboard1`: the guest reads the files a viewer copied.
struct HostFileClipboard {
    clipboard: Arc<Clipboard>,
}

#[zbus::interface(name = "fr.ameleo.Host.FileClipboard1")]
impl HostFileClipboard {
    async fn lock_clip_data(&self, clip_data_id: u32) -> zbus::fdo::Result<()> {
        debug!(clip_data_id, "guest locks the viewer's clipboard");
        if let Some(calls) = self.clipboard.owner_calls(None) {
            let _ = calls.send(HostCall::Lock(clip_data_id));
        }
        Ok(())
    }

    async fn unlock_clip_data(&self, clip_data_id: u32) -> zbus::fdo::Result<()> {
        debug!(clip_data_id, "guest unlocks the viewer's clipboard");
        if let Some(calls) = self.clipboard.owner_calls(None) {
            let _ = calls.send(HostCall::Unlock(clip_data_id));
        }
        Ok(())
    }

    async fn file_contents(
        &self,
        lindex: i32,
        flags: u32,
        position: u64,
        requested: u32,
        clip_data_id: u32,
        has_clip_data_id: bool,
    ) -> zbus::fdo::Result<Vec<u8>> {
        debug!(
            lindex,
            flags,
            position,
            requested,
            clip_data_id,
            has_clip_data_id,
            "guest reads a viewer's file"
        );
        let Some(calls) = self.clipboard.owner_calls(None) else {
            warn!(
                lindex,
                "guest reads a file, but no viewer owns the clipboard"
            );
            return Err(no_data());
        };
        let request = FilePiece {
            index: u32::try_from(lindex).map_err(|_| no_data())?,
            size: flags & file_contents::SIZE != 0,
            offset: position,
            length: requested.min(MAX_FILE_PIECE),
            clip_data_id: has_clip_data_id.then_some(clip_data_id),
        };
        let piece = Owner::ask(calls, |reply| HostCall::File { request, reply }).await;
        match &piece {
            Some(piece) => debug!(
                lindex,
                bytes = piece.len(),
                "viewer gave the guest its file"
            ),
            None => warn!(
                lindex,
                position, "the viewer gave no file contents (failed, or too late)"
            ),
        }
        piece.ok_or_else(no_data)
    }
}

/// A request to the client, waiting for its answer, with what came so far.
struct Waiting<R> {
    reply: R,
    received: Vec<u8>,
    /// For a file request: whether it asked for the size.
    size: bool,
    /// The most it may give: a range answer is at most the length asked (MS-RDPECLIP 2.2.5.3,
    /// cbRequested), a size answer carries none.
    most: usize,
}

/// One viewer's side of the clipboard.
pub struct ViewerClipboard {
    clipboard: Arc<Clipboard>,
    id: u64,
    calls_tx: mpsc::UnboundedSender<HostCall>,
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
    data: HashMap<u32, Waiting<DataReply>>,
    files: HashMap<u32, Waiting<FileReply>>,
    /// For each file of the list the guest got from this viewer, its index in the viewer's;
    /// `None` until the guest got one.
    file_index: Option<Vec<u32>>,
    /// The clipboard contents the guest locked on this viewer, by id.
    locks: std::collections::HashSet<u32>,
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
    fn request_id(&mut self) -> u32 {
        let id = self.next_request;
        self.next_request = self.next_request.wrapping_add(1);
        id
    }

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

    /// The guest asks for what this viewer copied: ask the client.
    pub fn host_call(&mut self, call: HostCall) -> Outgoing {
        let message = match call {
            HostCall::Data { mimes, reply } => {
                let id = self.request_id();
                self.data.insert(
                    id,
                    Waiting {
                        reply,
                        received: Vec::new(),
                        size: false,
                        most: MAX_CLIPBOARD,
                    },
                );
                Message::Request(ClipboardRequest {
                    id,
                    mimes: from_agent(&mimes),
                })
            }
            HostCall::File { mut request, reply } => {
                // The guest's index, in the list it got, to the viewer's: a file of no list the
                // guest got is none of the viewer's.
                match self
                    .file_index
                    .as_ref()
                    .and_then(|kept| kept.get(request.index as usize))
                {
                    Some(&index) => request.index = index,
                    None => {
                        warn!(
                            index = request.index,
                            "guest reads a file of no list it got"
                        );
                        let _ = reply.send(None);
                        return Vec::new();
                    }
                }
                let id = self.request_id();
                debug!(
                    id,
                    index = request.index,
                    size = request.size,
                    offset = request.offset,
                    length = request.length,
                    "file request to the viewer"
                );
                self.files.insert(
                    id,
                    Waiting {
                        reply,
                        received: Vec::new(),
                        size: request.size,
                        most: if request.size {
                            0
                        } else {
                            request.length as usize
                        },
                    },
                );
                Message::FileRequest(ClipboardFileRequest {
                    id,
                    index: request.index,
                    size: request.size,
                    offset: request.offset,
                    length: request.length,
                    // A lock the guest never took here means nothing to the client: ask for the
                    // current content. (The Windows agent may cite one it took on its side.)
                    clip_data_id: request.clip_data_id.filter(|id| self.locks.contains(id)),
                })
            }
            HostCall::Lock(clip_data_id) => {
                self.locks.insert(clip_data_id);
                Message::Lock(ClipboardLock { clip_data_id })
            }
            HostCall::Unlock(clip_data_id) => {
                self.locks.remove(&clip_data_id);
                Message::Unlock(ClipboardUnlock { clip_data_id })
            }
        };
        vec![encode(message)]
    }

    /// A message from the client. What takes time (reading the guest clipboard) runs apart and
    /// sends its answer through the outgoing channel.
    pub async fn message(&mut self, bytes: &[u8]) -> Result<Outgoing> {
        let Some(message) = ClipboardMessage::decode(bytes)?.message else {
            return Ok(Vec::new());
        };
        match message {
            Message::Grab(ClipboardGrab { mimes }) => {
                let mimes = to_agent(&mimes);
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
                    calls: self.calls_tx.clone(),
                });
                debug!(?mimes, "viewer copied");
                agent.clipboard.grab(serial, mimes).await?;
            }
            Message::Release(_) => self.release().await?,
            Message::Request(ClipboardRequest { id, mimes }) => {
                let agent = self.clipboard.agent();
                let outgoing = self.outgoing.clone();
                tokio::spawn(async move {
                    let data = match agent {
                        Some(agent) => guest_data(&agent, &mimes).await,
                        None => None,
                    };
                    for message in answer(id, data) {
                        let _ = outgoing.send(message);
                    }
                });
            }
            Message::Lock(ClipboardLock { clip_data_id }) => {
                if let Some(agent) = self.clipboard.agent() {
                    agent.files.lock_clip_data(clip_data_id).await?;
                }
            }
            Message::Unlock(ClipboardUnlock { clip_data_id }) => {
                if let Some(agent) = self.clipboard.agent() {
                    agent.files.unlock_clip_data(clip_data_id).await?;
                }
            }
            Message::FileRequest(request) => {
                let agent = self.clipboard.agent();
                let outgoing = self.outgoing.clone();
                tokio::spawn(async move {
                    let messages = match agent {
                        Some(agent) => guest_file(&agent, &request).await,
                        None => file_failed(request.id),
                    };
                    for message in messages {
                        let _ = outgoing.send(message);
                    }
                });
            }
            Message::Data(ClipboardData {
                id,
                mime,
                data,
                last,
            }) => {
                let Some(waiting) = self.data.get_mut(&id) else {
                    return Ok(Vec::new());
                };
                waiting.received.extend_from_slice(&data);
                if waiting.received.len() > waiting.most {
                    if let Some(waiting) = self.data.remove(&id) {
                        let _ = waiting.reply.send(None);
                    }
                } else if last && let Some(waiting) = self.data.remove(&id) {
                    let converted = client_data(&mime, waiting.received);
                    if let Some((_, _, Some(kept))) = &converted {
                        self.file_index = Some(kept.clone());
                    }
                    let _ = waiting
                        .reply
                        .send(converted.map(|(mime, data, _)| (mime, data)));
                }
            }
            Message::FileData(ClipboardFileData {
                id,
                size,
                data,
                last,
                failed,
            }) => {
                debug!(
                    id,
                    size,
                    bytes = data.len(),
                    last,
                    failed,
                    "file answer from the viewer"
                );
                let Some(waiting) = self.files.get_mut(&id) else {
                    warn!(id, "file answer to no request");
                    return Ok(Vec::new());
                };
                waiting.received.extend_from_slice(&data);
                if failed || waiting.received.len() > waiting.most {
                    if !failed {
                        warn!(id, "the viewer gave more than asked: refused");
                    }
                    if let Some(waiting) = self.files.remove(&id) {
                        let _ = waiting.reply.send(None);
                    }
                } else if last && let Some(waiting) = self.files.remove(&id) {
                    // A size goes to the agent as 8 bytes, little endian.
                    let answer = if waiting.size {
                        size.to_le_bytes().to_vec()
                    } else {
                        waiting.received
                    };
                    let _ = waiting.reply.send(Some(answer));
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
            agent.clipboard.release().await?;
        }
        Ok(())
    }
}

/// The guest clipboard's data, in the first of `mimes` (ours) it holds, as the viewers take it.
async fn guest_data(agent: &Agent, mimes: &[String]) -> Option<(String, Vec<u8>)> {
    let (mime, data) = agent.clipboard.request(to_agent(mimes)).await.ok()?;
    if data.len() > MAX_CLIPBOARD {
        return None;
    }
    if mime == mime::FILE_LIST {
        let files = files::parse(&data)?.into_iter().map(to_message).collect();
        return Some((
            FILE_LIST_MIME.to_string(),
            ClipboardFileList { files }.encode_to_vec(),
        ));
    }
    Some((mime, data))
}

/// A viewer's data for the guest, in the agent's type and format.
/// For a file list, also which of the viewer's files the guest got, in its order: the guest
/// names files by their index in the list it got.
fn client_data(mime: &str, data: Vec<u8>) -> Option<(String, Vec<u8>, Option<Vec<u32>>)> {
    match mime {
        "" => None,
        FILE_LIST_MIME => {
            let list = match ClipboardFileList::decode(data.as_slice()) {
                Ok(list) => list,
                Err(e) => {
                    warn!(error = %e, "undecodable file list from the viewer");
                    return None;
                }
            };
            let files: Vec<File> = list.files.into_iter().map(from_message).collect();
            debug!(count = files.len(), first = ?files.first(), "viewer's file list");
            let Some(built) = files::build(&files) else {
                warn!(
                    ?files,
                    "file list refused: a path is empty, absolute, or has . or .. in it"
                );
                return None;
            };
            if built.kept.len() < files.len() {
                let left_out: Vec<&str> = files
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !built.kept.contains(i))
                    .map(|(_, f)| f.path.as_str())
                    .collect();
                warn!(
                    ?left_out,
                    "paths Windows cannot take (over 259 characters, or with \\, : or a control \
                     character), left out of the copy"
                );
            }
            let kept = built.kept.iter().map(|&i| i as u32).collect();
            Some((mime::FILE_LIST.to_string(), built.list, Some(kept)))
        }
        _ => Some((to_agent(&[mime.to_string()]).pop()?, data, None)),
    }
}

fn to_message(file: File) -> ClipboardFile {
    ClipboardFile {
        path: file.path,
        directory: file.directory,
        size: file.size,
        modified_ms: file.modified_ms,
    }
}

fn from_message(file: ClipboardFile) -> File {
    File {
        path: file.path,
        directory: file.directory,
        size: file.size,
        modified_ms: file.modified_ms,
    }
}

/// The size or a piece of a file the guest copied, as answers to the client.
async fn guest_file(agent: &Agent, request: &ClipboardFileRequest) -> Outgoing {
    let read = async {
        let index = i32::try_from(request.index)?;
        // MS-RDPECLIP 2.2.5.3: a size request asks for 8 bytes at position 0.
        let (flags, offset, length) = if request.size {
            (file_contents::SIZE, 0, 8)
        } else {
            let length = request
                .length
                .min(MAX_FILE_PIECE)
                .min(file_contents::MAX_RANGE);
            (file_contents::RANGE, request.offset, length)
        };
        let data = agent
            .files
            .file_contents(
                index,
                flags,
                offset,
                length,
                request.clip_data_id.unwrap_or_default(),
                request.clip_data_id.is_some(),
            )
            .await?;
        if request.size && data.len() != 8 {
            bail!("a file size of {} bytes", data.len());
        }
        // At most the length asked (MS-RDPECLIP 2.2.5.3).
        if data.len() > length as usize {
            bail!("{} bytes of a file, {length} asked", data.len());
        }
        anyhow::Ok(data)
    };
    let id = request.id;
    match read.await {
        Ok(size) if request.size => {
            let size = u64::from_le_bytes(size.try_into().unwrap_or_default());
            vec![encode(Message::FileData(ClipboardFileData {
                id,
                size,
                data: Vec::new(),
                last: true,
                failed: false,
            }))]
        }
        Ok(data) => chunks(&data)
            .map(|(chunk, last)| {
                encode(Message::FileData(ClipboardFileData {
                    id,
                    size: 0,
                    data: chunk.to_vec(),
                    last,
                    failed: false,
                }))
            })
            .collect(),
        Err(e) => {
            debug!(error = %e, "guest file not read");
            file_failed(id)
        }
    }
}

fn file_failed(id: u32) -> Outgoing {
    vec![encode(Message::FileData(ClipboardFileData {
        id,
        size: 0,
        data: Vec::new(),
        last: true,
        failed: true,
    }))]
}

/// `data` in chunks a data channel message carries, each with whether it is the last one; one
/// empty chunk for no data.
fn chunks(data: &[u8]) -> impl Iterator<Item = (&[u8], bool)> {
    let count = data.len().div_ceil(MAX_CHUNK).max(1);
    (0..count).map(move |i| {
        (
            &data[(i * MAX_CHUNK).min(data.len())..((i + 1) * MAX_CHUNK).min(data.len())],
            i + 1 == count,
        )
    })
}

/// The answer to a client's request, in chunks.
fn answer(id: u32, data: Option<(String, Vec<u8>)>) -> Outgoing {
    let (mime, data) = data.unwrap_or_default();
    chunks(&data)
        .map(|(chunk, last)| {
            encode(Message::Data(ClipboardData {
                id,
                mime: mime.clone(),
                data: chunk.to_vec(),
                last,
            }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunking() {
        let pieces = |n: usize| {
            chunks(&vec![7; n])
                .map(|(c, last)| (c.len(), last))
                .collect::<Vec<_>>()
        };
        assert_eq!(pieces(0), [(0, true)]);
        assert_eq!(pieces(10), [(10, true)]);
        assert_eq!(pieces(MAX_CHUNK), [(MAX_CHUNK, true)]);
        assert_eq!(pieces(MAX_CHUNK + 1), [(MAX_CHUNK, false), (1, true)]);
    }

    #[test]
    fn types_translate() {
        let ours = vec![
            FILE_LIST_MIME.to_string(),
            "text/html".to_string(),
            mime::TEXT.to_string(),
        ];
        assert_eq!(to_agent(&ours), [mime::FILE_LIST, mime::TEXT]);
        assert_eq!(from_agent(&to_agent(&ours)), [FILE_LIST_MIME, mime::TEXT]);
    }

    /// The guest reads files by their index in the list it got, mapped to the viewer's, and a
    /// viewer's answer is at most what was asked.
    #[tokio::test]
    async fn file_reads_are_checked() {
        let clipboard = Clipboard::new();
        let (mut viewer, _follow) = clipboard.viewer();
        let read = |viewer: &mut ViewerClipboard, index, length| {
            let (reply, answer) = oneshot::channel();
            let request = FilePiece {
                index,
                size: false,
                offset: 0,
                length,
                clip_data_id: None,
            };
            let sent = viewer.host_call(HostCall::File { request, reply });
            let sent = sent.into_iter().next().map(|m| {
                match ClipboardMessage::decode(m.as_slice()).unwrap().message {
                    Some(Message::FileRequest(r)) => (r.id, r.index),
                    _ => panic!("not a file request"),
                }
            });
            (sent, answer)
        };
        // No list given to the guest: no file of the viewer's.
        let (sent, mut answer) = read(&mut viewer, 0, 4);
        assert_eq!(sent, None);
        assert_eq!(answer.try_recv().unwrap(), None);
        // The guest gets a list, with the second file left out (too long a path for Windows).
        let file = |path: String| ClipboardFile {
            path,
            directory: false,
            size: 4,
            modified_ms: 0,
        };
        let list = ClipboardFileList {
            files: vec![file("a".into()), file("b".repeat(300)), file("c".into())],
        };
        let (reply, mut got) = oneshot::channel();
        let request = viewer.host_call(HostCall::Data {
            mimes: vec![mime::FILE_LIST.into()],
            reply,
        });
        let Some(Message::Request(ClipboardRequest { id, .. })) =
            ClipboardMessage::decode(request[0].as_slice())
                .unwrap()
                .message
        else {
            panic!("not a data request");
        };
        let data = encode(Message::Data(ClipboardData {
            id,
            mime: FILE_LIST_MIME.into(),
            data: list.encode_to_vec(),
            last: true,
        }));
        viewer.message(&data).await.unwrap();
        assert!(got.try_recv().unwrap().is_some());
        // The guest's second file is the viewer's third; it has no third.
        let (sent, _) = read(&mut viewer, 1, 4);
        assert_eq!(sent.map(|(_, index)| index), Some(2));
        assert_eq!(read(&mut viewer, 2, 4).0, None);
        // An answer longer than asked is refused, one as long is taken.
        let answer_with = |id, bytes: usize| {
            encode(Message::FileData(ClipboardFileData {
                id,
                size: 0,
                data: vec![0; bytes],
                last: true,
                failed: false,
            }))
        };
        let (sent, mut answer) = read(&mut viewer, 0, 4);
        viewer
            .message(&answer_with(sent.unwrap().0, 5))
            .await
            .unwrap();
        assert_eq!(answer.try_recv().unwrap(), None);
        let (sent, mut answer) = read(&mut viewer, 0, 4);
        viewer
            .message(&answer_with(sent.unwrap().0, 4))
            .await
            .unwrap();
        assert_eq!(answer.try_recv().unwrap(), Some(vec![0; 4]));
    }

    /// A file list from a viewer reaches the agent in the Windows format, and back.
    #[test]
    fn file_list_both_ways() {
        let list = ClipboardFileList {
            files: vec![
                ClipboardFile {
                    path: "dir".into(),
                    directory: true,
                    size: 0,
                    modified_ms: 0,
                },
                ClipboardFile {
                    path: "dir/a.txt".into(),
                    directory: false,
                    size: 3,
                    modified_ms: 1_762_000_000_000,
                },
            ],
        };
        let (mime, windows, kept) = client_data(FILE_LIST_MIME, list.encode_to_vec()).unwrap();
        assert_eq!(kept, Some(vec![0, 1]));
        assert_eq!(mime, mime::FILE_LIST);
        let back: Vec<ClipboardFile> = files::parse(&windows)
            .unwrap()
            .into_iter()
            .map(to_message)
            .collect();
        assert_eq!(back, list.files);
    }
}
