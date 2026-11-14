// SPDX-License-Identifier: MPL-2.0

//! The guest's sound in Opus, and the clients' sound for its microphone, with libopus and no
//! media framework.
//!
//! The encoder runs on a thread of its own, a 10 ms Opus frame every 10 ms, with or without
//! the guest's sound: a WebRTC sender needs the sound to flow before it can make its offer, and
//! receivers expect a steady stream. The guest's samples, converted to 48 kHz stereo, wait in
//! a queue that absorbs QEMU's jitter (it writes 10 ms every 10 ms on average, with up to
//! several milliseconds of jitter); a frame the queue cannot fill is completed with silence.
//! The frames are stamped by their count from the encoder's start.

pub mod mic;
mod opus;
mod pcm;
mod resample;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use tracing::warn;

pub use opus::Decoder;
pub use pcm::Pcm;
use resample::Resampler;

/// An Opus frame of 10 ms, 48 kHz stereo.
#[derive(Debug, Clone)]
pub struct OpusFrame {
    pub data: Arc<[u8]>,
    /// Its time from the encoder's start.
    pub pts: Duration,
}

/// The Opus bitrate, in bit/s.
const BITRATE: u32 = 128_000;

/// A frame: 10 ms at 48 kHz.
const FRAME: Duration = Duration::from_millis(10);
const FRAME_SAMPLES: usize = 480;

/// The guest's sound waits this long in the queue before it plays, after a start or an
/// interruption: the room for QEMU's jitter.
const PREBUFFER: usize = FRAME_SAMPLES * 2;

/// The queue holds this much at most: past it, the guest runs ahead of the encoder's clock,
/// and the queue goes back to `PREBUFFER`, so that the delay does not grow.
const MAX_QUEUE: usize = FRAME_SAMPLES * 8;

/// What the encoding thread and the guest share.
struct Shared {
    /// The guest's sound, 48 kHz stereo, interleaved.
    queue: VecDeque<f32>,
    /// Whether the queue is filling up before it plays.
    filling: bool,
    /// The guest's format and the resampler to 48 kHz.
    format: Option<(Pcm, Resampler)>,
    /// The gain of the guest's volume.
    gain: f32,
}

