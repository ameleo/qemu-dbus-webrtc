// SPDX-License-Identifier: MPL-2.0

//! Client drives, shown in the guest as drive letters through the guest agent.
//!
//! A viewer announces its drives; the agent mounts each on a letter. Every file operation of a
//! guest application then comes to the server (`fr.ameleo.Host.Drive1`), which sends it to the
//! viewer that owns the drive and gives back its answer. When the viewer goes, its drives go
//! too; when the agent comes back (the guest restarted), the drives are mounted again.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use qdw_protocol::drive::{Reassembler, encode};
use qdw_protocol::v1::{
    DriveAdded, DriveEntry, DriveFile, DriveList, DriveMessage, DriveOpen, DriveRead, DriveRename,
    DriveReply, DriveRequest, DriveSetBasic, DriveSetDelete, DriveSetSize, DriveVolume, DriveWrite,
    drive_message::Message, drive_reply, drive_request::Operation,
};
use qdw_qemu::agent::{self, DriveProxy, drive};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

/// How long the guest waits for a viewer's answer to a file operation.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

/// NTSTATUS for a path the server refuses before asking the client.
const STATUS_OBJECT_NAME_INVALID: u32 = 0xC000_0033;

/// A guest path (`\` separated) as the viewers name it (`/` separated); `None` if it goes
/// through `..` or `.`, or has a `:` (a drive, or a Windows stream) or a control character in
/// it: no client is asked about such a path. A Windows client joining `C:/x` to its folder
/// would get `C:/x`.
fn client_path(guest: &str) -> Option<String> {
    if guest.chars().any(|c| c == ':' || c.is_control()) {
        return None;
    }
    let path = guest.trim_start_matches('\\').replace('\\', "/");
    let escapes = path.split('/').any(|c| c == ".." || c == ".");
    (!escapes).then_some(path)
}

/// A viewer path as the guest names it.
fn guest_path(client: &str) -> String {
    client.trim_start_matches('/').replace('/', "\\")
}

fn entry(e: DriveEntry) -> drive::Entry {
    (
        e.name,
        e.attributes,
        e.size,
        e.creation_time,
        e.last_access_time,
        e.last_write_time,
        e.change_time,
    )
}

/// A file operation for the viewer that owns a drive, waiting for its answer.
pub struct DriveCall {
    client_id: u32,
    operation: Operation,
    reply: oneshot::Sender<DriveReply>,
}

/// A drive shown in the guest.
struct Drive {
    viewer: u64,
    client_id: u32,
    name: String,
    label: String,
    calls: mpsc::UnboundedSender<DriveCall>,
    /// To tell the viewer its drive's letter, again when the agent comes back.
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
}

/// The drives of all viewers.
pub struct Drives {
    agent: Mutex<Option<DriveProxy<'static>>>,
    /// By the guest's id of the drive.
    drives: Mutex<HashMap<u32, Drive>>,
    next_id: AtomicU32,
    viewers: AtomicU64,
}

fn added(client_id: u32, result: std::result::Result<String, String>) -> Vec<Vec<u8>> {
    let (letter, error) = match result {
        Ok(letter) => (letter, String::new()),
        Err(error) => (String::new(), error),
    };
    encode(&DriveMessage {
        message: Some(Message::Added(DriveAdded {
            id: client_id,
            letter,
            error,
        })),
    })
}

fn send(outgoing: &mpsc::UnboundedSender<Vec<u8>>, messages: Vec<Vec<u8>>) {
    for message in messages {
        let _ = outgoing.send(message);
    }
}

