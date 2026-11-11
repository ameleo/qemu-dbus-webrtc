// SPDX-License-Identifier: MPL-2.0

//! qemu-dbus-webrtc: a WebRTC console for QEMU virtual machines.
//!
//! For now (milestone 1), it attaches to a running VM and reports what its display does.

mod display;
mod qmp;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use display::{Attach, DisplayEvent, VmProxy};

const USAGE: &str = "usage: qemu-dbus-webrtc (--qmp <QMP socket> | --dbus <D-Bus address>)";

/// How often the update rate of each console is reported.
const RATE_INTERVAL: Duration = Duration::from_secs(5);

fn parse_args() -> Result<Attach> {
    let mut args = std::env::args().skip(1);
    let attach = match (args.next().as_deref(), args.next()) {
        (Some("--qmp"), Some(path)) => Attach::P2p {
            qmp: PathBuf::from(path),
        },
        (Some("--dbus"), Some(address)) => Attach::Bus { address },
        _ => bail!(USAGE),
    };
    if args.next().is_some() {
        bail!(USAGE);
    }
    Ok(attach)
}

/// A fourcc as its four characters.
fn fourcc(code: u32) -> String {
    code.to_le_bytes()
        .iter()
        .map(|&b| if b.is_ascii_graphic() { b as char } else { '?' })
        .collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let attach = parse_args()?;

    let vm_connection = display::attach(&attach).await?;
    let vm = VmProxy::new(&vm_connection).await?;
    let ids = vm.console_ids().await?;
    info!(name = %vm.name().await?, uuid = %vm.uuid().await?, consoles = ?ids, "attached");

    let (events_tx, mut events) = mpsc::unbounded_channel();
    let mut listeners = Vec::new();
    for id in ids {
        let console = display::ConsoleProxy::builder(&vm_connection)
            .path(format!("/org/qemu/Display1/Console_{id}"))?
            .build()
            .await?;
        let kind = console.kind().await?;
        info!(
            console = id,
            label = %console.label().await?,
            head = console.head().await?,
            kind = %kind,
            size = %format!("{}x{}", console.width().await?, console.height().await?),
            "console"
        );
        if kind == "Graphic" {
            listeners.push(display::listen(&vm_connection, id, events_tx.clone()).await?);
            info!(console = id, "listening");
        }
    }
    drop(events_tx);

    // Updates per console since the last report.
    let mut updates: HashMap<u32, u32> = HashMap::new();
    let mut since = Instant::now();
    loop {
        let event = tokio::select! {
            event = events.recv() => event,
            _ = tokio::signal::ctrl_c() => break,
        };
        let Some(event) = event else {
            warn!("the display listeners are gone");
            break;
        };
        match event {
            DisplayEvent::Scanout {
                console,
                width,
                height,
                stride,
                pixman_format,
                data,
            } => info!(
                console,
                size = %format!("{width}x{height}"),
                stride,
                format = %format!("{pixman_format:#x}"),
                bytes = data.len(),
                "scanout (shared memory)"
            ),
            DisplayEvent::ScanoutDmabuf {
                console,
                planes,
                width,
                height,
                fourcc: code,
                modifier,
                y0_top,
            } => info!(
                console,
                size = %format!("{width}x{height}"),
                fourcc = %fourcc(code),
                modifier = %format!("{modifier:#x}"),
                planes = planes.len(),
                offsets = ?planes.iter().map(|p| p.offset).collect::<Vec<_>>(),
                strides = ?planes.iter().map(|p| p.stride).collect::<Vec<_>>(),
                y0_top,
                "scanout (DMA-BUF)"
            ),
            DisplayEvent::Update {
                console,
                x,
                y,
                width,
                height,
            }
            | DisplayEvent::UpdateDmabuf {
                console,
                x,
                y,
                width,
                height,
            } => {
                debug!(console, x, y, size = %format!("{width}x{height}"), "update");
                *updates.entry(console).or_default() += 1;
            }
            DisplayEvent::Disable { console } => info!(console, "disabled"),
            DisplayEvent::MouseSet {
                console,
                x,
                y,
                visible,
            } => debug!(console, x, y, visible, "pointer"),
            DisplayEvent::CursorDefine {
                console,
                width,
                height,
                hot_x,
                hot_y,
                data,
            } => {
                info!(console, size = %format!("{width}x{height}"), hot_x, hot_y, bytes = data.len(), "cursor shape");
            }
        }
        if since.elapsed() >= RATE_INTERVAL {
            let secs = since.elapsed().as_secs_f64();
            for (console, count) in updates.drain() {
                info!(
                    console,
                    per_second = format!("{:.1}", f64::from(count) / secs),
                    "updates"
                );
            }
            since = Instant::now();
        }
    }
    drop(listeners);
    Ok(())
}
