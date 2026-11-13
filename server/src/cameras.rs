// SPDX-License-Identifier: MPL-2.0

//! Client cameras, shown in the guest as cameras through the guest agent.
//!
//! The user's client announces its cameras, with their formats and controls; the agent shows
//! each in the guest. When an application opens one, the agent asks for a format: the server
//! asks the client to send it on one of the offer's camera tracks, and relays the H.264 that
//! comes on it to the agent as it is. When the viewer goes, its cameras go too; when the agent
//! comes back (the guest restarted), they are added again.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use futures_util::StreamExt;
use qdw_protocol::Message as _;
use qdw_protocol::v1::{
    CameraAdded, CameraControl, CameraControlValue, CameraFormat, CameraMessage, CameraStart,
    CameraStop, camera_message::Message,
};
use qdw_qemu::agent::{CameraProxy, camera};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::session::Session;

/// Access units waiting for the guest, at most: about a second at 30 frames a second. Past
/// that the guest takes them more slowly than they come, and they are dropped.
const QUEUED_FRAMES: usize = 32;

/// An access unit for the guest's camera: its id, the capture time, the H.264.
type Frame = (u32, u64, Vec<u8>);

/// Whether an H.264 access unit (Annex B: each NAL unit after a 0x000001 start code) decodes
/// on its own: it has a sequence parameter set (nal_unit_type 7) or an IDR slice (5) before its
/// first other slice (1). The agent's contract puts the SPS and PPS before each IDR.
fn keyframe(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < au.len() {
        if au[i..i + 3] == [0, 0, 1] {
            match au[i + 3] & 0x1f {
                5 | 7 => return true,
                1 => return false,
                _ => {}
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// Push the access units of `frames` to the guest's cameras, in their order: one call after the
/// other, apart from the viewer's loop.
fn pusher(cameras: Arc<Cameras>) -> mpsc::Sender<Frame> {
    let (tx, mut frames) = mpsc::channel::<Frame>(QUEUED_FRAMES);
    tokio::spawn(async move {
        while let Some((id, timestamp_us, data)) = frames.recv().await {
            if let Some(agent) = cameras.agent()
                && let Err(e) = agent.push_frame(id, timestamp_us, data).await
            {
                debug!(id, error = %e, "camera frame not pushed");
            }
        }
    });
    tx
}

/// What the guest asks of a viewer's camera.
#[derive(Debug)]
pub enum CameraCall {
    /// An application opened it, in the format of this index.
    Start { client_id: u32, format: u32 },
    /// No application uses it anymore.
    Stop { client_id: u32 },
    /// The guest's decoder needs a keyframe.
    Keyframe { client_id: u32 },
    /// An application changed a control.
    Control {
        client_id: u32,
        set: u8,
        property: u8,
        mode: u8,
        value: i32,
    },
}

/// A camera shown in the guest.
#[derive(Clone)]
struct Camera {
    viewer: u64,
    client_id: u32,
    name: String,
    formats: Vec<(u32, u32, u32, u32)>,
    controls: Vec<camera::Control>,
    calls: mpsc::UnboundedSender<CameraCall>,
    /// To tell the viewer what became of its camera, again when the agent comes back.
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
}

/// The cameras of all viewers.
pub struct Cameras {
    agent: Mutex<Option<CameraProxy<'static>>>,
    /// The task that follows the agent's signals.
    signals: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// By the guest's id of the camera.
    cameras: Mutex<HashMap<u32, Camera>>,
    next_id: AtomicU32,
    viewers: AtomicU64,
}

fn encode(message: Message) -> Vec<u8> {
    CameraMessage {
        message: Some(message),
    }
    .encode_to_vec()
}

fn added(client_id: u32, error: String) -> Vec<u8> {
    encode(Message::Added(CameraAdded {
        id: client_id,
        error,
    }))
}

/// Show `camera` in the guest as `id`, with its controls.
async fn add(agent: &CameraProxy<'static>, id: u32, camera: &Camera) -> Result<(), String> {
    agent
        .add_camera(id, &camera.name, camera.formats.clone())
        .await
        .map_err(|e| e.to_string())?;
    if !camera.controls.is_empty() {
        agent
            .set_controls(id, camera.controls.clone())
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

impl Cameras {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            agent: Mutex::new(None),
            signals: Mutex::new(None),
            cameras: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            viewers: AtomicU64::new(0),
        })
    }

    fn agent(&self) -> Option<CameraProxy<'static>> {
        self.agent.lock().unwrap().clone()
    }

    /// Send `call` to the viewer of the guest's camera `id`.
    fn route(&self, id: u32, call: impl FnOnce(u32) -> CameraCall) {
        let cameras = self.cameras.lock().unwrap();
        if let Some(camera) = cameras.get(&id) {
            let _ = camera.calls.send(call(camera.client_id));
        }
    }

    /// The agent is connected: follow its signals, and add the cameras the viewers announced.
    pub async fn attach(self: &Arc<Self>, connection: &zbus::Connection) -> Result<()> {
        let proxy = CameraProxy::new(connection).await?;
        let mut requested = proxy.receive_stream_requested().await?;
        let mut stopped = proxy.receive_stream_stopped().await?;
        let mut keyframes = proxy.receive_key_frame_needed().await?;
        let mut controls = proxy.receive_control_requested().await?;
        let cameras = self.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(signal) = requested.next() => if let Ok(args) = signal.args() {
                        let format = args.format;
                        info!(id = args.id, format, "guest camera opened");
                        cameras.route(args.id, |client_id| CameraCall::Start { client_id, format });
                    },
                    Some(signal) = stopped.next() => if let Ok(args) = signal.args() {
                        info!(id = args.id, "guest camera closed");
                        cameras.route(args.id, |client_id| CameraCall::Stop { client_id });
                    },
                    Some(signal) = keyframes.next() => if let Ok(args) = signal.args() {
                        info!(id = args.id, "guest camera needs a keyframe");
                        cameras.route(args.id, |client_id| CameraCall::Keyframe { client_id });
                    },
                    Some(signal) = controls.next() => if let Ok(args) = signal.args() {
                        let (set, property, mode, value) = (args.set, args.property, args.mode, args.value);
                        cameras.route(args.id, |client_id| CameraCall::Control {
                            client_id, set, property, mode, value,
                        });
                    },
                    else => break,
                }
            }
        });
        if let Some(old) = self.signals.lock().unwrap().replace(task) {
            old.abort();
        }
        *self.agent.lock().unwrap() = Some(proxy.clone());
        let cameras: Vec<(u32, Camera)> = self
            .cameras
            .lock()
            .unwrap()
            .iter()
            .map(|(&id, c)| (id, c.clone()))
            .collect();
        for (id, camera) in cameras {
            let error = add(&proxy, id, &camera).await.err().unwrap_or_default();
            let _ = camera.outgoing.send(added(camera.client_id, error));
        }
        Ok(())
    }

    /// The agent went away.
    pub fn detach(&self) {
        *self.agent.lock().unwrap() = None;
        if let Some(task) = self.signals.lock().unwrap().take() {
            task.abort();
        }
    }

    /// A viewer's side of the cameras, with what its loop follows.
    pub fn viewer(self: &Arc<Self>) -> (ViewerCameras, CamerasFollow) {
        let (calls_tx, calls) = mpsc::unbounded_channel();
        let (outgoing_tx, outgoing) = mpsc::unbounded_channel();
        let viewer = ViewerCameras {
            cameras: self.clone(),
            id: self.viewers.fetch_add(1, Ordering::Relaxed),
            calls_tx,
            outgoing: outgoing_tx,
            streams: HashMap::new(),
            waiting: HashMap::new(),
            frames: None,
            broken: HashSet::new(),
        };
        (viewer, CamerasFollow { calls, outgoing })
    }
}