impl Drives {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            agent: Mutex::new(None),
            drives: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            viewers: AtomicU64::new(0),
        })
    }

    fn agent(&self) -> Option<DriveProxy<'static>> {
        self.agent.lock().unwrap().clone()
    }

    /// Add the interface the guest calls for the drives' files to an agent connection.
    pub fn serve(
        self: &Arc<Self>,
        builder: zbus::connection::Builder<'static>,
    ) -> zbus::Result<zbus::connection::Builder<'static>> {
        builder.serve_at(
            agent::HOST_PATH,
            HostDrive {
                drives: self.clone(),
            },
        )
    }

    /// The agent is connected: mount the drives the viewers announced.
    pub async fn attach(&self, connection: &zbus::Connection) -> Result<()> {
        let proxy = DriveProxy::new(connection).await?;
        *self.agent.lock().unwrap() = Some(proxy.clone());
        let drives: Vec<_> = self
            .drives
            .lock()
            .unwrap()
            .iter()
            .map(|(&id, d)| {
                (
                    id,
                    d.client_id,
                    d.name.clone(),
                    d.label.clone(),
                    d.outgoing.clone(),
                )
            })
            .collect();
        for (id, client_id, name, label, outgoing) in drives {
            let result = proxy
                .add_drive(id, &name, &label)
                .await
                .map_err(|e| e.to_string());
            send(&outgoing, added(client_id, result));
        }
        Ok(())
    }

    /// The agent went away, its drives with it.
    pub fn detach(&self) {
        *self.agent.lock().unwrap() = None;
    }

    /// Send a file operation to the viewer that owns drive `id` (the guest's id), and wait for
    /// its answer. `None` if the viewer could not be asked or did not answer.
    async fn ask(&self, id: u32, operation: Operation) -> Option<DriveReply> {
        let (calls, client_id) = {
            let drives = self.drives.lock().unwrap();
            let drive = drives.get(&id)?;
            (drive.calls.clone(), drive.client_id)
        };
        let (reply, answer) = oneshot::channel();
        calls
            .send(DriveCall {
                client_id,
                operation,
                reply,
            })
            .ok()?;
        match tokio::time::timeout(ANSWER_TIMEOUT, answer).await {
            Ok(Ok(reply)) => Some(reply),
            Ok(Err(_)) => None,
            Err(_) => {
                warn!(
                    drive = id,
                    "the viewer did not answer a file operation in time"
                );
                None
            }
        }
    }

    /// A viewer's side of the drives, with what its loop follows.
    pub fn viewer(self: &Arc<Self>) -> (ViewerDrives, DrivesFollow) {
        let (calls_tx, calls) = mpsc::unbounded_channel();
        let (outgoing_tx, outgoing) = mpsc::unbounded_channel();
        let viewer = ViewerDrives {
            drives: self.clone(),
            id: self.viewers.fetch_add(1, Ordering::Relaxed),
            calls_tx,
            outgoing: outgoing_tx,
            waiting: HashMap::new(),
            next_request: 0,
            reassembler: Reassembler::default(),
        };
        (viewer, DrivesFollow { calls, outgoing })
    }
}

/// What a viewer's loop follows for the drives.
pub struct DrivesFollow {
    /// File operations for this viewer's drives.
    pub calls: mpsc::UnboundedReceiver<DriveCall>,
    /// Messages to send to the client, from work done apart.
    pub outgoing: mpsc::UnboundedReceiver<Vec<u8>>,
}

/// One viewer's side of the drives.
pub struct ViewerDrives {
    drives: Arc<Drives>,
    id: u64,
    calls_tx: mpsc::UnboundedSender<DriveCall>,
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
    /// Requests sent to the client, waiting for its reply.
    waiting: HashMap<u32, oneshot::Sender<DriveReply>>,
    next_request: u32,
    reassembler: Reassembler,
}

impl ViewerDrives {
    /// A file operation of the guest on one of this viewer's drives: send it to the client.
    pub fn call(&mut self, call: DriveCall) -> Vec<Vec<u8>> {
        let request_id = self.next_request;
        self.next_request = self.next_request.wrapping_add(1);
        self.waiting.insert(request_id, call.reply);
        encode(&DriveMessage {
            message: Some(Message::Request(DriveRequest {
                request_id,
                drive_id: call.client_id,
                operation: Some(call.operation),
            })),
        })
    }

