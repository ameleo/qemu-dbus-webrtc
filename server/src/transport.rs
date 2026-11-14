// SPDX-License-Identifier: MPL-2.0

//! What the sessions of the viewers ([`crate::peer`]) share with the rest of the server: the
//! signals a session gives, its data channels, and the viewers the encoders feed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use qdw_audio::OpusFrame;
use qdw_protocol::{
    CAMERA_CHANNEL, CLIPBOARD_CHANNEL, CONTROL_CHANNEL, DRIVE_CHANNEL, INPUT_CHANNEL,
    POINTER_CHANNEL, PRINTER_CHANNEL,
};
use qdw_video::encode::{Codec, EncodedFrame};

use crate::audio::Mic;

/// The guest screens a session carries, at most: one video track each.
pub const MAX_SCREENS: usize = 4;

/// What the session has to send to the browser.
#[derive(Debug)]
pub enum Signal {
    /// The offer, with the server's candidates in it.
    Offer(String),
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

/// Where a stream's encoded frames `F` go for one viewer, called on the encoder's thread.
pub struct Viewer<F> {
    id: u64,
    push: Box<dyn FnMut(&F) + Send>,
}

impl<F> Viewer<F> {
    /// A viewer that takes the frames with `push`, under `id`.
    pub fn new(id: u64, push: impl FnMut(&F) + Send + 'static) -> Self {
        Self {
            id,
            push: Box::new(push),
        }
    }
}

/// A new identity for a viewer: its session takes it out of the viewers by it (see
/// [`leave`]).
pub fn viewer_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The viewers of a stream: a console's video, or the guest's sound.
pub type Viewers<F> = Arc<Mutex<Vec<Viewer<F>>>>;

/// The viewers of a console's video.
pub type VideoViewers = Viewers<EncodedFrame>;

/// The viewers of the guest's sound.
pub type SoundViewers = Viewers<OpusFrame>;

/// Send an encoded frame to every viewer of its stream.
pub fn fan_out<F>(viewers: &Viewers<F>, frame: &F) {
    for viewer in viewers.lock().unwrap().iter_mut() {
        (viewer.push)(frame);
    }
}

/// Take the viewer `id` out of `viewers`.
pub fn leave<F>(viewers: &Viewers<F>, id: u64) {
    viewers.lock().unwrap().retain(|v| v.id != id);
}

/// What a session is made of.
pub struct Start {
    pub codec: Codec,
    pub full_chroma: bool,
    /// In 10 bits, as the offer says.
    pub ten_bits: bool,
    /// The viewers of each screen's encoder, one video track each, in order.
    pub viewers: Vec<VideoViewers>,
    /// The viewers of the guest's sound.
    pub audio_viewers: SoundViewers,
    /// The guest's microphone, for a client that may speak in it.
    pub mic: Option<Arc<Mic>>,
    pub bitrates: Bitrates,
}

pub use crate::peer::Peer as Session;