/// What a viewer's loop follows for the cameras.
pub struct CamerasFollow {
    /// What the guest asks of this viewer's cameras.
    pub calls: mpsc::UnboundedReceiver<CameraCall>,
    /// Messages to send to the client, from work done apart.
    pub outgoing: mpsc::UnboundedReceiver<Vec<u8>>,
}

/// One viewer's side of the cameras.
pub struct ViewerCameras {
    cameras: Arc<Cameras>,
    id: u64,
    calls_tx: mpsc::UnboundedSender<CameraCall>,
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
    /// The camera tracks in use: by mid, the guest's id of the camera streaming on it.
    streams: HashMap<String, u32>,
    /// The tracks that have not brought a frame since their camera started: when it started.
    waiting: HashMap<String, std::time::Instant>,
    /// The access units on their way to the guest, once a camera streamed.
    frames: Option<mpsc::Sender<Frame>>,
    /// The tracks that lost an access unit: the next ones would not decode, so none goes to
    /// the guest before a keyframe.
    broken: HashSet<String>,
}

/// A client's format as the agent takes it, if it does: even sizes, within the agent's limits.
fn format(f: &CameraFormat) -> Option<(u32, u32, u32, u32)> {
    let fits = f.width > 0
        && f.height > 0
        && f.width.is_multiple_of(2)
        && f.height.is_multiple_of(2)
        && f.width <= camera::MAX_WIDTH
        && f.height <= camera::MAX_HEIGHT
        && f.fps_numerator > 0
        && f.fps_denominator > 0;
    fits.then_some((f.width, f.height, f.fps_numerator, f.fps_denominator))
}

