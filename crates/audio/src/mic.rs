// SPDX-License-Identifier: MPL-2.0

//! The guest's microphone: the clients' sound, converted to the format the guest records in.
//!
//! ```text
//! appsrc (CANONICAL) ! audioconvert ! audioresample ! guest format ! appsink -> buffer
//! ```
//!
//! QEMU reads the buffer when it wants, and waits for the answer: the buffer gives at once what
//! it holds. It keeps 100 ms at most, the oldest dropped first, so that the delay cannot grow.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_audio as gst_audio;
use tracing::warn;

use crate::Pcm;

/// The decoded sound of a client, as the microphone takes it.
pub const CANONICAL: &str = "audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=2";

/// The bytes of one second of [`CANONICAL`].
const CANONICAL_BYTES_PER_SECOND: u64 = 48_000 * 2 * 2;

/// What the buffer holds at most: 100 ms.
const MAX_DELAY_DIVISOR: usize = 10;

/// Samples in the guest's format.
struct Samples {
    bytes: VecDeque<u8>,
    /// The most it holds, in bytes: a whole number of frames.
    max: usize,
    /// The bytes of one frame. Whatever comes in and goes out does in whole frames: a part of
    /// one would shift every later sample.
    frame: usize,
    /// The format of the samples it takes (sample type, rate, channels): a buffer in another
    /// one, converted before the format changed and arriving after, is not the guest's sound.
    format: Option<(gst_audio::AudioFormat, u32, u32)>,
}

impl Samples {
    /// Add samples, in whole frames (a part of one at the end is left out), then drop the
    /// oldest past `max`.
    fn append(&mut self, samples: &[u8]) {
        let whole = samples.len() - samples.len() % self.frame;
        self.bytes.extend(&samples[..whole]);
        self.trim();
    }

    /// Drop the oldest frames past `max`.
    fn trim(&mut self) {
        let excess = self.bytes.len().saturating_sub(self.max);
        let excess = excess.div_ceil(self.frame) * self.frame;
        self.bytes.drain(..excess.min(self.bytes.len()));
    }

    /// Up to `size` bytes, in whole frames.
    fn take(&mut self, size: usize) -> Vec<u8> {
        let size = size.min(self.bytes.len());
        let size = size - size % self.frame;
        self.bytes.drain(..size).collect()
    }
}

/// The microphone of the guest.
pub struct Microphone {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    format: gst::Element,
    /// Samples in the guest's format, whole frames only (all channels of one instant).
    buffer: Arc<Mutex<Samples>>,
    /// Samples pushed so far, to stamp the next ones.
    pushed: Mutex<u64>,
}

