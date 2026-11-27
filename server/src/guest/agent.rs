// SPDX-License-Identifier: MPL-2.0

//! The connection to the guest agent, which the clipboard, the drives, the printers and the
//! cameras go through.

use std::sync::Arc;
use std::time::Duration;

use qdw_qemu::agent;
use tracing::{info, warn};

use crate::guest::cameras::Cameras;
use crate::guest::clipboard::Clipboard;
use crate::guest::consent::Consents;
use crate::guest::drives::Drives;
use crate::guest::printers::Printers;
use crate::session::input::AgentMouse;

/// How long before trying the agent again, when it could not be reached.
const RETRY: Duration = Duration::from_secs(3);
/// How long the agent has to answer `Reset`.
const RESET_TIME: Duration = Duration::from_secs(5);

/// What uses the guest agent's connection: told when it comes and goes.
#[derive(Clone)]
pub struct Users {
    /// The connection itself, for whoever calls the agent now and then (`user_left`).
    pub link: tokio::sync::watch::Sender<Option<zbus::Connection>>,
    pub mouse: AgentMouse,
    pub clipboard: Arc<Clipboard>,
    pub drives: Arc<Drives>,
    pub printers: Arc<Printers>,
    pub cameras: Arc<Cameras>,
    pub consents: Arc<Consents>,
}

/// Keep a connection to the guest agent: when the agent or the guest restarts, connect again
/// once the guest opens the agent's port, and bring the drives, printers and cameras back.
/// Runs until the VM is detached; without an agent device on the VM, there is no clipboard, no
/// drives, no printers and no cameras.
pub async fn run(vm: zbus::Connection, users: Users) {
    let Users {
        link,
        mouse,
        clipboard,
        drives,
        printers,
        cameras,
        consents,
    } = &users;
    loop {
        let host = |builder| consents.serve(printers.serve(drives.serve(clipboard.serve(builder))));
        match agent::connect(&vm, host).await {
            Ok(mut agent) => {
                let connection = agent.connection.clone();
                info!("guest agent connected");
                // Detached however this ends, this task aborted included (the server detaches
                // from the VM): what holds the connection lets it go, and QEMU takes the next.
                link.send_replace(Some(connection.clone()));
                let _attached = Attached(users.clone());
                if let Err(e) = mouse.attach(&connection).await {
                    warn!(error = %format!("{e:#}"), "guest agent pointer: the QEMU tablet instead");
                }
                if let Err(e) = drives.attach(&connection).await {
                    warn!(error = %format!("{e:#}"), "guest agent drives");
                }
                if let Err(e) = printers.attach(&connection).await {
                    warn!(error = %format!("{e:#}"), "guest agent printers");
                }
                if let Err(e) = cameras.attach(&connection).await {
                    warn!(error = %format!("{e:#}"), "guest agent cameras");
                }
                if let Err(e) = consents.attach(&connection).await {
                    info!(error = %format!("{e:#}"), "guest agent consent");
                }
                // Gone when its connection fails, or when the guest closes its port (the
                // agent restarted): nothing fails on the connection then.
                tokio::select! {
                    result = clipboard.follow(&connection) => {
                        if let Err(e) = result {
                            warn!(error = %format!("{e:#}"), "guest agent");
                        }
                    }
                    () = agent.closed() => info!("guest agent port closed"),
                }
                drop(agent);
                info!("guest agent gone");
            }
            Err(e) if e.is::<agent::NoDevice>() => {
                info!(
                    "no guest agent device on this VM: no clipboard, no drives, no printers, no cameras"
                );
                return;
            }
            Err(e) => {
                warn!(error = %format!("{e:#}"), "guest agent not reachable");
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

/// Tell the guest agent, if there is one, that the user left (`Agent1.Reset`): it lets go of
/// what the user's client brought (clipboard, drives, printers, cameras) and decides whether
/// and how to lock the guest's session.
pub async fn user_left(agent: &tokio::sync::watch::Receiver<Option<zbus::Connection>>) {
    let Some(connection) = agent.borrow().clone() else {
        info!("no guest agent: the guest is not told its user left");
        return;
    };
    let reset = async { agent::AgentProxy::new(&connection).await?.reset().await };
    match tokio::time::timeout(RESET_TIME, reset).await {
        Ok(Ok(())) => info!("the guest agent told the user left"),
        Ok(Err(e)) => warn!(error = %e, "the guest agent not told the user left"),
        Err(_) => warn!("the guest agent did not answer, told the user left"),
    }
}

/// What uses the agent's connection, detached from it when dropped.
struct Attached(Users);

impl Drop for Attached {
    fn drop(&mut self) {
        let users = &self.0;
        users.consents.detach();
        users.link.send_replace(None);
        users.clipboard.detach();
        users.drives.detach();
        users.printers.detach();
        users.cameras.detach();
        users.mouse.detach();
    }
}