/// A client's control as the agent takes it, if its numbers fit.
fn control(c: &CameraControl) -> Option<camera::Control> {
    Some((
        u8::try_from(c.set).ok()?,
        u8::try_from(c.property).ok()?,
        u8::try_from(c.capabilities).ok()?,
        c.min,
        c.max,
        c.step,
        c.default,
        u8::try_from(c.mode).ok()?,
        c.value,
    ))
}

impl ViewerCameras {
    /// The guest's id of this viewer's camera `client_id`.
    fn guest_id(&self, client_id: u32) -> Option<u32> {
        let cameras = self.cameras.cameras.lock().unwrap();
        cameras
            .iter()
            .find(|(_, c)| c.viewer == self.id && c.client_id == client_id)
            .map(|(&id, _)| id)
    }

    /// What the guest asks of one of this viewer's cameras: the messages for the client.
    pub fn call(&mut self, call: CameraCall, session: &Session) -> Vec<Vec<u8>> {
        match call {
            CameraCall::Start { client_id, format } => {
                let Some(id) = self.guest_id(client_id) else {
                    return Vec::new();
                };
                // Its track if it streams already (another format), a free one otherwise.
                let mid = match self.streams.iter().find(|&(_, &g)| g == id) {
                    Some((mid, _)) => mid.clone(),
                    None => {
                        let free = session
                            .camera_mids()
                            .into_iter()
                            .find(|mid| !self.streams.contains_key(mid));
                        let Some(mid) = free else {
                            warn!(id, "no free camera track");
                            return Vec::new();
                        };
                        mid
                    }
                };
                info!(id, format, mid, "client camera asked to stream");
                self.streams.insert(mid.clone(), id);
                self.waiting.insert(mid.clone(), std::time::Instant::now());
                vec![encode(Message::Start(CameraStart {
                    id: client_id,
                    format,
                    mid,
                }))]
            }
            CameraCall::Stop { client_id } => {
                if let Some(id) = self.guest_id(client_id) {
                    self.streams.retain(|_, &mut g| g != id);
                    let streams = &self.streams;
                    self.broken.retain(|mid| streams.contains_key(mid));
                }
                vec![encode(Message::Stop(CameraStop { id: client_id }))]
            }
            CameraCall::Keyframe { client_id } => {
                if let Some(id) = self.guest_id(client_id)
                    && let Some((mid, _)) = self.streams.iter().find(|&(_, &g)| g == id)
                {
                    session.camera_keyframe(mid);
                }
                Vec::new()
            }
            CameraCall::Control {
                client_id,
                set,
                property,
                mode,
                value,
            } => vec![encode(Message::SetControl(CameraControlValue {
                id: client_id,
                set: set.into(),
                property: property.into(),
                mode: mode.into(),
                value,
            }))],
        }
    }

    /// An access unit came on the camera track of `mid`: to the guest's camera streaming on it,
    /// in order.
    pub fn frame(&mut self, mid: &str, timestamp_us: u64, data: Vec<u8>, session: &Session) {
        tracing::trace!(mid, timestamp_us, bytes = data.len(), "camera frame");
        let keyframe = keyframe(&data);
        if let Some(asked) = self.waiting.remove(mid) {
            info!(
                mid,
                after_ms = asked.elapsed().as_millis() as u64,
                keyframe,
                bytes = data.len(),
                "first camera frame"
            );
        }
        let Some(&id) = self.streams.get(mid) else {
            return;
        };
        if self.broken.contains(mid) {
            if !keyframe {
                return;
            }
            self.broken.remove(mid);
        }
        let cameras = &self.cameras;
        let frames = self.frames.get_or_insert_with(|| pusher(cameras.clone()));
        // UNTESTED: a guest slower than the camera (the drop, the keyframe asked, the frames
        // held back until it comes) needs a session's pipeline to ask for the keyframe.
        if frames.try_send((id, timestamp_us, data)).is_err() {
            warn!(
                mid,
                "the guest takes the camera's frames too slowly: dropped until a keyframe"
            );
            self.broken.insert(mid.to_string());
            session.camera_keyframe(mid);
        }
    }

