// SPDX-License-Identifier: MPL-2.0

//! Opus encoding of the guest's sound.
//!
//! ```text
//! appsrc (guest samples) ! audioconvert ! audioresample ! volume ─┐
//!                                                                  audiomixer ! opusenc ! appsink
//! audiotestsrc (silence) ─────────────────────────────────────────┘
//! ```
//!
//! The silence keeps the stream going while the guest plays nothing: a WebRTC sender needs the
//! audio to flow before it can make its offer, and receivers expect a steady stream.
//!
//! The guest's samples are stamped by their count, each buffer starting where the previous
//! one ended: QEMU writes 10 ms every 10 ms on average, but with up to several milliseconds of
//! jitter, and stamping each buffer on arrival made gaps and overlaps, heard as crackles. The
//! stamps go back to the clock only when they drift from it, after the guest paused.

pub mod mic;

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_audio as gst_audio;
use tracing::warn;

/// How far the stamps may drift from the clock before they go back to it.
const RESYNC: Duration = Duration::from_millis(80);

/// The caps of the encoded stream, to feed it to another pipeline.
pub const STREAM_CAPS: &str = "audio/x-opus,channel-mapping-family=0,rate=48000,channels=2";

/// The sample format of the guest's sound, as QEMU gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pcm {
    pub bits: u8,
    pub signed: bool,
    pub float: bool,
    pub big_endian: bool,
    pub rate: u32,
    pub channels: u8,
}

impl Pcm {
    fn info(self) -> Option<gst_audio::AudioInfo> {
        use gst_audio::AudioFormat as F;
        let format = match (self.bits, self.float, self.signed, self.big_endian) {
            (8, false, true, _) => F::S8,
            (8, false, false, _) => F::U8,
            (16, false, true, false) => F::S16le,
            (16, false, true, true) => F::S16be,
            (16, false, false, false) => F::U16le,
            (16, false, false, true) => F::U16be,
            (32, false, true, false) => F::S32le,
            (32, false, true, true) => F::S32be,
            (32, true, _, false) => F::F32le,
            (32, true, _, true) => F::F32be,
            _ => return None,
        };
        gst_audio::AudioInfo::builder(format, self.rate, u32::from(self.channels))
            .build()
            .ok()
    }
}

/// The encoder of the guest's sound.
pub struct AudioEncoder {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    volume: gst::Element,
    /// The format of the guest's samples, and the stamp of the next buffer.
    stream: Mutex<Option<(gst_audio::AudioInfo, Option<gst::ClockTime>)>>,
}

impl AudioEncoder {
    /// Build and start the encoder. Its output goes to `sink`, called once per Opus frame
    /// (10 ms), on a GStreamer thread.
    pub fn new(sink: impl FnMut(&gst::Buffer) + Send + 'static) -> Result<Self> {
        let raw = "audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=2";
        let description = format!(
            "appsrc name=src is-live=true format=time \
             ! queue max-size-time=200000000 max-size-buffers=0 max-size-bytes=0 \
             ! audioconvert ! audioresample ! volume name=volume ! {raw} ! mix. \
             audiotestsrc wave=silence is-live=true samplesperbuffer=480 ! {raw} ! mix. \
             audiomixer name=mix latency=20000000 \
             ! opusenc audio-type=generic bitrate=128000 frame-size=10 \
             ! appsink name=sink sync=false"
        );
        let pipeline = gst::parse::launch(&description)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        let src = pipeline
            .by_name("src")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .ok_or_else(|| anyhow!("no appsrc"))?;
        let volume = pipeline
            .by_name("volume")
            .ok_or_else(|| anyhow!("no volume"))?;
        let appsink = pipeline
            .by_name("sink")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .ok_or_else(|| anyhow!("no appsink"))?;
        let sink = Mutex::new(sink);
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |appsink| {
                    let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer_owned().ok_or(gst::FlowError::Error)?;
                    (sink.lock().unwrap())(&buffer);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        let bus = pipeline.bus().ok_or_else(|| anyhow!("no bus"))?;
        bus.set_sync_handler(|_, message| {
            if let gst::MessageView::Error(error) = message.view() {
                warn!(
                    source = ?error.src().map(|s| s.path_string()),
                    error = %error.error(),
                    debug = ?error.debug(),
                    "audio pipeline error"
                );
            }
            gst::BusSyncReply::Drop
        });
        pipeline.set_state(gst::State::Playing)?;
        Ok(Self {
            pipeline,
            src,
            volume,
            stream: Mutex::new(None),
        })
    }

    /// The guest's samples now come in this format.
    pub fn set_format(&self, pcm: Pcm) -> Result<()> {
        let info = pcm
            .info()
            .ok_or_else(|| anyhow!("unsupported sample format {pcm:?}"))?;
        self.src.set_caps(Some(&info.to_caps()?));
        *self.stream.lock().unwrap() = Some((info, None));
        Ok(())
    }

    /// Samples of the guest, in the last format set.
    pub fn push(&self, samples: Vec<u8>) {
        let mut stream = self.stream.lock().unwrap();
        let Some((info, next)) = stream.as_mut() else {
            return;
        };
        let Some(now) = self.pipeline.current_running_time() else {
            return;
        };
        let frames = (samples.len() / info.bpf() as usize) as u64;
        let duration =
            gst::ClockTime::from_nseconds(frames * 1_000_000_000 / u64::from(info.rate()));
        // Where the previous buffer ended, unless that is too far from the clock.
        let resync = gst::ClockTime::from_nseconds(RESYNC.as_nanos() as u64);
        let pts = match *next {
            Some(pts) if pts.absdiff(now) < resync => pts,
            _ => now,
        };
        *next = Some(pts + duration);
        let mut buffer = gst::Buffer::from_mut_slice(samples);
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(pts);
            buffer.set_duration(duration);
        }
        // A full queue (the encoder stalled) drops samples rather than holding QEMU.
        let _ = self.src.push_buffer(buffer);
    }

    /// The guest's volume: muted, and per channel from 0 to 255. The guest's slider is a
    /// perceptual scale: a cubic curve turns it into a gain.
    pub fn set_volume(&self, mute: bool, volume: &[u8]) {
        let level = if volume.is_empty() {
            1.0
        } else {
            volume.iter().map(|&v| f64::from(v)).sum::<f64>() / volume.len() as f64 / 255.0
        };
        self.volume.set_property("volume", level.powi(3));
        self.volume.set_property("mute", mute);
    }

    pub fn finish(self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
