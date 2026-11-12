// SPDX-License-Identifier: MPL-2.0

//! qemu-dbus-webrtc: a WebRTC console for QEMU virtual machines.
//!
//! For now (milestone 3), it attaches to a running VM, encodes each console with the host GPU,
//! shows the first one in a browser over WebRTC, and can record the streams to files.

mod agent;
mod audio;
mod cameras;
mod clipboard;
mod copied;
mod drives;
mod input;
mod layout;
mod pointer;
mod printers;
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
use qdw_video::{gpus, record};
use session::Viewers;
use web::Request;

const USAGE: &str = "usage: qemu-dbus-webrtc (--qmp <QMP socket> | --dbus <D-Bus address>) \
                     [--codec av1|vp9|hevc] [--chroma 420|444] [--software] [--bitrate <max kbit/s>] \
                     [--record <path prefix>] [--listen <address:port>] \
                     [--tls <certificate.pem> <key.pem>]\n       \
                     qemu-dbus-webrtc --codecs   (what the GPUs encode)";

/// Where the viewer page is served by default: this host only, since there is no
/// authentication yet.
const DEFAULT_LISTEN: &str = "127.0.0.1:8090";

/// On a still screen, a console with viewers still gets a frame this often. Each one is nearly
/// empty since nothing changed, but it shows the stream is alive: a WebRTC receiver that gets
/// no frame for a few seconds takes the stream for frozen and asks for a keyframe (libwebrtc
/// does, about every 3 seconds). Congestion control will feed on these frames too.
const STILL_FRAME_INTERVAL: Duration = Duration::from_millis(250);

/// At most one keyframe per console in this interval, whatever the viewers ask. A keyframe is
/// large: on a congested connection, it brings more losses, and more requests for keyframes;
/// lost packets are asked again (NACK) and sent again (RTX) in the meantime.
const KEYFRAME_INTERVAL: Duration = Duration::from_secs(1);

/// The bandwidth estimation, in kbit/s: never below `MIN_BITRATE` (a desktop below 4 Mbit/s is
/// not usable, so the session rather fails visibly), starting at `START_BITRATE` (sharp at
/// once on today's connections, where a ramp from the floor would show a blurred screen for
/// seconds), up to `--bitrate`.
const MIN_BITRATE: u32 = 4_000;
const START_BITRATE: u32 = 10_000;
const MAX_BITRATE: u32 = 30_000;

/// The video bitrate follows the user's estimate when it changes by this share at least...
const BITRATE_STEP: f64 = 0.05;
/// ...and at most this often, unless it falls by `BITRATE_DROP` or more (the connection is
/// congested): a bitrate change resets the rate control of the encoder, which needs a few
/// frames to settle.
const BITRATE_INTERVAL: Duration = Duration::from_millis(500);
const BITRATE_DROP: f64 = 0.15;
/// The bitrate never goes beyond this many times what the encoder produced over the last
/// `OUTPUT_WINDOW`: on a quiet screen, the encoder sends less than the estimate, and the
/// estimate rises with no evidence of the bandwidth (GCC sees no delay on a link that is never
/// full); at the first large motion, the encoder would send far more than the link takes.
/// From a quiet screen, the bitrate climbs back by this ratio at each step.
const OUTPUT_HEADROOM: f64 = 1.25;
const OUTPUT_WINDOW: Duration = Duration::from_secs(1);

/// At most this many frames a second per console. The VA encoders take a variable frame rate
/// for 30 frames a second and give each frame a thirtieth of their bitrate: more frames would
/// send more than the bitrate, beyond the bandwidth estimate.
const MAX_FPS: u32 = 30;
const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / MAX_FPS as u64);

/// How often the rates of each console are reported.
const RATE_INTERVAL: Duration = Duration::from_secs(5);

struct Args {
    attach: Attach,
    settings: Settings,
    /// `--codec` was given: no other codec in its place.
    codec_given: bool,
    bitrates: session::Bitrates,
    /// Record each console's stream to `<prefix>-<console>.<extension>`.
    record: Option<String>,
    /// The address of the viewer page and its signalling.
    listen: SocketAddr,
    /// Serve in HTTPS.
    tls: Option<web::Tls>,
}

