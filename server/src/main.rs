// SPDX-License-Identifier: MPL-2.0

//! qemu-dbus-webrtc: a WebRTC console for QEMU virtual machines.
//!
//! For now (milestone 3), it attaches to a running VM, encodes each console with the host GPU,
//! shows the first one in a browser over WebRTC, and can record the streams to files.

mod input;
mod pointer;
mod session;
mod web;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use gstreamer as gst;
use qdw_qemu::display::{self, Attach, DisplayEvent, VmProxy};
use qdw_video::encode::{Codec, Encoder, Settings};
use qdw_video::record;
use session::Viewers;
use web::Request;

const USAGE: &str = "usage: qemu-dbus-webrtc (--qmp <QMP socket> | --dbus <D-Bus address>) \
                     [--codec av1|vp9|hevc] [--chroma 420|444] [--bitrate <kbit/s>] \
                     [--record <path prefix>] [--listen <address:port>]";

/// Where the viewer page is served by default: this host only, since there is no
/// authentication yet.
const DEFAULT_LISTEN: &str = "127.0.0.1:8090";

/// On a still screen, a console with viewers still gets a frame this often. Each one is nearly
/// empty since nothing changed, but it shows the stream is alive: a WebRTC receiver that gets
/// no frame for a few seconds takes the stream for frozen and asks for a keyframe (libwebrtc
/// does, about every 3 seconds). Congestion control will feed on these frames too.
const STILL_FRAME_INTERVAL: Duration = Duration::from_millis(250);

/// At most one keyframe per console in this interval, whatever the viewers ask.
const KEYFRAME_INTERVAL: Duration = Duration::from_millis(300);

/// How often the rates of each console are reported.
const RATE_INTERVAL: Duration = Duration::from_secs(5);

struct Args {
    attach: Attach,
    settings: Settings,
    /// Record each console's stream to `<prefix>-<console>.<extension>`.
    record: Option<String>,
    /// The address of the viewer page and its signalling.
    listen: SocketAddr,
}

fn parse_args() -> Result<Args> {
    let mut attach = None;
    let mut settings = Settings {
        codec: Codec::Av1,
        full_chroma: false,
        bitrate: 10_000,
    };
    let mut record = None;
    let mut listen: SocketAddr = DEFAULT_LISTEN.parse()?;
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
            "--listen" => listen = value.parse().context(USAGE)?,
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
        listen,
    })
}

/// Where an encoder's output goes: to the console's viewers if it has any, and to a file when
/// recording.
fn stream_sink(
    args: &Args,
    console: u32,
    viewers: Option<Viewers>,
) -> Result<impl FnMut(&gst::Buffer) + Send + 'static> {
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
    Ok(move |frame: &gst::Buffer| {
        if let Some(viewers) = &viewers {
            let viewers = viewers.lock().unwrap();
            if !viewers.is_empty() {
                // Each viewer's pipeline has its own clock, started when the viewer came: drop
                // the encoder's timestamps so that its appsrc stamps the frame on arrival.
                // Kept, they would be ahead of the viewer's clock by the time between the two
                // starts, and the frame held back for that long.
                let mut unstamped = frame.copy();
                {
                    let unstamped = unstamped.get_mut().unwrap();
                    unstamped.set_pts(gst::ClockTime::NONE);
                    unstamped.set_dts(gst::ClockTime::NONE);
                }
                for viewer in viewers.iter() {
                    // A viewer that is going away refuses it; it is removed with its session.
                    let _ = viewer.push_buffer(unstamped.clone());
                }
            }
        }
        if let Some(r) = &mut recorder {
            let written = frame
                .map_readable()
                .map_err(|e| std::io::Error::other(e.to_string()))
                .and_then(|map| r.write(&map));
            if let Err(e) = written {
                warn!(console, error = %e, "recording stopped");
                recorder = None;
            }
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
    let mut shown = None;
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
            shown.get_or_insert(id);
        }
    }
    drop(events_tx);

    // Viewers see the first graphic console; several monitors come later.
    let shown = shown.context("the VM has no graphic console")?;
    let viewers: HashMap<u32, Viewers> = HashMap::from([(shown, Viewers::default())]);
    let (requests_tx, mut requests) = mpsc::unbounded_channel();
    let (pointer_tx, pointer_rx) = tokio::sync::watch::channel(pointer::Pointer::default());
    let shared = web::Shared {
        codec: args.settings.codec,
        console: shown,
        viewers: viewers[&shown].clone(),
        requests: requests_tx,
        input: qdw_qemu::input::Input::new(&vm_connection, shown).await?,
        pointer: pointer_rx,
    };
    if !args.listen.ip().is_loopback() {
        warn!(listen = %args.listen, "no authentication yet: anyone who reaches this address sees the VM");
    }
    let listen = args.listen;
    tokio::spawn(async move {
        if let Err(e) = web::serve(listen, shared).await {
            warn!(error = %format!("{e:#}"), "the viewer page is down");
        }
    });
    let mut last_keyframe: HashMap<u32, Instant> = HashMap::new();

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
    let mut still = tokio::time::interval(STILL_FRAME_INTERVAL);
    still.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let event = tokio::select! {
            event = events.recv() => event,
            _ = still.tick() => {
                // A frame pushed just after a tick is a bit less than one interval old at the
                // next one: hence the margin, or every other tick would be skipped.
                for (console, encoder) in &encoders {
                    let watched = viewers.get(console).is_some_and(|v| !v.lock().unwrap().is_empty());
                    if watched && encoder.since_last_frame() >= STILL_FRAME_INTERVAL.mul_f32(0.8)
                        && let Err(e) = encoder.frame()
                    {
                        warn!(console, error = %e, "still frame not encoded");
                    }
                }
                continue;
            }
            _ = report.tick() => {
                report_rates(&encoders, &mut updates, since.elapsed());
                since = Instant::now();
                continue;
            }
            Some(request) = requests.recv() => {
                match request {
                    Request::Keyframe { console } => {
                        let due = last_keyframe.get(&console).is_none_or(|t| t.elapsed() >= KEYFRAME_INTERVAL);
                        if due && let Some(encoder) = encoders.get(&console) {
                            last_keyframe.insert(console, Instant::now());
                            if let Err(e) = encoder.keyframe() {
                                warn!(console, error = %e, "keyframe not encoded");
                            }
                        }
                    }
                }
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
                    viewers.get(&console).cloned(),
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
            } => {
                debug!(console, x, y, visible, "pointer");
                if console == shown {
                    pointer_tx.send_if_modified(|p| {
                        std::mem::replace(&mut p.visible, visible) != visible
                    });
                }
            }
            DisplayEvent::CursorDefine {
                console,
                width,
                height,
                hot_x,
                hot_y,
                data,
            } => {
                debug!(console, size = %format!("{width}x{height}"), hot_x, hot_y, "pointer shape");
                if console == shown {
                    match pointer::shape(width, height, hot_x, hot_y, &data) {
                        Some(shape) => {
                            pointer_tx.send_modify(|p| p.shape = Some(std::sync::Arc::new(shape)))
                        }
                        None => warn!(
                            console,
                            bytes = data.len(),
                            "pointer shape of an unexpected size"
                        ),
                    }
                }
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
    viewers: Option<Viewers>,
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
        std::collections::hash_map::Entry::Vacant(entry) => entry.insert(Encoder::new(
            &args.settings,
            stream_sink(args, console, viewers)?,
        )?),
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
