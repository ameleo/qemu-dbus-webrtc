// SPDX-License-Identifier: MPL-2.0

//! A client's signalling, over its WebSocket or relayed by the broker: its hello, its account
//! checked, the VM found and attached to, then its session.

use anyhow::Result;
use axum::extract::ws::{Message as WsMessage, WebSocket};
use tokio::sync::watch;
use tracing::{Instrument, info, warn};

use qdw_protocol::Message;
use qdw_protocol::v1::{ClientMessage, Error, ServerMessage, client_message, server_message};

use super::lock::Departure;
use super::viewer::{Viewer, viewer};
use crate::config::Auth;
use crate::vms::Vms;

/// The error that asks a client for an account: the page shows its login form on it.
const ACCOUNT_REQUIRED: &str = "an account is required";

/// How long a client has to say hello, its account in it: the page and the native client say
/// it as soon as the WebSocket opens, and a connection that says nothing is not kept.
const HELLO_TIME: std::time::Duration = std::time::Duration::from_secs(10);
/// How long a wrong account waits for its answer: guessing passwords takes that long each.
const REFUSAL_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

/// Who decides that a client may open a VM.
pub enum Admission<'a> {
    /// This server: its accounts, the client's in its hello.
    Local {
        auth: &'a Auth,
        checks: &'a tokio::sync::Semaphore,
    },
    /// The broker that relays the connection, which checked the user's sign-in and their
    /// access to the VM it names.
    Broker {
        /// The user's name, for the logs.
        user: String,
        /// The user's stable identifier, "<issuer>|<subject>".
        id: String,
        /// View-only, as the broker says; the client may ask it too.
        shadow: bool,
    },
}

/// A client came for the VM `id` (a UUID or a name): its hello, its account checked, whether
/// it decodes the video, then the VM, found, started or attached to if need be, and the
/// session.
pub async fn open(
    mut socket: Socket,
    vms: &Vms,
    admission: Admission<'_>,
    id: &str,
    stopping: watch::Receiver<bool>,
) -> Result<()> {
    let hello = match tokio::time::timeout(HELLO_TIME, receive(&mut socket)).await {
        Ok(Ok(Some(client_message::Message::Hello(hello)))) => hello,
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(e)) => return Err(e),
        Ok(Ok(Some(_))) | Err(_) => anyhow::bail!("no hello"),
    };
    // Who the user is, to tell one person from another when the session changes hands: the
    // broker's identifier, the account's user; nobody in particular without accounts.
    // Each kind its own, so that an account's user never matches a broker's identifier; empty
    // for nobody in particular, who always finds the guest's session locked.
    let identity = match &admission {
        Admission::Broker { id, .. } if !id.is_empty() => format!("broker:{id}"),
        Admission::Local { .. } if !hello.user.is_empty() => format!("account:{}", hello.user),
        _ => String::new(),
    };
    let mut shadow = hello.shadow;
    // The VMs the client may open: all without accounts, those of its account otherwise; the one
    // the broker names, for a connection it relays.
    let grant = match admission {
        Admission::Broker {
            user,
            shadow: view_only,
            ..
        } => {
            shadow |= view_only;
            info!(id, user, shadow, "relayed by the broker");
            crate::host::access::Grant::Only(vec![id.to_owned()])
        }
        Admission::Local {
            auth: Auth::None, ..
        } => crate::host::access::Grant::All,
        Admission::Local {
            auth: Auth::Password(accounts),
            checks,
        } => {
            // No account given: the client asks for one, with no delay (nothing is guessed).
            if hello.user.is_empty() {
                refuse(&mut socket, ACCOUNT_REQUIRED).await;
                return Ok(());
            }
            let (accounts, user, password) =
                (accounts.clone(), hello.user.clone(), hello.password.clone());
            // Argon2 takes its time and memory: off the async threads, a few at once.
            let granted = {
                let _turn = checks.acquire().await?;
                tokio::task::spawn_blocking(move || {
                    accounts.check(&user, &password).map(|a| a.grant.clone())
                })
                .await?
            };
            let Some(grant) = granted else {
                info!(id, user = hello.user, "wrong user or password");
                tokio::time::sleep(REFUSAL_DELAY).await;
                refuse(&mut socket, "wrong user or password").await;
                return Ok(());
            };
            info!(id, user = hello.user, "logged in");
            grant
        }
    };
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
    // A VM the account may not open is one the client does not know of: the same answer as for
    // a VM the host does not run, and the VM is neither started nor attached to.
    if !granted(vms, &grant, uuid).await {
        info!(id, uuid, user = hello.user, "VM not granted to the user");
        refuse(&mut socket, "no such VM on this host").await;
        return Ok(());
    }
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
    let who = Viewer {
        codecs: hello.codecs,
        shadow,
        identity,
    };
    let mut server_stopping = stopping.clone();
    // The user's session, then the lock after it: the server waits for both, should it stop
    // meanwhile.
    let pending = (!shadow).then(|| vms.pending_lock());
    let (ended, user) = viewer(socket, &client, who, stopping)
        .instrument(span.clone())
        .await;
    let departure = {
        let _entered = span.enter();
        match &ended {
            Ok(departure) => {
                info!(?departure, "viewer left");
                *departure
            }
            Err(e) => {
                warn!(error = %format!("{e:#}"), "viewer session ended");
                Departure::Lost
            }
        }
    };
    // A client that may come back keeps the VM attached a while.
    if departure != Departure::Lost {
        client.leaves();
    }
    // The user gone, the guest's session is locked, as the configuration says.
    let Some(user) = user else {
        return Ok(());
    };
    let shared = client.shared().clone();
    let Some(delay) = shared.locker.policy.after(departure) else {
        return Ok(());
    };
    let lock = async move {
        if !delay.is_zero() {
            // Later, unless the user comes back before; at once if the server stops meanwhile.
            // The VM stays attached until then.
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                _ = server_stopping.wait_for(|stopping| *stopping) => {}
            }
        }
        if !*shared.vm_stopped.borrow() {
            let locker = &shared.locker;
            shared.user.lock_after(user, || locker.lock()).await;
        }
        drop(client);
        drop(pending);
    }
    .instrument(span);
    if delay.is_zero() {
        lock.await;
    } else {
        tokio::spawn(lock);
    }
    Ok(())
}