    /// The guest's id of this viewer's drive `client_id`.
    fn guest_id(&self, client_id: u32) -> Option<u32> {
        let drives = self.drives.drives.lock().unwrap();
        drives
            .iter()
            .find(|(_, d)| d.viewer == self.id && d.client_id == client_id)
            .map(|(&id, _)| id)
    }

    /// A data channel message from the client.
    pub async fn message(&mut self, bytes: &[u8]) -> Result<()> {
        let Some(message) = self.reassembler.push(bytes)? else {
            return Ok(());
        };
        match message.message {
            Some(Message::Announce(announce)) => {
                self.announce(announce.id, announce.name, announce.label)
            }
            Some(Message::Remove(remove)) => {
                if let Some(id) = self.guest_id(remove.id) {
                    self.remove(id).await;
                }
            }
            Some(Message::Changed(changed)) => {
                if let (Some(id), Some(agent)) = (self.guest_id(changed.id), self.drives.agent()) {
                    let changes = changed
                        .changes
                        .into_iter()
                        .map(|c| (c.action, guest_path(&c.path)))
                        .collect();
                    agent.changed(id, changes).await?;
                }
            }
            Some(Message::Reply(reply)) => {
                if let Some(waiting) = self.waiting.remove(&reply.request_id) {
                    let _ = waiting.send(reply);
                }
            }
            // Server to client only, or empty.
            Some(Message::Added(_) | Message::Request(_) | Message::Fragment(_)) | None => {}
        }
        Ok(())
    }

    fn announce(&mut self, client_id: u32, name: String, label: String) {
        // Checked and added under one lock: two viewers announcing at once stay within the
        // limit.
        let added_as = {
            let mut drives = self.drives.drives.lock().unwrap();
            let announced = drives
                .values()
                .any(|d| d.viewer == self.id && d.client_id == client_id);
            if announced {
                Err("this drive id is announced already")
            } else if drives.len() >= drive::MAX_DRIVES {
                Err("too many drives")
            } else {
                let id = self.drives.next_id.fetch_add(1, Ordering::Relaxed);
                drives.insert(
                    id,
                    Drive {
                        viewer: self.id,
                        client_id,
                        name: name.clone(),
                        label: label.clone(),
                        calls: self.calls_tx.clone(),
                        outgoing: self.outgoing.clone(),
                    },
                );
                Ok(id)
            }
        };
        let id = match added_as {
            Ok(id) => id,
            Err(why) => {
                send(&self.outgoing, added(client_id, Err(why.into())));
                return;
            }
        };
        let agent = self.drives.agent();
        let outgoing = self.outgoing.clone();
        // Mounting takes a moment in the guest: apart from the viewer's loop.
        tokio::spawn(async move {
            let result = match agent {
                Some(agent) => agent
                    .add_drive(id, &name, &label)
                    .await
                    .map_err(|e| e.to_string()),
                None => Err("no guest agent: the drive is shown when it comes".into()),
            };
            match &result {
                Ok(letter) => info!(id, name, letter, "client drive shown in the guest"),
                Err(e) => warn!(id, name, error = e, "client drive not shown"),
            }
            send(&outgoing, added(client_id, result));
        });
    }

    async fn remove(&mut self, id: u32) {
        self.drives.drives.lock().unwrap().remove(&id);
        if let Some(agent) = self.drives.agent()
            && let Err(e) = agent.remove_drive(id).await
        {
            debug!(id, error = %e, "client drive not removed from the guest");
        }
    }

    /// The viewer went away: its drives leave the guest.
    pub async fn release(&mut self) {
        let ids: Vec<u32> = {
            let drives = self.drives.drives.lock().unwrap();
            drives
                .iter()
                .filter(|(_, d)| d.viewer == self.id)
                .map(|(&id, _)| id)
                .collect()
        };
        for id in ids {
            self.remove(id).await;
        }
    }
}

/// `fr.ameleo.Host.Drive1`: the guest's file operations on the client drives. Each answers the
/// client's NTSTATUS first; a D-Bus error means the client could not be asked.
struct HostDrive {
    drives: Arc<Drives>,
}

fn unreachable() -> zbus::fdo::Error {
    zbus::fdo::Error::Failed("the client of this drive could not be asked".into())
}

