// SPDX-License-Identifier: MPL-2.0

//! The guest's sound, from QEMU's D-Bus audio backend (`-audiodev dbus`, used by the display).
//!
//! As for the display, a listener gets it: a socket pair, one end given to QEMU with
//! `RegisterOutListener`, on which we serve `org.qemu.Display1.AudioOutListener`. QEMU then
//! announces each playback stream (`Init`), its volume, and writes its samples as they come.
//!
//! The guest's microphone goes the other way, through `RegisterInListener` and
//! `org.qemu.Display1.AudioInListener`: QEMU announces each record stream and reads its samples
//! (`Read`), from its main loop, waiting for the answer: a [`Recorder`] answers at once with
//! what it has.

use std::os::unix::net::UnixStream as StdUnixStream;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use zbus::zvariant::OwnedFd;

#[zbus::proxy(
    interface = "org.qemu.Display1.Audio",
    default_service = "org.qemu",
    default_path = "/org/qemu/Display1/Audio"
)]
trait Audio {
    fn register_out_listener(&self, listener: OwnedFd) -> zbus::Result<()>;
    fn register_in_listener(&self, listener: OwnedFd) -> zbus::Result<()>;
}

/// The sample format of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub bits: u8,
    pub signed: bool,
    pub float: bool,
    pub big_endian: bool,
    pub rate: u32,
    pub channels: u8,
}

/// What the guest's sound does. Streams are told apart by their id.
#[derive(Debug)]
pub enum AudioEvent {
    /// A stream begins, in this format.
    Init { id: u64, format: Format },
    /// The stream ends.
    Fini { id: u64 },
    /// The stream plays, or pauses.
    Enabled { id: u64, enabled: bool },
    /// The stream's volume: muted, and per channel from 0 to 255.
    Volume {
        id: u64,
        mute: bool,
        volume: Vec<u8>,
    },
    /// Samples of the stream, interleaved, in its format.
    Write { id: u64, data: Vec<u8> },
}

/// `org.qemu.Display1.AudioOutListener`.
struct Listener {
    events: mpsc::UnboundedSender<AudioEvent>,
}

impl Listener {
    fn send(&self, event: AudioEvent) {
        // The receiver is gone only when shutting down.
        let _ = self.events.send(event);
    }
}

// Calls are handled one at a time, in the order QEMU sends them: samples must not overtake one
// another.
#[zbus::interface(name = "org.qemu.Display1.AudioOutListener", spawn = false)]
impl Listener {
    #[allow(clippy::too_many_arguments)]
    async fn init(
        &self,
        id: u64,
        bits: u8,
        is_signed: bool,
        is_float: bool,
        freq: u32,
        nchannels: u8,
        _bytes_per_frame: u32,
        _bytes_per_second: u32,
        be: bool,
    ) {
        let format = Format {
            bits,
            signed: is_signed,
            float: is_float,
            big_endian: be,
            rate: freq,
            channels: nchannels,
        };
        self.send(AudioEvent::Init { id, format });
    }

    async fn fini(&self, id: u64) {
        self.send(AudioEvent::Fini { id });
    }

    async fn set_enabled(&self, id: u64, enabled: bool) {
        self.send(AudioEvent::Enabled { id, enabled });
    }

    async fn set_volume(&self, id: u64, mute: bool, volume: Vec<u8>) {
        self.send(AudioEvent::Volume { id, mute, volume });
    }

    async fn write(&self, id: u64, data: Vec<u8>) {
        self.send(AudioEvent::Write { id, data });
    }

    #[zbus(property)]
    fn interfaces(&self) -> Vec<String> {
        Vec::new()
    }
}

const LISTENER_PATH: &str = "/org/qemu/Display1/AudioOutListener";

/// Listen to the guest's sound. The returned connection carries it: keep it alive for as long
/// as it is wanted.
pub async fn listen(
    vm: &zbus::Connection,
    events: mpsc::UnboundedSender<AudioEvent>,
) -> Result<zbus::Connection> {
    let audio = AudioProxy::new(vm).await?;
    let (ours, theirs) = StdUnixStream::pair()?;
    ours.set_nonblocking(true)?;
    let stream = tokio::net::UnixStream::from_std(ours)?;
    let connection = zbus::connection::Builder::unix_stream(stream)
        .p2p()
        .serve_at(LISTENER_PATH, Listener { events })?
        .build();
    // QEMU authenticates the listener connection before it answers: both at once.
    let theirs: std::os::fd::OwnedFd = theirs.into();
    let (connection, registered) =
        tokio::join!(connection, audio.register_out_listener(theirs.into()));
    registered.context("registering an audio listener")?;
    Ok(connection?)
}

/// What feeds the guest's microphone.
pub trait Recorder: Send + Sync + 'static {
    /// A record stream begins, in this format.
    fn init(&self, id: u64, format: Format);
    /// The stream ends.
    fn fini(&self, id: u64);
    /// Up to `size` bytes of samples, in the stream's format; fewer if there are not as many.
    /// Called from QEMU's main loop, which waits: return at once.
    fn read(&self, id: u64, size: u64) -> Vec<u8>;
}

/// `org.qemu.Display1.AudioInListener`.
struct InListener {
    recorder: std::sync::Arc<dyn Recorder>,
}

// One call at a time, in order, as for the playback.
#[zbus::interface(name = "org.qemu.Display1.AudioInListener", spawn = false)]
impl InListener {
    #[allow(clippy::too_many_arguments)]
    async fn init(
        &self,
        id: u64,
        bits: u8,
        is_signed: bool,
        is_float: bool,
        freq: u32,
        nchannels: u8,
        _bytes_per_frame: u32,
        _bytes_per_second: u32,
        be: bool,
    ) {
        let format = Format {
            bits,
            signed: is_signed,
            float: is_float,
            big_endian: be,
            rate: freq,
            channels: nchannels,
        };
        self.recorder.init(id, format);
    }

    async fn fini(&self, id: u64) {
        self.recorder.fini(id);
    }

    async fn set_enabled(&self, _id: u64, _enabled: bool) {}

    async fn set_volume(&self, _id: u64, _mute: bool, _volume: Vec<u8>) {}

    async fn read(&self, id: u64, size: u64) -> Vec<u8> {
        self.recorder.read(id, size)
    }

    #[zbus(property)]
    fn interfaces(&self) -> Vec<String> {
        Vec::new()
    }
}

const IN_LISTENER_PATH: &str = "/org/qemu/Display1/AudioInListener";

/// Feed the guest's microphone from `recorder`. The returned connection carries it: keep it
/// alive for as long as it is wanted.
pub async fn record(
    vm: &zbus::Connection,
    recorder: std::sync::Arc<dyn Recorder>,
) -> Result<zbus::Connection> {
    let audio = AudioProxy::new(vm).await?;
    let (ours, theirs) = StdUnixStream::pair()?;
    ours.set_nonblocking(true)?;
    let stream = tokio::net::UnixStream::from_std(ours)?;
    let connection = zbus::connection::Builder::unix_stream(stream)
        .p2p()
        .serve_at(IN_LISTENER_PATH, InListener { recorder })?
        .build();
    // QEMU authenticates the listener connection before it answers: both at once.
    let theirs: std::os::fd::OwnedFd = theirs.into();
    let (connection, registered) =
        tokio::join!(connection, audio.register_in_listener(theirs.into()));
    registered.context("registering a microphone listener")?;
    Ok(connection?)
}
