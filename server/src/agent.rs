// SPDX-License-Identifier: MPL-2.0

//! The connection to the guest agent, which the clipboard, the drives and the printers go
//! through.

use std::sync::Arc;
use std::time::Duration;

use qdw_qemu::agent;
use tracing::{info, warn};

use crate::clipboard::Clipboard;
use crate::drives::Drives;
use crate::printers::Printers;

/// How long before trying the agent again, when it went away.
const RETRY: Duration = Duration::from_secs(3);

/// Keep a connection to the guest agent: when the guest restarts, connect again and bring the
/// drives and printers back. Runs until the program ends; without an agent device on the VM,
/// there is no clipboard, no drives and no printers.
pub async fn run(
    vm: zbus::Connection,
    clipboard: Arc<Clipboard>,
    drives: Arc<Drives>,
    printers: Arc<Printers>,
) {
    loop {
        let host = |builder| printers.serve(drives.serve(clipboard.serve(builder)?)?);
        match agent::connect(&vm, host).await {
            Ok(connection) => {
                info!("guest agent connected");
                if let Err(e) = drives.attach(&connection).await {
                    warn!(error = %format!("{e:#}"), "guest agent drives");
                }
                if let Err(e) = printers.attach(&connection).await {
                    warn!(error = %format!("{e:#}"), "guest agent printers");
                }
                if let Err(e) = clipboard.follow(&connection).await {
                    warn!(error = %format!("{e:#}"), "guest agent");
                }
                info!("guest agent gone");
                clipboard.detach();
                drives.detach();
                printers.detach();
            }
            Err(e) => {
                let e = format!("{e:#}");
                if e.contains("no fr.ameleo.agent.0") {
                    info!("no guest agent device on this VM: no clipboard, no drives, no printers");
                    return;
                }
                warn!(error = %e, "guest agent not reachable");
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}