impl HostDrive {
    async fn ask(&self, drive_id: u32, operation: Operation) -> zbus::fdo::Result<DriveReply> {
        self.drives
            .ask(drive_id, operation)
            .await
            .ok_or_else(unreachable)
    }

    /// For the operations that answer a status only.
    async fn status(&self, drive_id: u32, operation: Operation) -> zbus::fdo::Result<u32> {
        Ok(self.ask(drive_id, operation).await?.status)
    }
}

#[zbus::interface(name = "fr.ameleo.Host.Drive1")]
impl HostDrive {
    #[allow(clippy::too_many_arguments)]
    async fn open(
        &self,
        drive_id: u32,
        path: &str,
        desired_access: u32,
        file_attributes: u32,
        share_access: u32,
        create_disposition: u32,
        create_options: u32,
    ) -> zbus::fdo::Result<(u32, u32, u32)> {
        let Some(path) = client_path(path) else {
            return Ok((STATUS_OBJECT_NAME_INVALID, 0, 0));
        };
        let open = DriveOpen {
            path,
            desired_access,
            file_attributes,
            share_access,
            create_disposition,
            create_options,
        };
        let reply = self.ask(drive_id, Operation::Open(open)).await?;
        let (file_id, information) = match reply.result {
            Some(drive_reply::Result::Opened(o)) => (o.file_id, o.information),
            _ => (0, 0),
        };
        Ok((reply.status, file_id, information))
    }

    async fn close(&self, drive_id: u32, file_id: u32) -> zbus::fdo::Result<u32> {
        self.status(drive_id, Operation::Close(DriveFile { file_id }))
            .await
    }

    async fn read(
        &self,
        drive_id: u32,
        file_id: u32,
        offset: u64,
        length: u32,
    ) -> zbus::fdo::Result<(u32, Vec<u8>)> {
        let length = length.min(drive::IO_MAX);
        let reply = self
            .ask(
                drive_id,
                Operation::Read(DriveRead {
                    file_id,
                    offset,
                    length,
                }),
            )
            .await?;
        let mut data = match reply.result {
            Some(drive_reply::Result::Data(data)) => data,
            _ => Vec::new(),
        };
        data.truncate(length as usize);
        Ok((reply.status, data))
    }

    async fn write(
        &self,
        drive_id: u32,
        file_id: u32,
        offset: u64,
        data: Vec<u8>,
    ) -> zbus::fdo::Result<(u32, u32)> {
        let sent = u32::try_from(data.len()).unwrap_or(u32::MAX);
        let reply = self
            .ask(
                drive_id,
                Operation::Write(DriveWrite {
                    file_id,
                    offset,
                    data,
                }),
            )
            .await?;
        let written = match reply.result {
            // At most what was sent.
            Some(drive_reply::Result::Written(written)) => written.min(sent),
            _ => 0,
        };
        Ok((reply.status, written))
    }

    async fn flush(&self, drive_id: u32, file_id: u32) -> zbus::fdo::Result<u32> {
        self.status(drive_id, Operation::Flush(DriveFile { file_id }))
            .await
    }

    async fn query(&self, drive_id: u32, file_id: u32) -> zbus::fdo::Result<(u32, drive::Entry)> {
        let reply = self
            .ask(drive_id, Operation::Query(DriveFile { file_id }))
            .await?;
        let found = match reply.result {
            Some(drive_reply::Result::Entry(e)) => entry(e),
            _ => entry(DriveEntry::default()),
        };
        Ok((reply.status, found))
    }

    async fn set_basic(
        &self,
        drive_id: u32,
        file_id: u32,
        attributes: u32,
        creation_time: u64,
        last_access_time: u64,
        last_write_time: u64,
    ) -> zbus::fdo::Result<u32> {
        let basic = DriveSetBasic {
            file_id,
            attributes,
            creation_time,
            last_access_time,
            last_write_time,
        };
        self.status(drive_id, Operation::SetBasic(basic)).await
    }

