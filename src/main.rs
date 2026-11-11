// SPDX-License-Identifier: MPL-2.0

//! qemu-dbus-webrtc: a WebRTC console for QEMU virtual machines.
//!
//! For now (milestone 2), it attaches to a running VM, encodes each console with the host GPU
//! and can record the streams to files.

mod display;
mod encode;
mod qmp;
mod record;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use display::{Attach, DisplayEvent, VmProxy};
use encode::{Codec, Encoder, Settings};

const USAGE: &str = "usage: qemu-dbus-webrtc (--qmp <QMP socket> | --dbus <D-Bus address>) \
                     [--codec av1|vp9|hevc|h264] [--chroma 420|444] [--bitrate <kbit/s>] \
                     [--record <path prefix>]";

/// How often the rates of each console are reported.
const RATE_INTERVAL: Duration = Duration::from_secs(5);

struct Args {
    attach: Attach,
    settings: Settings,
    /// Record each console's stream to `<prefix>-<console>.<extension>`.
    record: Option<String>,
}

fn parse_args() -> Result<Args> {
    let mut attach = None;
    let mut settings = Settings {
        codec: Codec::Av1,
        full_chroma: false,
        bitrate: 10_000,
    };
    let mut record = None;
    let mut args = std::env::args().skip(1);
    while let Some(option) = args.next() {
        let Some(value) = args.next() else {
            bail!(USAGE)
        };
        match option.as_str() {
            "--qmp" if attach.is_none() => {
                attach = Some(Attach::P2p {
                    qmp: PathBuf::from(value),
                })
            }
            "--dbus" if attach.is_none() => attach = Some(Attach::Bus { address: value }),
            "--codec" => settings.codec = Codec::parse(&value).context(USAGE)?,
            "--chroma" => {
                settings.full_chroma = match value.as_str() {
                    "420" => false,
                    "444" => true,
                    _ => bail!(USAGE),
                }
            }
            "--bitrate" => settings.bitrate = value.parse().context(USAGE)?,
            "--record" => record = Some(value),
            _ => bail!(USAGE),
        }
    }
    let Some(attach) = attach else { bail!(USAGE) };
    if settings.full_chroma && !settings.codec.has_full_chroma() {
        bail!("{:?} has no 4:4:4 on the hardware encoders", settings.codec);
    }
    Ok(Args {
        attach,
        settings,
        record,
    })
}