impl Microphone {
    pub fn new() -> Result<Self> {
        let description = format!(
            "appsrc name=src format=time caps={CANONICAL} \
             ! audioconvert ! audioresample ! capsfilter name=format \
             ! appsink name=sink sync=false"
        );
        let pipeline = gst::parse::launch(&description)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        let src = pipeline
            .by_name("src")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .ok_or_else(|| anyhow!("no appsrc"))?;
        let format = pipeline
            .by_name("format")
            .ok_or_else(|| anyhow!("no capsfilter"))?;
        let appsink = pipeline
            .by_name("sink")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .ok_or_else(|| anyhow!("no appsink"))?;
        let buffer = Arc::new(Mutex::new(Samples {
            bytes: VecDeque::new(),
            max: 0,
            frame: 1,
            format: None,
        }));
        let filled = buffer.clone();
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |appsink| {
                    let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let samples = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = samples.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let mut samples = filled.lock().unwrap();
                    // Only in the format the guest records in now.
                    let format = sample
                        .caps()
                        .and_then(|caps| gst_audio::AudioInfo::from_caps(caps).ok())
                        .map(|info| (info.format(), info.rate(), info.channels()));
                    if format.is_some() && format == samples.format {
                        samples.append(&map);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        let bus = pipeline.bus().ok_or_else(|| anyhow!("no bus"))?;
        bus.set_sync_handler(|_, message| {
            if let gst::MessageView::Error(error) = message.view() {
                warn!(error = %error.error(), debug = ?error.debug(), "microphone pipeline error");
            }
            gst::BusSyncReply::Drop
        });
        pipeline.set_state(gst::State::Playing)?;
        Ok(Self {
            pipeline,
            src,
            format,
            buffer,
            pushed: Mutex::new(0),
        })
    }

    /// The guest now records in this format.
    pub fn set_format(&self, pcm: Pcm) -> Result<()> {
        let info = pcm
            .info()
            .ok_or_else(|| anyhow!("unsupported sample format {pcm:?}"))?;
        self.format.set_property("caps", info.to_caps()?);
        let frame = (info.bpf() as usize).max(1);
        let mut samples = self.buffer.lock().unwrap();
        // Samples of the former format mean nothing in the new one.
        samples.bytes.clear();
        samples.format = Some((info.format(), info.rate(), info.channels()));
        samples.frame = frame;
        samples.max = frame * (info.rate() as usize / MAX_DELAY_DIVISOR);
        Ok(())
    }

    /// A client's sound, in [`CANONICAL`].
    pub fn push(&self, samples: &[u8]) {
        let mut pushed = self.pushed.lock().unwrap();
        let nanos = |bytes: u64| bytes * 1_000_000_000 / CANONICAL_BYTES_PER_SECOND;
        let start = gst::ClockTime::from_nseconds(nanos(*pushed));
        *pushed += samples.len() as u64;
        let mut buffer = gst::Buffer::from_slice(samples.to_vec());
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(start);
            buffer.set_duration(gst::ClockTime::from_nseconds(nanos(samples.len() as u64)));
        }
        let _ = self.src.push_buffer(buffer);
    }

    /// Up to `size` bytes of samples in the guest's format, at once, in whole frames.
    pub fn read(&self, size: usize) -> Vec<u8> {
        self.buffer.lock().unwrap().take(size)
    }
}

impl Drop for Microphone {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client's sound comes out in the guest's format, through the real pipeline: the format
    /// check lets the guest's own format through, and what comes out is whole frames.
    #[test]
    fn a_client_is_heard_in_the_guest_format() {
        gst::init().unwrap();
        let mic = Microphone::new().unwrap();
        // 44.1 kHz mono, 16 bits: frames of 2 bytes.
        mic.set_format(Pcm {
            bits: 16,
            signed: true,
            float: false,
            big_endian: false,
            rate: 44_100,
            channels: 1,
        })
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let heard = loop {
            assert!(std::time::Instant::now() < deadline, "nothing heard");
            // 10 ms of the canonical format.
            mic.push(&[0x11; 480 * 4]);
            std::thread::sleep(std::time::Duration::from_millis(10));
            let heard = mic.read(4096);
            if !heard.is_empty() {
                break heard;
            }
        };
        assert_eq!(heard.len() % 2, 0);
    }

    #[test]
    fn whole_frames_only() {
        // Frames of 4 bytes (16-bit stereo), 3 at most.
        let mut samples = Samples {
            bytes: (0..14).collect(),
            max: 12,
            frame: 4,
            format: None,
        };
        // 14 bytes, 12 at most: the oldest whole frame goes, not 2 bytes of it.
        samples.trim();
        assert_eq!(samples.bytes.len(), 10);
        assert_eq!(samples.bytes.front(), Some(&4));
        // 6 bytes asked: one whole frame given.
        assert_eq!(samples.take(6), [4, 5, 6, 7]);
        assert_eq!(samples.take(100), [8, 9, 10, 11]);
        // What is left is less than a frame: nothing.
        assert!(samples.take(100).is_empty());
        // What comes in does in whole frames too: 6 bytes are one frame, the rest left out.
        samples.bytes.clear();
        samples.append(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(samples.bytes.len(), 4);
        samples.append(&[7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18]);
        assert_eq!(
            samples.take(100),
            [7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18]
        );
    }
}