/// Whether `grant` names the VM `uuid`: "*", its UUID, or a name the VM goes by now. A name
/// that names no VM, or several, names none.
async fn granted(vms: &Vms, grant: &crate::host::access::Grant, uuid: &str) -> bool {
    let crate::host::access::Grant::Only(names) = grant else {
        return true;
    };
    for name in names {
        if vms.uuid_of(name).await.ok().flatten().as_deref() == Some(uuid) {
            return true;
        }
    }
    false
}

/// A client's signalling: its WebSocket, or a connection the broker relays (`host/broker.rs`),
/// which carries the same messages.
pub enum Socket {
    Ws(WebSocket),
    Relayed(crate::host::broker::Relayed),
}

impl Socket {
    async fn send_binary(&mut self, bytes: Vec<u8>) -> Result<()> {
        match self {
            Self::Ws(socket) => socket.send(WsMessage::Binary(bytes.into())).await?,
            Self::Relayed(relayed) => relayed.send(bytes).await?,
        }
        Ok(())
    }

    /// The next binary message; `None` when the client has gone.
    async fn receive_binary(&mut self) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Ws(socket) => loop {
                match socket.recv().await {
                    None | Some(Ok(WsMessage::Close(_))) => return Ok(None),
                    Some(Ok(WsMessage::Binary(bytes))) => return Ok(Some(bytes.into())),
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return Err(e.into()),
                }
            },
            Self::Relayed(relayed) => relayed.receive().await,
        }
    }

    async fn close(&mut self, code: u16) {
        match self {
            Self::Ws(socket) => {
                let frame = axum::extract::ws::CloseFrame {
                    code,
                    reason: "".into(),
                };
                let _ = socket.send(WsMessage::Close(Some(frame))).await;
            }
            Self::Relayed(relayed) => relayed.close(code).await,
        }
    }
}

/// Close the WebSocket cleanly, with the status `code` (RFC 6455 7.4.1): 1000 for an end the
/// session meant (the client leaves, the VM stops, a refusal its Error explains), 1001 when
/// the server goes away, 1011 when it failed. Whether the close frame went or not, the
/// session is over.
pub(crate) async fn close(socket: &mut Socket, code: u16) {
    socket.close(code).await;
}

/// Refuse the client, saying why (an Error), then close: the end is a normal one (1000), the
/// refusal its reason.
pub(crate) async fn refuse(socket: &mut Socket, message: &str) {
    let message = message.to_owned();
    let _ = send(socket, server_message::Message::Error(Error { message })).await;
    close(socket, axum::extract::ws::close_code::NORMAL).await;
}

/// One message to the client, as a binary WebSocket message.
pub(crate) async fn send(socket: &mut Socket, message: server_message::Message) -> Result<()> {
    let bytes = ServerMessage {
        message: Some(message),
    }
    .encode_to_vec();
    socket.send_binary(bytes).await
}

/// The next message from the client; `None` when it has gone.
pub(crate) async fn receive(socket: &mut Socket) -> Result<Option<client_message::Message>> {
    loop {
        let Some(bytes) = socket.receive_binary().await? else {
            return Ok(None);
        };
        let message = ClientMessage::decode(bytes.as_slice())?.message;
        // An empty message is one of a newer client that this server does not know.
        if message.is_some() {
            return Ok(message);
        }
    }
}
