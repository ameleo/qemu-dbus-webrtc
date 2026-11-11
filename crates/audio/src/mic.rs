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
use tracing::warn;

use crate::Pcm;

/// The decoded sound of a client, as the microphone takes it.
pub const CANONICAL: &str = "audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=2";

/// The bytes of one second of [`CANONICAL`].
const CANONICAL_BYTES_PER_SECOND: u64 = 48_000 * 2 * 2;

/// What the buffer holds at most: 100 ms.
const MAX_DELAY_DIVISOR: usize = 10;

/// The microphone of the guest.
pub struct Microphone {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    format: gst::Element,
    /// Samples in the guest's format, and the most it may hold.
    buffer: Arc<Mutex<(VecDeque<u8>, usize)>>,
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
        let buffer = Arc::new(Mutex::new((VecDeque::new(), 0)));
        let filled = buffer.clone();
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |appsink| {
                    let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let samples = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = samples.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let (buffer, max) = &mut *filled.lock().unwrap();
                    buffer.extend(map.iter());
                    let excess = buffer.len().saturating_sub(*max);
                    buffer.drain(..excess);
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
        let max = info.bpf() as usize * info.rate() as usize / MAX_DELAY_DIVISOR;
        let (buffer, limit) = &mut *self.buffer.lock().unwrap();
        // Samples of the former format mean nothing in the new one.
        buffer.clear();
        *limit = max;
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

    /// Up to `size` bytes of samples in the guest's format, at once.
    pub fn read(&self, size: usize) -> Vec<u8> {
        let (buffer, _) = &mut *self.buffer.lock().unwrap();
        let size = size.min(buffer.len());
        buffer.drain(..size).collect()
    }
}

impl Drop for Microphone {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