    /// A data channel message from the client.
    pub async fn message(&mut self, bytes: &[u8]) -> Result<()> {
        let message = CameraMessage::decode(bytes)?;
        match message.message {
            Some(Message::Announce(announce)) => self.announce(announce),
            Some(Message::Remove(remove)) => {
                if let Some(id) = self.guest_id(remove.id) {
                    self.remove(id).await;
                }
            }
            Some(Message::ControlValue(v)) => {
                if let (Some(id), Some(agent)) = (self.guest_id(v.id), self.cameras.agent())
                    && let (Ok(set), Ok(property), Ok(mode)) = (
                        u8::try_from(v.set),
                        u8::try_from(v.property),
                        u8::try_from(v.mode),
                    )
                {
                    agent
                        .control_value(id, set, property, mode, v.value)
                        .await?;
                }
            }
            // Server to client only, or empty.
            Some(
                Message::Added(_) | Message::Start(_) | Message::Stop(_) | Message::SetControl(_),
            )
            | None => {}
        }
        Ok(())
    }

    fn announce(&mut self, announce: qdw_protocol::v1::CameraAnnounce) {
        let client_id = announce.id;
        let formats: Vec<_> = announce.formats.iter().filter_map(format).collect();
        let controls: Vec<_> = announce
            .controls
            .iter()
            .filter_map(control)
            .take(camera::MAX_CONTROLS)
            .collect();
        if formats.len() != announce.formats.len() || formats.is_empty() {
            let why = "a format the guest does not take (even sizes up to 3840x2160)";
            let _ = self.outgoing.send(added(client_id, why.into()));
            return;
        }
        let camera = Camera {
            viewer: self.id,
            client_id,
            name: announce.name,
            formats,
            controls,
            calls: self.calls_tx.clone(),
            outgoing: self.outgoing.clone(),
        };
        // Checked and added under one lock: two viewers announcing at once stay within the
        // limit.
        let added_as = {
            let mut cameras = self.cameras.cameras.lock().unwrap();
            let announced = cameras
                .values()
                .any(|c| c.viewer == self.id && c.client_id == client_id);
            if announced {
                Err("this camera id is announced already")
            } else if cameras.len() >= camera::MAX_CAMERAS {
                Err("too many cameras")
            } else {
                let id = self.cameras.next_id.fetch_add(1, Ordering::Relaxed);
                cameras.insert(id, camera.clone());
                Ok(id)
            }
        };
        let id = match added_as {
            Ok(id) => id,
            Err(why) => {
                let _ = self.outgoing.send(added(client_id, why.into()));
                return;
            }
        };
        let agent = self.cameras.agent();
        tokio::spawn(async move {
            let result = match agent {
                Some(agent) => add(&agent, id, &camera).await,
                None => Err("no guest agent: the camera is added when it comes".into()),
            };
            match &result {
                Ok(()) => info!(id, name = camera.name, "client camera added to the guest"),
                Err(e) => warn!(id, name = camera.name, error = e, "client camera not added"),
            }
            let _ = camera
                .outgoing
                .send(added(client_id, result.err().unwrap_or_default()));
        });
    }

    async fn remove(&mut self, id: u32) {
        self.cameras.cameras.lock().unwrap().remove(&id);
        self.streams.retain(|_, &mut g| g != id);
        let streams = &self.streams;
        self.broken.retain(|mid| streams.contains_key(mid));
        if let Some(agent) = self.cameras.agent()
            && let Err(e) = agent.remove_camera(id).await
        {
            debug!(id, error = %e, "client camera not removed from the guest");
        }
    }

    /// The viewer went away: its cameras leave the guest.
    pub async fn release(&mut self) {
        let ids: Vec<u32> = {
            let cameras = self.cameras.cameras.lock().unwrap();
            cameras
                .iter()
                .filter(|(_, c)| c.viewer == self.id)
                .map(|(&id, _)| id)
                .collect()
        };
        for id in ids {
            self.remove(id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframes_are_told_apart() {
        // SPS, PPS, IDR slice; with an access unit delimiter first, and 3-byte start codes.
        assert!(keyframe(&[
            0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 1, 0x65, 3
        ]));
        assert!(keyframe(&[0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x65, 3]));
        // A non-IDR slice, even with an SPS-looking byte in its data.
        assert!(!keyframe(&[0, 0, 0, 1, 0x41, 0x67, 0, 0, 1, 0x67]));
        assert!(!keyframe(&[0, 0, 0, 1]));
        assert!(!keyframe(&[]));
    }

    #[test]
    fn formats_the_guest_takes() {
        let f = |width, height| CameraFormat {
            width,
            height,
            fps_numerator: 30,
            fps_denominator: 1,
        };
        assert_eq!(format(&f(1280, 720)), Some((1280, 720, 30, 1)));
        assert_eq!(format(&f(1281, 720)), None);
        assert_eq!(format(&f(4096, 2160)), None);
    }
}
