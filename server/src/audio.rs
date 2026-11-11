// SPDX-License-Identifier: MPL-2.0

//! The guest's sound: from QEMU to the Opus encoder, and to every viewer.

use anyhow::Result;
use qdw_audio::{AudioEncoder, Pcm};
use qdw_qemu::audio::{self, AudioEvent};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::session::{self, AudioViewers};

/// The guest's sound, while it lasts.
pub struct Audio {
    /// The listener connection: QEMU stops sending when it closes.
    _listener: Option<zbus::Connection>,
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
                    let pcm = Pcm {
                        bits: format.bits,
                        signed: format.signed,
                        float: format.float,
                        big_endian: format.big_endian,
                        rate: format.rate,
                        channels: format.channels,
                    };
                    match encoder.set_format(pcm) {
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
        encoder.finish();
    });
    Ok(Audio {
        _listener: listener,
        task,
    })
}
