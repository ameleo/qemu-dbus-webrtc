// SPDX-License-Identifier: MPL-2.0

//! A viewer's WebRTC connection, by one of two stacks: GStreamer's webrtcbin
//! ([`crate::session`]) or str0m ([`crate::peer`]), the one chosen in the configuration, until
//! str0m replaces webrtcbin. What both share: the signals a session gives, its data channels,
//! and the viewers the encoders feed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use gstreamer as gst;
use tokio::sync::mpsc;

use qdw_protocol::{
    CAMERA_CHANNEL, CLIPBOARD_CHANNEL, CONTROL_CHANNEL, DRIVE_CHANNEL, INPUT_CHANNEL,
    POINTER_CHANNEL, PRINTER_CHANNEL,
};
use qdw_video::encode::Codec;

use crate::audio::Mic;

/// The guest screens a session carries, at most: one video track each.
pub const MAX_SCREENS: usize = 4;

/// The WebRTC stack of the sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stack {
    /// GStreamer's webrtcbin.
    #[default]
    Webrtcbin,
    /// str0m, without a media framework.
    Str0m,
}

impl Stack {
    pub fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "webrtcbin" => Self::Webrtcbin,
            "str0m" => Self::Str0m,
            other => bail!("WebRTC stack {other}: webrtcbin or str0m"),
        })
    }
}

/// What the session has to send to the browser.
#[derive(Debug)]
pub enum Signal {
    Offer(String),
    IceCandidate {
        mline_index: u32,
        candidate: String,
    },
    /// A data channel is open: messages can go on it.
    Open(Channel),
    /// A message came on a data channel.
    Message(Channel, Vec<u8>),
    /// An H.264 access unit (Annex B) from a client camera, on the track of `mid`.
    CameraFrame {
        mid: String,
        timestamp_us: u64,
        data: Vec<u8>,
    },
    /// A new estimate of the bandwidth to the client, in kbit/s.
    Estimate(u32),
    /// The connection failed or ended; the session is over.
    Closed,
}

/// The data channels of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    /// Keyboard and pointer, from the client.
    Input,
    /// The guest pointer, to the client.
    Pointer,
    /// The clipboard, both ways.
    Clipboard,
    /// What the client wants of its session, from the client.
    Control,
    /// The client's drives, both ways.
    Drive,
    /// The client's printers, both ways.
    Printer,
    /// The client's cameras, both ways.
    Camera,
}

impl Channel {
    pub const ALL: [Self; 7] = [
        Self::Input,
        Self::Pointer,
        Self::Clipboard,
        Self::Control,
        Self::Drive,
        Self::Printer,
        Self::Camera,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Input => INPUT_CHANNEL,
            Self::Pointer => POINTER_CHANNEL,
            Self::Clipboard => CLIPBOARD_CHANNEL,
            Self::Control => CONTROL_CHANNEL,
            Self::Drive => DRIVE_CHANNEL,
            Self::Printer => PRINTER_CHANNEL,
            Self::Camera => CAMERA_CHANNEL,
        }
    }
}

/// The bounds of the bandwidth estimation, in kbit/s: the estimate starts at `start` and stays
/// within `min` and `max`.
#[derive(Debug, Clone, Copy)]
pub struct Bitrates {
    pub min: u32,
    pub start: u32,
    pub max: u32,
}

/// Where a stream's encoded frames go for one viewer, called on the encoder's thread.
pub struct Viewer {
    id: u64,
    push: Box<dyn FnMut(&gst::Buffer) + Send>,
}

impl Viewer {
    /// A viewer that takes the frames with `push`, under `id`.
    pub fn new(id: u64, push: impl FnMut(&gst::Buffer) + Send + 'static) -> Self {
        Self {
            id,
            push: Box::new(push),
        }
    }

    /// A new identity for a viewer: its session takes it out of the viewers by it (see
    /// [`leave`]).
    pub fn next_id() -> u64 {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }
}

/// The viewers of a stream: a console's video, or the guest's sound.
pub type Viewers = Arc<Mutex<Vec<Viewer>>>;

/// Send an encoded frame to every viewer of its stream.
pub fn fan_out(viewers: &Viewers, frame: &gst::Buffer) {
    for viewer in viewers.lock().unwrap().iter_mut() {
        (viewer.push)(frame);
    }
}

/// Take the viewer `id` out of `viewers`.
pub fn leave(viewers: &Viewers, id: u64) {
    viewers.lock().unwrap().retain(|v| v.id != id);
}

/// What a session is made of.
pub struct Start {
    pub codec: Codec,
    pub full_chroma: bool,
    /// In 10 bits, as the offer says.
    pub ten_bits: bool,
    /// The viewers of each screen's encoder, one video track each, in order.
    pub viewers: Vec<Viewers>,
    /// The viewers of the guest's sound.
    pub audio_viewers: Viewers,
    /// The guest's microphone, for a client that may speak in it.
    pub mic: Option<Arc<Mic>>,
    pub bitrates: Bitrates,
}

/// A viewer's connection, by either stack.
pub enum Session {
    Webrtcbin(crate::session::Session),
    Str0m(crate::peer::Peer),
}

impl Session {
    /// Start a session: its signals go to `signals`; `keyframe` is called with a screen's index
    /// when the client needs a keyframe of it (a lost frame, or the start).
    pub fn start(
        stack: Stack,
        start: Start,
        signals: mpsc::UnboundedSender<Signal>,
        keyframe: impl Fn(usize) + Send + Sync + 'static,
    ) -> Result<Self> {
        Ok(match stack {
            Stack::Webrtcbin => {
                Self::Webrtcbin(crate::session::Session::start(start, signals, keyframe)?)
            }
            Stack::Str0m => Self::Str0m(crate::peer::Peer::start(start, signals, keyframe)?),
        })
    }

    /// Pause or resume the video. True when the video resumes: the client then needs a keyframe.
    pub fn pause_video(&self, paused: bool) -> bool {
        match self {
            Self::Webrtcbin(s) => s.pause_video(paused),
            Self::Str0m(p) => p.pause_video(paused),
        }
    }

    /// The mids of the camera tracks, once negotiated.
    pub fn camera_mids(&self) -> Vec<String> {
        match self {
            Self::Webrtcbin(s) => s.camera_mids(),
            Self::Str0m(p) => p.camera_mids(),
        }
    }

    /// Ask the client for a keyframe on the camera track of `mid`.
    pub fn camera_keyframe(&self, mid: &str) {
        match self {
            Self::Webrtcbin(s) => s.camera_keyframe(mid),
            Self::Str0m(p) => p.camera_keyframe(mid),
        }
    }

    /// Send a message on a data channel.
    pub fn send(&self, channel: Channel, message: Vec<u8>) {
        match self {
            Self::Webrtcbin(s) => s.send(channel, message),
            Self::Str0m(p) => p.send(channel, message),
        }
    }

    /// Log what the client reports of its reception, at debug level.
    pub fn report_reception(&self) {
        match self {
            Self::Webrtcbin(s) => s.report_reception(),
            Self::Str0m(p) => p.report_reception(),
        }
    }

    pub fn apply_answer(&self, sdp: &str) -> Result<()> {
        match self {
            Self::Webrtcbin(s) => s.apply_answer(sdp),
            Self::Str0m(p) => p.apply_answer(sdp),
        }
    }

    pub fn add_ice_candidate(&self, mline_index: u32, candidate: &str) {
        match self {
            Self::Webrtcbin(s) => s.add_ice_candidate(mline_index, candidate),
            Self::Str0m(p) => p.add_ice_candidate(candidate),
        }
    }
}
