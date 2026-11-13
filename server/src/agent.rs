// SPDX-License-Identifier: MPL-2.0

//! The connection to the guest agent, which the clipboard, the drives, the printers and the
//! cameras go through.

use std::sync::Arc;
use std::time::Duration;

use qdw_qemu::agent;
use tracing::{info, warn};

use crate::cameras::Cameras;
use crate::clipboard::Clipboard;
use crate::drives::Drives;
use crate::input::AgentMouse;
use crate::printers::Printers;

/// How long before trying the agent again, when it could not be reached.
const RETRY: Duration = Duration::from_secs(3);

/// Keep a connection to the guest agent: when the agent or the guest restarts, connect again
/// once the guest opens the agent's port, and bring the drives, printers and cameras back.
/// Runs until the VM is detached; without an agent device on the VM, there is no clipboard, no
/// drives, no printers and no cameras.
pub async fn run(
    vm: zbus::Connection,
    mouse: AgentMouse,
    clipboard: Arc<Clipboard>,
    drives: Arc<Drives>,
    printers: Arc<Printers>,
    cameras: Arc<Cameras>,
) {
    loop {
        let host = |builder| printers.serve(drives.serve(clipboard.serve(builder)?)?);
        match agent::connect(&vm, host).await {
            Ok(mut agent) => {
                let connection = agent.connection.clone();
                info!("guest agent connected");
                // Detached however this ends, this task aborted included (the server detaches
                // from the VM): what holds the connection lets it go, and QEMU takes the next.
                let _attached = Attached {
                    mouse: mouse.clone(),
                    clipboard: clipboard.clone(),
                    drives: drives.clone(),
                    printers: printers.clone(),
                    cameras: cameras.clone(),
                };
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

/// What uses the agent's connection, detached from it when dropped.
struct Attached {
    mouse: AgentMouse,
    clipboard: Arc<Clipboard>,
    drives: Arc<Drives>,
    printers: Arc<Printers>,
    cameras: Arc<Cameras>,
}

impl Drop for Attached {
    fn drop(&mut self) {
        self.clipboard.detach();
        self.drives.detach();
        self.printers.detach();
        self.cameras.detach();
        self.mouse.detach();
    }
}