    async fn set_size(
        &self,
        drive_id: u32,
        file_id: u32,
        size: u64,
        allocation: bool,
    ) -> zbus::fdo::Result<u32> {
        self.status(
            drive_id,
            Operation::SetSize(DriveSetSize {
                file_id,
                size,
                allocation,
            }),
        )
        .await
    }

    async fn set_delete(
        &self,
        drive_id: u32,
        file_id: u32,
        delete: bool,
    ) -> zbus::fdo::Result<u32> {
        self.status(
            drive_id,
            Operation::SetDelete(DriveSetDelete { file_id, delete }),
        )
        .await
    }

    async fn rename(
        &self,
        drive_id: u32,
        file_id: u32,
        new_path: &str,
        replace: bool,
    ) -> zbus::fdo::Result<u32> {
        let Some(new_path) = client_path(new_path) else {
            return Ok(STATUS_OBJECT_NAME_INVALID);
        };
        self.status(
            drive_id,
            Operation::Rename(DriveRename {
                file_id,
                new_path,
                replace,
            }),
        )
        .await
    }

    async fn list(&self, drive_id: u32, path: &str) -> zbus::fdo::Result<(u32, Vec<drive::Entry>)> {
        let Some(path) = client_path(path) else {
            return Ok((STATUS_OBJECT_NAME_INVALID, Vec::new()));
        };
        let reply = self
            .ask(drive_id, Operation::List(DriveList { path }))
            .await?;
        let entries = match reply.result {
            Some(drive_reply::Result::Listing(listing)) => {
                listing.entries.into_iter().map(entry).collect()
            }
            _ => Vec::new(),
        };
        Ok((reply.status, entries))
    }

    async fn volume(&self, drive_id: u32) -> zbus::fdo::Result<(u32, u64, u64, String, String)> {
        let reply = self
            .ask(drive_id, Operation::Volume(DriveVolume {}))
            .await?;
        Ok(match reply.result {
            Some(drive_reply::Result::Volume(v)) => (
                reply.status,
                v.total_bytes,
                v.free_bytes,
                v.label,
                v.file_system,
            ),
            _ => (reply.status, 0, 0, String::new(), String::new()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives past the guest's letters are refused, and an id announced twice.
    #[tokio::test]
    async fn announcements_are_bounded() {
        let drives = Drives::new();
        let (mut viewer, _follow) = drives.viewer();
        let (mut other, mut follow) = drives.viewer();
        for id in 0..drive::MAX_DRIVES as u32 {
            viewer.announce(id, format!("d{id}"), String::new());
        }
        viewer.announce(0, "again".into(), String::new());
        other.announce(0, "late".into(), String::new());
        assert_eq!(drives.drives.lock().unwrap().len(), drive::MAX_DRIVES);
        // The refusal reaches the viewer that announced; the guest got no more drives.
        let refused = follow.outgoing.recv().await.unwrap();
        let refused = qdw_protocol::drive::Reassembler::default()
            .push(&refused)
            .unwrap()
            .unwrap();
        let Some(Message::Added(added)) = refused.message else {
            panic!("not an answer to an announcement");
        };
        assert_eq!((added.id, added.error.as_str()), (0, "too many drives"));
        other.release().await;
        viewer.release().await;
        assert!(drives.drives.lock().unwrap().is_empty());
    }

    #[test]
    fn paths() {
        assert_eq!(
            client_path("Docs\\été\\a.txt").as_deref(),
            Some("Docs/été/a.txt")
        );
        assert_eq!(client_path("").as_deref(), Some(""));
        assert_eq!(client_path("\\Docs").as_deref(), Some("Docs"));
        assert_eq!(client_path("Docs\\..\\..\\secret"), None);
        assert_eq!(client_path(".\\x"), None);
        assert_eq!(client_path("C:\\Windows"), None);
        assert_eq!(client_path("a.txt:secret"), None);
        assert_eq!(client_path("a\u{0}b"), None);
        assert_eq!(guest_path("Docs/a.txt"), "Docs\\a.txt");
    }
}