fn parse_args() -> Result<Args> {
    let mut attach = None;
    let mut settings = Settings {
        codec: Codec::Av1,
        full_chroma: false,
        bitrate: START_BITRATE,
        ten_bits: false,
        software: false,
    };
    let mut codec_given = false;
    let mut max_bitrate = MAX_BITRATE;
    let mut record = None;
    let mut listen: SocketAddr = DEFAULT_LISTEN.parse()?;
    let mut tls = None;
    let mut args = std::env::args().skip(1);
    while let Some(option) = args.next() {
        // Options without a value.
        if option == "--software" {
            settings.software = true;
            continue;
        }
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
            "--codec" => {
                settings.codec = Codec::parse(&value).context(USAGE)?;
                codec_given = true;
            }
            "--chroma" => {
                settings.full_chroma = match value.as_str() {
                    "420" => false,
                    "444" => true,
                    _ => bail!(USAGE),
                }
            }
            "--bitrate" => max_bitrate = value.parse().context(USAGE)?,
            "--record" => record = Some(value),
            "--listen" => listen = value.parse().context(USAGE)?,
            "--tls" => {
                let Some(key) = args.next() else { bail!(USAGE) };
                tls = Some(web::Tls {
                    certificate: value.into(),
                    key: key.into(),
                });
            }
            _ => bail!(USAGE),
        }
    }
    let Some(attach) = attach else { bail!(USAGE) };
    // On the CPU, VP9 unless another codec was asked for.
    if settings.software && !codec_given {
        settings.codec = Codec::Vp9;
    }
    if settings.full_chroma && !settings.codec.has_full_chroma() {
        bail!("{:?} has no 4:4:4 on the hardware encoders", settings.codec);
    }
    let max = max_bitrate.max(MIN_BITRATE);
    let bitrates = session::Bitrates {
        min: MIN_BITRATE,
        start: START_BITRATE.min(max),
        max,
    };
    settings.bitrate = bitrates.start;
    Ok(Args {
        attach,
        settings,
        codec_given,
        bitrates,
        record,
        listen,
        tls,
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
            session::fan_out(viewers, frame);
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
    if std::env::args().nth(1).as_deref() == Some("--codecs") {
        gstreamer::init()?;
        print!("{}", gpus::report(&gpus::gpus(), gpus::software()));
        return Ok(());
    }
    let mut args = parse_args()?;
    gstreamer::init()?;
    let found = gpus::gpus();
    // Without a GPU, encode on the CPU: VP9, unless another codec was asked for.
    if found.is_empty() && !args.settings.software {
        info!("no GPU encoder: encoding on the CPU");
        args.settings.software = true;
    }
    // On the CPU, VP9 unless another codec was asked for.
    if args.settings.software && !args.codec_given {
        args.settings.codec = Codec::Vp9;
    }
    // Fail now, saying why, rather than at the first frame.
    gpus::check(&found, &args.settings)?;

    let vm_connection = display::attach(&args.attach).await?;
    let vm = VmProxy::new(&vm_connection).await?;
    let ids = vm.console_ids().await?;
    info!(name = %vm.name().await?, uuid = %vm.uuid().await?, consoles = ?ids, "attached");

    let (events_tx, mut events) = mpsc::unbounded_channel();
    let mut listeners = Vec::new();
    // The graphic consoles: the guest's screens, by head.
    let mut graphic = Vec::new();
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
            graphic.push((console.head().await?, id, console));
        }
    }
    drop(events_tx);

    // The guest's screens, in the order of their heads: one video track each for the viewers,
    // as many as a session carries.
    graphic.sort_by_key(|(head, id, _)| (*head, *id));
    graphic.truncate(session::MAX_SCREENS);
    if graphic.is_empty() {
        bail!("the VM has no graphic console");
    }
    let screens: Vec<web::Screen> = graphic
        .iter()
        .map(|&(head, console, _)| web::Screen {
            console,
            head,
            viewers: Viewers::default(),
        })
        .collect();
    info!(screens = ?screens.iter().map(|s| s.console).collect::<Vec<_>>(), "guest screens");
    let viewers: HashMap<u32, Viewers> = screens
        .iter()
        .map(|s| (s.console, s.viewers.clone()))
        .collect();
    // The screen of each console.
    let screen_of: HashMap<u32, usize> = screens
        .iter()
        .enumerate()
        .map(|(i, s)| (s.console, i))
        .collect();
    let first = screens[0].console;
    let (requests_tx, mut requests) = mpsc::unbounded_channel();
    // The guest's sound, encoded once for every viewer.
    let audio_viewers = session::AudioViewers::default();
    let audio = audio::start(&vm_connection, audio_viewers.clone()).await?;
    let (pointer_tx, pointer_rx) =
        tokio::sync::watch::channel(vec![pointer::Pointer::default(); screens.len()]);
    // The clipboard, through the guest agent, if the VM has one.
    let clipboard = clipboard::Clipboard::new();
    let drives = drives::Drives::new();
    let printers = printers::Printers::new();
    let cameras = cameras::Cameras::new();
    // The guest's screens at the size and place of the user's monitors.
    let layout = layout::Layout::start(graphic.into_iter().map(|(_, _, c)| c).collect());
    let agent_mouse = input::AgentMouse::default();
    tokio::spawn(agent::run(
        vm_connection.clone(),
        agent_mouse.clone(),
        clipboard.clone(),
        drives.clone(),
        printers.clone(),
        cameras.clone(),
    ));
    let shared = web::Shared {
        codec: args.settings.codec,
        full_chroma: args.settings.full_chroma,
        screens: screens.clone(),
        audio_viewers: audio_viewers.clone(),
        mic: audio.mic.clone(),
        requests: requests_tx,
        input: qdw_qemu::input::Input::new(&vm_connection, first).await?,
        agent_mouse,
        pointer: pointer_rx,
        clipboard: clipboard.clone(),
        drives: drives.clone(),
        printers: printers.clone(),
        cameras: cameras.clone(),
        layout: layout.clone(),
        user: Default::default(),
        bitrates: args.bitrates,
        depths: Default::default(),
    };
    if !args.listen.ip().is_loopback() {
        warn!(listen = %args.listen, "no authentication yet: anyone who reaches this address sees the VM");
    }
    let (listen, tls) = (args.listen, args.tls.clone());
    tokio::spawn(async move {
        if let Err(e) = web::serve(listen, tls, shared).await {
            warn!(error = %format!("{e:#}"), "the viewer page is down");
        }
    });
    let mut last_keyframe: HashMap<u32, Instant> = HashMap::new();
    // The video bitrate, and when it last changed: new encoders start at it.
    let mut settings = args.settings.clone();
    let mut bitrate_changed = Instant::now();
    // What each encoder produced, sampled at each still frame tick, over `OUTPUT_WINDOW`.
    let mut output: HashMap<u32, std::collections::VecDeque<(Instant, u64)>> = HashMap::new();
    // A console is encoded when someone watches it (a paused viewer does not) or records it:
    // otherwise its updates cost nothing. A viewer that comes or resumes asks for a keyframe,
    // which encodes the screen as it is.
    let recording = args.record.is_some();
    let wanted = |console: u32| {
        recording
            || viewers
                .get(&console)
                .is_some_and(|v| !v.lock().unwrap().is_empty())
    };

    let mut encoders: HashMap<u32, Encoder> = HashMap::new();
    // The size of each console's screen while it is on: the screens share the bitrate by it.
    let mut sizes: HashMap<u32, (u32, u32)> = HashMap::new();
    // The framebuffers QEMU copies in its calls rather than share, by console.
    let mut copies: HashMap<u32, copied::Copied> = HashMap::new();
    // An update that comes sooner than `FRAME_INTERVAL` after the last frame is not dropped:
    // the console waits in `due`, and its latest framebuffer is encoded as soon as the frame
    // rate allows, so the last picture always goes out.
    let mut due: std::collections::HashSet<u32> = std::collections::HashSet::new();
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
        // When the first console waiting for its frame rate may be encoded.
        let next_due = due
            .iter()
            .map(|console| {
                encoders.get(console).map_or(Duration::ZERO, |encoder| {
                    FRAME_INTERVAL.saturating_sub(encoder.since_last_frame())
                })
            })
            .min();
        let event = tokio::select! {
            event = events.recv() => event,
            () = tokio::time::sleep(next_due.unwrap_or_default()), if next_due.is_some() => {
                for console in std::mem::take(&mut due) {
                    if wanted(console)
                        && let Some(encoder) = encoders.get(&console)
                        && let Err(e) = encoder.frame()
                    {
                        warn!(console, error = %e, "frame not encoded");
                    }
                }
                continue;
            }
            _ = still.tick() => {
                // A frame pushed just after a tick is a bit less than one interval old at the
                // next one: hence the margin, or every other tick would be skipped.
                for (console, encoder) in &encoders {
                    let samples = output.entry(*console).or_default();
                    samples.push_back((Instant::now(), encoder.output()));
                    while samples.front().is_some_and(|(at, _)| at.elapsed() > OUTPUT_WINDOW) {
                        samples.pop_front();
                    }
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
                    Request::Bitrate { bitrate } => {
                        // Within reach of what the encoders produced lately.
                        let produced: Option<f64> = output
                            .values()
                            .map(|samples| {
                                let (&(first, from), &(last, to)) = (samples.front()?, samples.back()?);
                                let span = last.duration_since(first).as_secs_f64();
                                (span >= OUTPUT_WINDOW.as_secs_f64() / 2.)
                                    .then(|| to.saturating_sub(from) as f64 * 8. / 1000. / span)
                            })
                            .sum();
                        let bitrate = match produced {
                            Some(produced) => bitrate
                                .min((produced * OUTPUT_HEADROOM) as u32)
                                .max(args.bitrates.min),
                            None => bitrate,
                        };
                        let current = f64::from(settings.bitrate);
                        let change = (f64::from(bitrate) - current).abs() / current;
                        let congested = f64::from(bitrate) <= current * (1.0 - BITRATE_DROP);
                        if change >= BITRATE_STEP
                            && (congested || bitrate_changed.elapsed() >= BITRATE_INTERVAL)
                        {
                            debug!(from = settings.bitrate, to = bitrate, "video bitrate");
                            settings.bitrate = bitrate;
                            bitrate_changed = Instant::now();
                            share(&encoders, &sizes, bitrate, args.bitrates.min);
                        }
                    }
                    Request::TenBits(ten_bits) => {
                        if settings.codec.has_ten_bits() && settings.ten_bits != ten_bits {
                            info!(ten_bits, "video depth");
                            settings.ten_bits = ten_bits;
                            for (console, encoder) in encoders.iter_mut() {
                                if let Err(e) = encoder.restart(&settings) {
                                    warn!(console, error = %format!("{e:#}"), "encoder not restarted");
                                }
                            }
                        }
                    }
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
        // The console the guest drew on, if it did.
        let mut drawn = None;
        // The screens on changed: they share the bitrate anew.
        let mut rebalance = false;
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
                    "scanout (copied)"
                );
                fresh.insert(console);
                if let Some(&screen) = screen_of.get(&console) {
                    layout.shown(screen, width, height);
                }
                sizes.insert(console, (width, height));
                rebalance = true;
                let result = copied::Copied::new(width, height, stride, &data).and_then(|copy| {
                    let fd = copy.fd()?;
                    scanout(
                        viewers.get(&console).cloned(),
                        &args,
                        &settings,
                        &mut encoders,
                        console,
                        width,
                        height,
                        Source::Shared {
                            fd: &fd,
                            offset: 0,
                            stride: copy.stride(),
                            pixman_format,
                        },
                    )?;
                    copies.insert(console, copy);
                    Ok(())
                });
                if let Err(e) = result {
                    warn!(console, error = %format!("{e:#}"), "not encoded");
                    copies.remove(&console);
                    if let Some(encoder) = encoders.remove(&console) {
                        encoder.finish();
                    }
                }
            }
            DisplayEvent::ScanoutMap {
                console,
                fd,
                offset,
                width,
                height,
                stride,
                pixman_format,
            } => {
                info!(
                    console,
                    size = %format!("{width}x{height}"),
                    stride,
                    format = %format!("{pixman_format:#x}"),
                    "scanout (shared memory)"
                );
                fresh.insert(console);
                copies.remove(&console);
                if let Some(&screen) = screen_of.get(&console) {
                    layout.shown(screen, width, height);
                }
                sizes.insert(console, (width, height));
                rebalance = true;
                if let Err(e) = scanout(
                    viewers.get(&console).cloned(),
                    &args,
                    &settings,
                    &mut encoders,
                    console,
                    width,
                    height,
                    Source::Shared {
                        fd: &fd,
                        offset,
                        stride,
                        pixman_format,
                    },
                ) {
                    warn!(console, error = %format!("{e:#}"), "not encoded");
                    if let Some(encoder) = encoders.remove(&console) {
                        encoder.finish();
                    }
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
                copies.remove(&console);
                if let Some(&screen) = screen_of.get(&console) {
                    layout.shown(screen, width, height);
                }
                sizes.insert(console, (width, height));
                rebalance = true;
                if let Err(e) = scanout(
                    viewers.get(&console).cloned(),
                    &args,
                    &settings,
                    &mut encoders,
                    console,
                    width,
                    height,
                    Source::Dmabuf {
                        planes: &planes,
                        fourcc: code,
                        modifier,
                    },
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
                stride,
                data,
                ..
            } => {
                debug!(console, x, y, size = %format!("{width}x{height}"), "update (copied)");
                match copies.get(&console) {
                    Some(copy) => match copy.update(x, y, width, height, stride, &data) {
                        Ok(()) => drawn = Some(console),
                        Err(e) => warn!(console, error = %format!("{e:#}"), "update not applied"),
                    },
                    None => debug!(console, "update without a copied framebuffer"),
                }
            }
            DisplayEvent::UpdateDmabuf {
                console,
                x,
                y,
                width,
                height,
            }
            | DisplayEvent::UpdateMap {
                console,
                x,
                y,
                width,
                height,
            } => {
                debug!(console, x, y, size = %format!("{width}x{height}"), "update");
                drawn = Some(console);
            }
            DisplayEvent::Disable { console } => {
                info!(console, "disabled");
                // A mode change disables the head for a moment: keep the encoder for when it
                // comes back.
                if let Some(encoder) = encoders.get_mut(&console) {
                    encoder.clear_framebuffer();
                }
                sizes.remove(&console);
                rebalance = true;
            }
            DisplayEvent::MouseSet {
                console,
                x,
                y,
                visible,
            } => {
                debug!(console, x, y, visible, "pointer");
                if let Some(&screen) = screen_of.get(&console) {
                    pointer_tx.send_if_modified(|pointers| {
                        std::mem::replace(&mut pointers[screen].visible, visible) != visible
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
                if let Some(&screen) = screen_of.get(&console) {
                    match pointer::shape(width, height, hot_x, hot_y, &data) {
                        Some(shape) => pointer_tx.send_modify(|pointers| {
                            pointers[screen].shape = Some(std::sync::Arc::new(shape))
                        }),
                        None => warn!(
                            console,
                            bytes = data.len(),
                            "pointer shape of an unexpected size"
                        ),
                    }
                }
            }
        }
        if rebalance {
            share(&encoders, &sizes, settings.bitrate, args.bitrates.min);
        }
        if let Some(console) = drawn {
            *updates.entry(console).or_default() += 1;
            fresh.remove(&console);
            if wanted(console)
                && let Some(encoder) = encoders.get(&console)
            {
                if encoder.since_last_frame() < FRAME_INTERVAL {
                    due.insert(console);
                } else if let Err(e) = encoder.frame() {
                    warn!(console, error = %e, "frame not encoded");
                }
            }
        }
        if events.is_empty() {
            for console in fresh.drain() {
                if wanted(console)
                    && let Some(encoder) = encoders.get(&console)
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
    drop(audio);
    Ok(())
}

/// A new DMA-BUF framebuffer on `console`: point its encoder at it, creating the encoder first
/// if needed.
#[allow(clippy::too_many_arguments)]
/// Share `total` kbit/s among the screens that are on: each gets a floor (its share of
/// `min_total`, or of `total` when it is lower), and the rest goes by area, so that the sum
/// stays within `total`.
fn share(
    encoders: &HashMap<u32, Encoder>,
    sizes: &HashMap<u32, (u32, u32)>,
    total: u32,
    min_total: u32,
) {
    let on: Vec<(u32, f64)> = encoders
        .keys()
        .filter_map(|c| {
            sizes
                .get(c)
                .map(|&(w, h)| (*c, f64::from(w) * f64::from(h)))
        })
        .collect();
    let Ok(count) = u32::try_from(on.len()) else {
        return;
    };
    if count == 0 {
        return;
    }
    let floor = (min_total / count).min(total / count);
    let rest = total.saturating_sub(floor * count);
    let area: f64 = on.iter().map(|(_, a)| a).sum();
    for (console, a) in on {
        let bitrate = floor + (f64::from(rest) * a / area.max(1.)) as u32;
        debug!(console, bitrate, "screen bitrate");
        if let Some(encoder) = encoders.get(&console) {
            encoder.set_bitrate(bitrate.max(1));
        }
    }
}

/// Where a console's new framebuffer is.
enum Source<'a> {
    /// GPU memory: a single linear plane.
    Dmabuf {
        planes: &'a [display::Plane],
        fourcc: u32,
        modifier: u64,
    },
    /// Memory shared with QEMU, or our copy of it.
    Shared {
        fd: &'a std::os::fd::OwnedFd,
        offset: u32,
        stride: u32,
        pixman_format: u32,
    },
}

/// A console has a new framebuffer: encode it from now on, with a new encoder if it has none.
#[allow(clippy::too_many_arguments)]
fn scanout(
    viewers: Option<Viewers>,
    args: &Args,
    settings: &Settings,
    encoders: &mut HashMap<u32, Encoder>,
    console: u32,
    width: u32,
    height: u32,
    source: Source,
) -> Result<()> {
    // Check the framebuffer before making an encoder for it.
    match &source {
        Source::Dmabuf {
            planes, modifier, ..
        } => {
            if planes.len() != 1 {
                bail!("{} planes, one expected", planes.len())
            }
            if *modifier != 0 {
                bail!("modifier {modifier:#x}, only linear buffers are handled");
            }
        }
        Source::Shared { pixman_format, .. } => {
            if qdw_video::encode::pixman_format(*pixman_format).is_none() {
                bail!("pixman format {pixman_format:#x} not handled");
            }
        }
    }
    let encoder = match encoders.entry(console) {
        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
        std::collections::hash_map::Entry::Vacant(entry) => entry.insert(Encoder::new(
            settings,
            stream_sink(args, console, viewers)?,
        )?),
    };
    match source {
        Source::Dmabuf { planes, fourcc, .. } => {
            let plane = &planes[0];
            encoder.set_framebuffer(&plane.fd, width, height, fourcc, plane.offset, plane.stride)
        }
        Source::Shared {
            fd,
            offset,
            stride,
            pixman_format,
        } => {
            let format = qdw_video::encode::pixman_format(pixman_format)
                .ok_or_else(|| anyhow::anyhow!("pixman format {pixman_format:#x}"))?;
            encoder.set_shared_framebuffer(fd, width, height, format, offset, stride)
        }
    }
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