/// Where an encoder's output goes: a file when recording, nowhere otherwise.
fn stream_sink(args: &Args, console: u32) -> Result<impl FnMut(&[u8]) + Send + 'static> {
    let mut recorder = match &args.record {
        Some(prefix) => {
            let path = format!("{prefix}-{console}.{}", args.settings.codec.extension());
            info!(console, %path, "recording");
            Some(
                record::Recorder::create(path.as_ref(), args.settings.codec)
                    .with_context(|| format!("creating {path}"))?,
            )
        }
        None => None,
    };
    Ok(move |frame: &[u8]| {
        if let Some(r) = &mut recorder
            && let Err(e) = r.write(frame)
        {
            warn!(console, error = %e, "recording stopped");
            recorder = None;
        }
    })
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
    let args = parse_args()?;
    gstreamer::init()?;

    let vm_connection = display::attach(&args.attach).await?;
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

    let mut encoders: HashMap<u32, Encoder> = HashMap::new();
    // Consoles with a new framebuffer not encoded yet. A scanout comes with its full content,
    // so it is encoded on its own, but only once no other event is waiting: when a listener
    // registers, QEMU sends the console's 2D surface, possibly stale, right before the real
    // scanout.
    let mut fresh: std::collections::HashSet<u32> = std::collections::HashSet::new();
    // Guest updates per console since the last report.
    let mut updates: HashMap<u32, u32> = HashMap::new();
    let mut since = Instant::now();
    let mut report = tokio::time::interval(RATE_INTERVAL);
    loop {
        let event = tokio::select! {
            event = events.recv() => event,
            _ = report.tick() => {
                report_rates(&encoders, &mut updates, since.elapsed());
                since = Instant::now();
                continue;
            }
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
            } => {
                info!(
                    console,
                    size = %format!("{width}x{height}"),
                    stride,
                    format = %format!("{pixman_format:#x}"),
                    bytes = data.len(),
                    "scanout (shared memory), not encoded"
                );
                if let Some(encoder) = encoders.remove(&console) {
                    encoder.finish();
                }
            }
            DisplayEvent::ScanoutDmabuf {
                console,
                planes,
                width,
                height,
                fourcc: code,
                modifier,
                y0_top,
            } => {
                info!(
                    console,
                    size = %format!("{width}x{height}"),
                    fourcc = %fourcc(code),
                    modifier = %format!("{modifier:#x}"),
                    planes = planes.len(),
                    offsets = ?planes.iter().map(|p| p.offset).collect::<Vec<_>>(),
                    strides = ?planes.iter().map(|p| p.stride).collect::<Vec<_>>(),
                    y0_top,
                    "scanout (DMA-BUF)"
                );
                fresh.insert(console);
                if let Err(e) = scanout(
                    &args,
                    &mut encoders,
                    console,
                    &planes,
                    width,
                    height,
                    code,
                    modifier,
                ) {
                    warn!(console, error = %format!("{e:#}"), "not encoded");
                    if let Some(encoder) = encoders.remove(&console) {
                        encoder.finish();
                    }
                }
            }
            DisplayEvent::Update {
                console,
                x,
                y,
                width,
                height,
            } => {
                debug!(console, x, y, size = %format!("{width}x{height}"), "update");
                *updates.entry(console).or_default() += 1;
            }
            DisplayEvent::UpdateDmabuf {
                console,
                x,
                y,
                width,
                height,
            } => {
                debug!(console, x, y, size = %format!("{width}x{height}"), "update");
                *updates.entry(console).or_default() += 1;
                fresh.remove(&console);
                if let Some(encoder) = encoders.get(&console)
                    && let Err(e) = encoder.frame()
                {
                    warn!(console, error = %e, "frame not encoded");
                }
            }
            DisplayEvent::Disable { console } => {
                info!(console, "disabled");
                // A mode change disables the head for a moment: keep the encoder for when it
                // comes back.
                if let Some(encoder) = encoders.get_mut(&console) {
                    encoder.clear_framebuffer();
                }
            }
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
        if events.is_empty() {
            for console in fresh.drain() {
                if let Some(encoder) = encoders.get(&console)
                    && let Err(e) = encoder.frame()
                {
                    warn!(console, error = %e, "frame not encoded");
                }
            }
        }
    }
    for (_, encoder) in encoders.drain() {
        encoder.finish();
    }
    drop(listeners);
    Ok(())
}

/// A new DMA-BUF framebuffer on `console`: point its encoder at it, creating the encoder first
/// if needed.
#[allow(clippy::too_many_arguments)]
fn scanout(
    args: &Args,
    encoders: &mut HashMap<u32, Encoder>,
    console: u32,
    planes: &[display::Plane],
    width: u32,
    height: u32,
    code: u32,
    modifier: u64,
) -> Result<()> {
    let [plane] = planes else {
        bail!("{} planes, one expected", planes.len())
    };
    if modifier != 0 {
        bail!("modifier {modifier:#x}, only linear buffers are handled");
    }
    let encoder = match encoders.entry(console) {
        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(Encoder::new(&args.settings, stream_sink(args, console)?)?)
        }
    };
    encoder.set_framebuffer(&plane.fd, width, height, code, plane.offset, plane.stride)
}

/// Log the rates of each active console over the last `elapsed`.
fn report_rates(
    encoders: &HashMap<u32, Encoder>,
    updates: &mut HashMap<u32, u32>,
    elapsed: Duration,
) {
    let secs = elapsed.as_secs_f64();
    for (&console, encoder) in encoders {
        let (frames, bytes) = encoder.take_counters();
        let guest = updates.remove(&console).unwrap_or(0);
        if guest == 0 && frames == 0 {
            continue;
        }
        info!(
            console,
            updates_per_second = format!("{:.1}", f64::from(guest) / secs),
            frames_per_second = format!("{:.1}", frames as f64 / secs),
            kbit_per_second = format!("{:.0}", bytes as f64 * 8.0 / 1000.0 / secs),
            "rates"
        );
    }
    updates.clear();
}