/// The encoder of the guest's sound.
pub struct AudioEncoder {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioEncoder {
    /// Start the encoder. Its output goes to `sink`, called once per Opus frame (10 ms), on
    /// the encoder's thread.
    pub fn new(sink: impl FnMut(&OpusFrame) + Send + 'static) -> Result<Self> {
        let encoder = opus::Encoder::new(BITRATE)?;
        let shared = Arc::new(Mutex::new(Shared {
            queue: VecDeque::new(),
            filling: true,
            format: None,
            gain: 1.0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("audio-encoder".into())
            .spawn({
                let (shared, stop) = (shared.clone(), stop.clone());
                move || run(encoder, &shared, &stop, sink)
            })?;
        Ok(Self {
            shared,
            stop,
            thread: Some(thread),
        })
    }

    /// The guest's samples now come in this format.
    pub fn set_format(&self, pcm: Pcm) -> Result<()> {
        if !pcm.is_supported() {
            return Err(anyhow!("unsupported sample format {pcm:?}"));
        }
        let resampler = Resampler::new(pcm.rate, opus::RATE, opus::CHANNELS)?;
        self.shared.lock().unwrap().format = Some((pcm, resampler));
        Ok(())
    }

    /// Samples of the guest, in the last format set: whole frames, a part of one left out (it
    /// would shift every later sample).
    pub fn push(&self, samples: Vec<u8>) {
        let mut shared = self.shared.lock().unwrap();
        let Some((pcm, resampler)) = &mut shared.format else {
            return;
        };
        let stereo = pcm::to_stereo(&pcm.to_float(&samples), usize::from(pcm.channels));
        let resampled = resampler.process(&stereo);
        let queue = &mut shared.queue;
        queue.extend(resampled);
        if queue.len() > MAX_QUEUE * opus::CHANNELS {
            let excess = queue.len() - PREBUFFER * opus::CHANNELS;
            queue.drain(..excess);
        }
    }

    /// The guest's volume: muted, and per channel from 0 to 255. The guest's slider is a
    /// perceptual scale: a cubic curve turns it into a gain.
    pub fn set_volume(&self, mute: bool, volume: &[u8]) {
        let level = if volume.is_empty() {
            1.0
        } else {
            volume.iter().map(|&v| f64::from(v)).sum::<f64>() / volume.len() as f64 / 255.0
        };
        self.shared.lock().unwrap().gain = if mute { 0.0 } else { level.powi(3) as f32 };
    }
}

/// Stopped however it goes: at the end of its task, or with the task aborted on shutdown.
impl Drop for AudioEncoder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The next frame: the guest's sound when the queue plays, silence where it runs short.
fn next_frame(shared: &Mutex<Shared>) -> Vec<i16> {
    let mut shared = shared.lock().unwrap();
    let wanted = FRAME_SAMPLES * opus::CHANNELS;
    if shared.filling && shared.queue.len() >= PREBUFFER * opus::CHANNELS {
        shared.filling = false;
    }
    let gain = shared.gain;
    let mut frame: Vec<f32> = if shared.filling {
        Vec::new()
    } else {
        let take = wanted.min(shared.queue.len());
        shared.queue.drain(..take).collect()
    };
    if frame.len() < wanted {
        // The guest stopped, or is late: silence, and the queue fills up again first.
        shared.filling = true;
        frame.resize(wanted, 0.0);
    }
    frame
        .iter()
        .map(|&s| ((s * gain).clamp(-1.0, 1.0) * 32767.0).round() as i16)
        .collect()
}

/// The encoding thread: a frame every 10 ms, on time, until told to stop.
fn run(
    mut encoder: opus::Encoder,
    shared: &Mutex<Shared>,
    stop: &AtomicBool,
    mut sink: impl FnMut(&OpusFrame),
) {
    let start = Instant::now();
    let mut count: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        let due = start + FRAME * count as u32;
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        } else if now - due > FRAME * 10 {
            // Far behind (the host stalled): the frames missed are not made up.
            count = ((now - start).as_nanos() / FRAME.as_nanos()) as u64;
            continue;
        }
        let samples = next_frame(shared);
        match encoder.encode(&samples) {
            Ok(packet) => sink(&OpusFrame {
                data: packet.into(),
                pts: Duration::from_nanos(count * FRAME.as_nanos() as u64),
            }),
            Err(error) => warn!(%error, "sound not encoded"),
        }
        count += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Mutex<Shared> {
        Mutex::new(Shared {
            queue: VecDeque::new(),
            filling: true,
            format: None,
            gain: 1.0,
        })
    }

    /// The queue plays once it holds `PREBUFFER`, fills up again after it ran short, and
    /// gives silence meanwhile.
    #[test]
    fn the_queue_absorbs_the_jitter() {
        let shared = shared();
        shared
            .lock()
            .unwrap()
            .queue
            .extend([0.5; FRAME_SAMPLES * 2]);
        // 10 ms waiting: not enough yet, silence.
        assert!(next_frame(&shared).iter().all(|&s| s == 0));
        shared
            .lock()
            .unwrap()
            .queue
            .extend([0.5; FRAME_SAMPLES * 2]);
        // 20 ms: it plays.
        assert!(next_frame(&shared).iter().all(|&s| s == 16384));
        assert!(next_frame(&shared).iter().all(|&s| s == 16384));
        // Empty: silence, and it waits for 20 ms again.
        assert!(next_frame(&shared).iter().all(|&s| s == 0));
        shared
            .lock()
            .unwrap()
            .queue
            .extend([0.5; FRAME_SAMPLES * 2]);
        assert!(next_frame(&shared).iter().all(|&s| s == 0));
    }

    /// Frames come out every 10 ms, stamped by their count, with or without the guest.
    #[test]
    fn frames_flow_without_the_guest() {
        let (tx, rx) = std::sync::mpsc::channel();
        let encoder = AudioEncoder::new(move |frame: &OpusFrame| {
            let _ = tx.send(frame.pts);
        })
        .unwrap();
        let stamps: Vec<_> = (0..5)
            .map(|_| rx.recv_timeout(Duration::from_secs(1)).unwrap())
            .collect();
        drop(encoder);
        for (i, stamp) in stamps.into_iter().enumerate() {
            assert_eq!(stamp, Duration::from_millis(10 * i as u64));
        }
    }

    #[test]
    fn formats_without_sound_are_refused() {
        let encoder = AudioEncoder::new(|_: &OpusFrame| {}).unwrap();
        let pcm = Pcm {
            bits: 16,
            signed: true,
            float: false,
            big_endian: false,
            rate: 0,
            channels: 2,
        };
        assert!(encoder.set_format(pcm).is_err());
        assert!(
            encoder
                .set_format(Pcm {
                    rate: 44_100,
                    ..pcm
                })
                .is_ok()
        );
    }
}
