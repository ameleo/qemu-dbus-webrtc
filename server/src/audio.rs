// SPDX-License-Identifier: MPL-2.0

//! The guest's sound: from QEMU to the Opus encoder, and to every viewer; and the guest's
//! microphone: from a viewer to QEMU.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use qdw_audio::mic::Microphone;
use qdw_audio::{AudioEncoder, Pcm};
use qdw_qemu::audio::{self, AudioEvent, Format, Recorder};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::session::{self, AudioViewers};

/// How long a viewer keeps the microphone after it stopped sending sound.
const SPEAKER_HOLD: Duration = Duration::from_secs(1);

/// The guest's sound, while it lasts.
pub struct Audio {
    /// The listener connections: QEMU stops when they close.
    _listener: Option<zbus::Connection>,
    _recorder: Option<zbus::Connection>,
    pub mic: Arc<Mic>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Start encoding the guest's sound for `viewers`. Without sound on the VM, the viewers get
/// silence.
pub async fn start(vm: &zbus::Connection, viewers: AudioViewers) -> Result<Audio> {
    let encoder = AudioEncoder::new(move |frame| session::fan_out_audio(&viewers, frame))?;
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let listener = match audio::listen(vm, events_tx).await {
        Ok(connection) => Some(connection),
        Err(e) => {
            info!(error = %format!("{e:#}"), "no guest sound: viewers get silence");
            None
        }
    };
    let task = tokio::spawn(async move {
        // The stream being played: guests open one per playback device, and sometimes a new one
        // before closing the last.
        let mut playing = None;
        while let Some(event) = events.recv().await {
            match event {
                AudioEvent::Init { id, format } => {
                    debug!(id, ?format, "guest sound stream");
                    match encoder.set_format(pcm(format)) {
                        Ok(()) => playing = Some(id),
                        Err(e) => warn!(id, error = %e, "guest sound not played"),
                    }
                }
                AudioEvent::Fini { id } => {
                    if playing == Some(id) {
                        playing = None;
                    }
                }
                AudioEvent::Enabled { id, enabled } => debug!(id, enabled, "guest sound"),
                // One playback device: its volume, whichever stream it comes with, and even
                // before the stream's Init.
                AudioEvent::Volume { id, mute, volume } => {
                    debug!(id, mute, ?volume, "guest volume");
                    encoder.set_volume(mute, &volume);
                }
                AudioEvent::Write { id, data } => {
                    if playing == Some(id) {
                        encoder.push(data);
                    }
                }
            }
        }
    });
    let mic = Arc::new(Mic::new()?);
    let recorder = match audio::record(vm, mic.clone()).await {
        Ok(connection) => Some(connection),
        Err(e) => {
            info!(error = %format!("{e:#}"), "no guest microphone");
            None
        }
    };
    Ok(Audio {
        _listener: listener,
        _recorder: recorder,
        mic,
        task,
    })
}

fn pcm(format: Format) -> Pcm {
    Pcm {
        bits: format.bits,
        signed: format.signed,
        float: format.float,
        big_endian: format.big_endian,
        rate: format.rate,
        channels: format.channels,
    }
}

/// The guest's microphone, fed by one viewer at a time: the one that sends sound, until it has
/// sent nothing for a second.
pub struct Mic {
    microphone: Microphone,
    /// The guest's record stream.
    recording: Mutex<Option<u64>>,
    speaker: Mutex<Option<(u64, Instant)>>,
    viewers: AtomicU64,
}

impl Mic {
    fn new() -> Result<Self> {
        Ok(Self {
            microphone: Microphone::new()?,
            recording: Mutex::new(None),
            speaker: Mutex::new(None),
            viewers: AtomicU64::new(0),
        })
    }

    /// An id for a viewer that may speak.
    pub fn viewer(&self) -> u64 {
        self.viewers.fetch_add(1, Ordering::Relaxed)
    }

    /// A viewer's decoded sound, in `qdw_audio::mic::CANONICAL`.
    pub fn push(&self, viewer: u64, samples: &[u8]) {
        {
            let mut speaker = self.speaker.lock().unwrap();
            match *speaker {
                Some((current, last)) if current != viewer && last.elapsed() < SPEAKER_HOLD => {
                    return;
                }
                Some((current, _)) if current == viewer => {}
                _ => debug!(viewer, "viewer speaks"),
            }
            *speaker = Some((viewer, Instant::now()));
        }
        self.microphone.push(samples);
    }
}

impl Recorder for Mic {
    fn init(&self, id: u64, format: Format) {
        debug!(id, ?format, "guest records");
        match self.microphone.set_format(pcm(format)) {
            Ok(()) => *self.recording.lock().unwrap() = Some(id),
            Err(e) => warn!(id, error = %e, "guest microphone not fed"),
        }
    }

    fn fini(&self, id: u64) {
        let mut recording = self.recording.lock().unwrap();
        if *recording == Some(id) {
            *recording = None;
        }
    }

    fn read(&self, id: u64, size: u64) -> Vec<u8> {
        if *self.recording.lock().unwrap() != Some(id) {
            return Vec::new();
        }
        self.microphone
            .read(usize::try_from(size).unwrap_or(usize::MAX))
    }
}
