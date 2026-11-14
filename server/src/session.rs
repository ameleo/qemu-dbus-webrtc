// SPDX-License-Identifier: MPL-2.0

//! One viewer: a WebRTC peer connection that carries a console's encoded stream.
//!
//! ```text
//! appsrc (encoded frames) ! queue ! RTP payloader ! webrtcbin
//! ```
//!
//! The server makes the offer; the signalling (SDP and ICE candidates) goes through the
//! caller, as `Signal`s out and `apply_answer`/`add_ice_candidate` in.

use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_sdp as gst_sdp;
use gstreamer_video as gst_video;
use gstreamer_webrtc as gst_webrtc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use qdw_protocol::{
    CAMERA_CHANNEL, CLIPBOARD_CHANNEL, CONTROL_CHANNEL, DRIVE_CHANNEL, INPUT_CHANNEL,
    MAX_CAMERA_STREAMS, POINTER_CHANNEL, PRINTER_CHANNEL,
};
use qdw_video::encode::Codec;

use crate::audio::Mic;

/// The payload type of the first screen's video; the next screens take every other one after
/// it (98, 100, 102), leaving room for the retransmission streams webrtcbin adds: webrtcbin and
/// browsers tell the streams of a bundle apart by their payload type.
const VIDEO_PT: u32 = 96;

/// The guest screens a session carries, at most: one video track each.
pub const MAX_SCREENS: usize = 4;

/// The payload type of the sound: apart from the video's, which may grow to several screens.
const AUDIO_PT: u32 = 111;

/// The payload types of the client's cameras, one per track: webrtcbin tells the incoming
/// streams of a bundle apart by their payload type.
const CAMERA_PT: u32 = 104;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    const ALL: [Self; 7] = [
        Self::Input,
        Self::Pointer,
        Self::Clipboard,
        Self::Control,
        Self::Drive,
        Self::Printer,
        Self::Camera,
    ];

    fn label(self) -> &'static str {
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

/// The appsrcs of a console's current viewers, which its encoder feeds.
pub type Viewers = Arc<Mutex<Vec<gst_app::AppSrc>>>;

/// A viewer of the guest's sound, with the offset from the encoder's timestamps to its clock.
pub struct AudioViewer {
    src: gst_app::AppSrc,
    offset: Option<i64>,
}

/// The current viewers of the guest's sound.
pub type AudioViewers = Arc<Mutex<Vec<AudioViewer>>>;

/// Send an encoded sound frame to every viewer. Unlike the video, the sound keeps the encoder's
/// timestamps, moved once to each viewer's clock: they become the RTP timestamps, from which the
/// receiver places the samples, and frames stamped on arrival come out of the encoder two at a
/// time, nearly on the same stamp: the receiver took one of each pair for a duplicate and
/// concealed half of the sound.
pub fn fan_out_audio(viewers: &AudioViewers, frame: &gst::Buffer) {
    let Some(pts) = frame.pts() else { return };
    let pts = pts.nseconds() as i64;
    for viewer in viewers.lock().unwrap().iter_mut() {
        let Some(now) = viewer.src.current_running_time() else {
            continue;
        };
        let offset = *viewer.offset.get_or_insert(now.nseconds() as i64 - pts);
        let Ok(stamp) = u64::try_from(pts + offset) else {
            continue;
        };
        let mut moved = frame.copy();
        {
            let moved = moved.get_mut().unwrap();
            moved.set_pts(gst::ClockTime::from_nseconds(stamp));
            moved.set_dts(gst::ClockTime::NONE);
        }
        let _ = viewer.src.push_buffer(moved);
    }
}

/// Send an encoded frame to every viewer.
pub fn fan_out(viewers: &Viewers, frame: &gst::Buffer) {
    let viewers = viewers.lock().unwrap();
    if viewers.is_empty() {
        return;
    }
    // Each viewer's pipeline has its own clock, started when the viewer came: drop the
    // encoder's timestamps so that its appsrc stamps the frame on arrival. Kept, they would be
    // ahead of the viewer's clock by the time between the two starts, and the frame held back
    // for that long.
    let mut unstamped = frame.copy();
    {
        let unstamped = unstamped.get_mut().unwrap();
        unstamped.set_pts(gst::ClockTime::NONE);
        unstamped.set_dts(gst::ClockTime::NONE);
    }
    for viewer in viewers.iter() {
        // A viewer that is going away refuses it; it is removed with its session.
        let _ = viewer.push_buffer(unstamped.clone());
    }
}

/// The transport-wide congestion control header extension: the client reports when each packet
/// arrived, which the bandwidth estimation needs.
const TWCC_URI: &str = "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01";

/// The bounds of the bandwidth estimation, in kbit/s: the estimate starts at `start` and stays
/// within `min` and `max`.
#[derive(Debug, Clone, Copy)]
pub struct Bitrates {
    pub min: u32,
    pub start: u32,
    pub max: u32,
}

pub struct Session {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    /// The data channels, in the order of [`Channel::ALL`].
    channels: Vec<gst_webrtc::WebRTCDataChannel>,
    /// One appsrc per guest screen, and the viewers of that screen's encoder.
    srcs: Vec<gst_app::AppSrc>,
    viewers: Vec<Viewers>,
    /// How many screens: the first video tracks, before the sound's.
    screens: usize,
    /// The client paused the video: its appsrcs are out of the viewers.
    paused: Arc<AtomicBool>,
    audio_src: gst_app::AppSrc,
    audio_viewers: AudioViewers,
    /// Ends the session: an answer webrtcbin refuses.
    closed: mpsc::UnboundedSender<Signal>,
}

/// Stops a pipeline when dropped, unless disarmed.
struct StopOnDrop(Option<gst::Pipeline>);

impl StopOnDrop {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let Some(pipeline) = self.0.take() {
            let _ = pipeline.set_state(gst::State::Null);
        }
    }
}

impl Session {
    /// Start a session for the streams of the guest's screens, encoded with `codec` (in 10 bits
    /// with `ten_bits`, as the offer says), one video track each in the order of `viewers`, and
    /// the guest's sound. `keyframe` is called with a
    /// screen's index when the client needs a keyframe of it (a lost frame, or the start).
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        codec: Codec,
        full_chroma: bool,
        ten_bits: bool,
        viewers: Vec<Viewers>,
        audio_viewers: AudioViewers,
        mic: Option<Arc<Mic>>,
        bitrates: Bitrates,
        signals: mpsc::UnboundedSender<Signal>,
        keyframe: impl Fn(usize) + Send + Sync + 'static,
    ) -> Result<Self> {
        let (payloader, encoding) = codec.rtp();
        let screens = viewers.len().min(MAX_SCREENS);
        // The stream's fmtp parameters the payloader does not give, added to its caps, which
        // webrtcbin writes into the offer: a capsfilter would refuse caps without them.
        let fmtp = codec.rtp_fmtp(full_chroma, ten_bits && codec.has_ten_bits());
        let videos: String = (0..screens)
            .map(|i| {
                let pt = VIDEO_PT + 2 * i as u32;
                format!(
                    "appsrc name=src{i} is-live=true format=time do-timestamp=true caps={caps} \
                     ! queue \
                     ! {payloader} pt={pt} mtu=1200 {setter}\
                     ! application/x-rtp,media=video,encoding-name={encoding},payload={pt},clock-rate=90000{extra},\
rtcp-fb-nack-pli=(boolean)true,rtcp-fb-transport-cc=(boolean)true,extmap-1=(string){TWCC_URI} \
                     ! capsfilter name=rtp{i} ",
                    caps = codec.stream_caps(),
                    extra = codec.rtp_extra(full_chroma),
                    setter = fmtp.as_ref().map_or(String::new(), |fmtp| format!(
                        "! capssetter join=true replace=false caps=\"application/x-rtp,{fmtp}\" "
                    )),
                )
            })
            .collect();
        let description = format!(
            "{videos} \
             appsrc name=audio is-live=true format=time caps={audio_caps} \
             ! queue leaky=downstream max-size-time=40000000 max-size-buffers=0 max-size-bytes=0 \
             ! rtpopuspay pt={AUDIO_PT} \
             ! application/x-rtp,media=audio,encoding-name=OPUS,payload={AUDIO_PT},clock-rate=48000,\
rtcp-fb-transport-cc=(boolean)true,extmap-1=(string){TWCC_URI} \
             ! capsfilter name=audiortp",
            audio_caps = qdw_audio::STREAM_CAPS,
        );
        let pipeline = gst::parse::launch(&description)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        // Stopped if the session does not get started: GStreamer wants a pipeline stopped
        // before it goes, and the session's own drop stops it once it is one.
        let stop_on_failure = StopOnDrop(Some(pipeline.clone()));
        let rtps = (0..screens)
            .map(|i| {
                pipeline
                    .by_name(&format!("rtp{i}"))
                    .context("no RTP capsfilter")
            })
            .collect::<Result<Vec<_>>>()?;
        let audio_rtp = pipeline
            .by_name("audiortp")
            .context("no audio RTP capsfilter")?;
        let audio_src = pipeline
            .by_name("audio")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .context("no audio appsrc")?;
        // webrtcbin is linked only once its signals are connected: linking it is what asks for
        // the negotiation.
        let webrtc = gst::ElementFactory::make("webrtcbin")
            .property_from_str("bundle-policy", "max-bundle")
            .build()?;
        // Google Congestion Control: webrtcbin asks for the element that sends its packets,
        // which estimates the bandwidth from the client's reports and paces the packets to it.
        let estimates = signals.clone();
        webrtc.connect("request-aux-sender", false, move |_| {
            // A panic cannot leave a GLib callback: no unwrap in here.
            let bwe = gst::ElementFactory::make("rtpgccbwe")
                .property("min-bitrate", bitrates.min.saturating_mul(1000))
                .property("max-bitrate", bitrates.max.saturating_mul(1000))
                .property("estimated-bitrate", bitrates.start.saturating_mul(1000))
                .property_from_str("estimator", "linear-regression")
                .build();
            let bwe = match bwe {
                Ok(bwe) => bwe,
                Err(e) => {
                    warn!(error = %e, "no bandwidth estimation (rtpgccbwe)");
                    return None;
                }
            };
            let estimates = estimates.clone();
            bwe.connect_notify(Some("estimated-bitrate"), move |bwe, _| {
                let estimate = bwe.property::<u32>("estimated-bitrate") / 1000;
                let _ = estimates.send(Signal::Estimate(estimate));
            });
            Some(bwe.to_value())
        });
        let srcs = (0..screens)
            .map(|i| {
                pipeline
                    .by_name(&format!("src{i}"))
                    .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
                    .context("no appsrc")
            })
            .collect::<Result<Vec<_>>>()?;

        // A keyframe request from the browser (PLI) travels upstream to the appsrc of its
        // screen.
        let keyframe = Arc::new(keyframe);
        let paused = Arc::new(AtomicBool::new(false));
        for (screen, src) in srcs.iter().enumerate() {
            let on_pli = keyframe.clone();
            let pli_paused = paused.clone();
            src.static_pad("src").context("no appsrc pad")?.add_probe(
                gst::PadProbeType::EVENT_UPSTREAM,
                move |_, info| {
                    if let Some(event) = info.event()
                        && gst_video::UpstreamForceKeyUnitEvent::parse(event).is_ok()
                    {
                        // A paused client gets no video: it may ask for keyframes all the same.
                        if !pli_paused.load(Ordering::Relaxed) {
                            debug!(screen, "keyframe requested by the browser");
                            on_pli(screen);
                        }
                    }
                    gst::PadProbeReturn::Ok
                },
            );
        }

        let offers = signals.clone();
        // One offer per session: webrtcbin may ask more than once (the data channels, then the
        // video), and nothing needs renegotiating yet.
        let offered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        webrtc.connect("on-negotiation-needed", false, move |values| {
            // A panic cannot leave a GLib callback: no unwrap in here.
            let Ok(webrtc) = values[0].get::<gst::Element>() else {
                return None;
            };
            if offered.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return None;
            }
            // The client receives the video, and the sound both ways: its microphone comes back
            // on the sound's track (the second, after the video).
            for index in 0.. {
                let Some(transceiver) = webrtc
                    .emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>(
                        "get-transceiver",
                        &[&index],
                    )
                else {
                    break;
                };
                // The screens, then the sound, both ways; the cameras' tracks, after them, only
                // receive.
                let index = index as usize;
                let direction = match index {
                    i if i < screens => gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly,
                    i if i == screens => gst_webrtc::WebRTCRTPTransceiverDirection::Sendrecv,
                    _ => continue,
                };
                transceiver.set_property("direction", direction);
                // The video: a client that lost packets asks for them again (NACK) and gets
                // them on a retransmission stream (RTX), rather than a whole new keyframe.
                if index < screens {
                    transceiver.set_property("do-nack", true);
                }
            }
            let offers = offers.clone();
            let local = webrtc.clone();
            let promise = gst::Promise::with_change_func(move |reply| {
                let offer = match reply {
                    Ok(Some(reply)) => reply
                        .get::<gst_webrtc::WebRTCSessionDescription>("offer")
                        .ok(),
                    _ => None,
                };
                let Some(offer) = offer else {
                    warn!("no offer from webrtcbin");
                    let _ = offers.send(Signal::Closed);
                    return;
                };
                local.emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);
                match offer.sdp().as_text() {
                    Ok(sdp) => {
                        let _ = offers.send(Signal::Offer(sdp));
                    }
                    Err(e) => warn!(error = %e, "offer not serializable"),
                }
            });
            webrtc.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
            None
        });

        let candidates = signals.clone();
        webrtc.connect("on-ice-candidate", false, move |values| {
            let (Ok(mline_index), Ok(candidate)) =
                (values[1].get::<u32>(), values[2].get::<String>())
            else {
                return None;
            };
            let _ = candidates.send(Signal::IceCandidate {
                mline_index,
                candidate,
            });
            None
        });

        let closed_on_answer = signals.clone();
        let closed_on_error = signals.clone();
        let closed_on_input = signals.clone();
        let frames = signals.clone();
        let closed = signals;
        let on_connected = keyframe.clone();
        webrtc.connect_notify(Some("connection-state"), move |webrtc, _| {
            let state =
                webrtc.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
            info!(?state, "peer connection");
            // The client needs a keyframe to start, even on a still screen, and only now can it
            // receive one: a frame sent earlier is lost, and a client that never received
            // anything has no reason to ask for one.
            if state == gst_webrtc::WebRTCPeerConnectionState::Connected {
                for screen in 0..screens {
                    on_connected(screen);
                }
            }
            if matches!(
                state,
                gst_webrtc::WebRTCPeerConnectionState::Failed
                    | gst_webrtc::WebRTCPeerConnectionState::Closed
            ) {
                let _ = closed.send(Signal::Closed);
            }
        });

        // A pipeline error ends the session.
        let errors = closed_on_error;
        pipeline
            .bus()
            .context("no bus")?
            .set_sync_handler(move |_, message| {
                if let gst::MessageView::Error(error) = message.view() {
                    // The client closing its peer connection aborts the SCTP association of the
                    // data channels (libwebrtc does not shut it down gracefully): sctpenc takes
                    // that for an error. It is the client leaving, not a failure.
                    let left = error
                        .src()
                        .is_some_and(|s| s.type_().name() == "GstSctpEnc")
                        && error
                            .debug()
                            .is_some_and(|d| d.contains("association went into error state"));
                    if left {
                        info!("the client closed its peer connection");
                    } else {
                        warn!(
                            source = ?error.src().map(|s| s.path_string()),
                            error = %error.error(),
                            debug = ?error.debug(),
                            "viewer pipeline error"
                        );
                    }
                    let _ = errors.send(Signal::Closed);
                }
                gst::BusSyncReply::Drop
            });

        // Where the connection stands, to tell a network problem from the rest.
        webrtc.connect_notify(Some("ice-connection-state"), |webrtc, _| {
            let state =
                webrtc.property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state");
            info!(?state, "ICE connection");
        });
        webrtc.connect_notify(Some("ice-gathering-state"), |webrtc, _| {
            let state =
                webrtc.property::<gst_webrtc::WebRTCICEGatheringState>("ice-gathering-state");
            debug!(?state, "ICE gathering");
        });

        // The client's microphone: a stream coming out of webrtcbin, decoded for the guest; or
        // dropped, for a client without `mic` (a shadow client).
        let speaker = mic.as_ref().map(|mic| mic.viewer());
        let weak = pipeline.downgrade();
        webrtc.connect_pad_added(move |_, pad| {
            if pad.direction() != gst::PadDirection::Src {
                return;
            }
            let Some(pipeline) = weak.upgrade() else {
                return;
            };
            // A client camera: on a video track.
            let transceiver =
                pad.property::<Option<gst_webrtc::WebRTCRTPTransceiver>>("transceiver");
            if let Some(transceiver) = transceiver
                && transceiver.property::<gst_webrtc::WebRTCKind>("kind")
                    == gst_webrtc::WebRTCKind::Video
            {
                let mid = transceiver
                    .property::<Option<String>>("mid")
                    .unwrap_or_default();
                if let Err(e) = Self::relay_camera(&pipeline, pad, mid, frames.clone()) {
                    warn!(error = %format!("{e:#}"), "client camera not relayed");
                }
                return;
            }
            let linked = match (&mic, speaker) {
                (Some(mic), Some(speaker)) => {
                    Self::decode_mic(&pipeline, pad, mic.clone(), speaker)
                }
                _ => Self::drop_stream(&pipeline, pad),
            };
            if let Err(e) = linked {
                warn!(error = %format!("{e:#}"), "client microphone not handled");
            }
        });

        pipeline.add(&webrtc)?;
        // The video and the sound first, then the data channels, which webrtcbin makes from
        // READY on. It negotiates once caps arrive on both tracks, after PLAYING: one offer
        // covers them all.
        for rtp in &rtps {
            rtp.link(&webrtc)?;
        }
        audio_rtp.link(&webrtc)?;
        // Then the tracks the client's cameras come on, receive only, in H.264: the guest takes
        // it as it is. Any level: the client sends what its camera gives. With the
        // transport-wide congestion control reports, from which the client estimates the
        // bandwidth: without them, it sends nothing.
        for slot in 0..MAX_CAMERA_STREAMS as u32 {
            let caps = gst::Caps::from_str(&format!(
                "application/x-rtp,media=video,encoding-name=H264,clock-rate=90000,\
                 payload={pt},packetization-mode=(string)1,profile-level-id=(string)42e01f,\
                 level-asymmetry-allowed=(string)1,rtcp-fb-nack-pli=(boolean)true,\
                 rtcp-fb-transport-cc=(boolean)true,extmap-1=(string){TWCC_URI}",
                pt = CAMERA_PT + slot
            ))?;
            webrtc.emit_by_name::<gst_webrtc::WebRTCRTPTransceiver>(
                "add-transceiver",
                &[&gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly, &caps],
            );
        }
        pipeline.set_state(gst::State::Ready)?;
        let mut channels = Vec::new();
        for channel in Channel::ALL {
            let data_channel = Self::data_channel(&webrtc, channel.label())?;
            // Nothing else in these callbacks: they run on webrtcbin's thread, where calling
            // into it deadlocks.
            let messages = closed_on_input.clone();
            data_channel.connect_on_message_data(move |_, data| {
                if let Some(data) = data {
                    debug!(?channel, bytes = data.len(), "data channel message");
                    let _ = messages.send(Signal::Message(channel, data.to_vec()));
                }
            });
            data_channel.connect_on_close(move |_| debug!(?channel, "data channel closed"));
            data_channel
                .connect_on_error(move |_, error| warn!(?channel, %error, "data channel error"));
            let opened = closed_on_input.clone();
            data_channel.connect_on_open(move |_| {
                let _ = opened.send(Signal::Open(channel));
            });
            channels.push(data_channel);
        }
        pipeline.set_state(gst::State::Playing)?;
        for (screen, src) in srcs.iter().enumerate() {
            viewers[screen].lock().unwrap().push(src.clone());
        }
        audio_viewers.lock().unwrap().push(AudioViewer {
            src: audio_src.clone(),
            offset: None,
        });
        stop_on_failure.disarm();
        Ok(Self {
            pipeline,
            webrtc,
            channels,
            srcs,
            viewers,
            screens,
            paused,
            audio_src,
            audio_viewers,
            closed: closed_on_answer,
        })
    }

    /// Pause or resume the video. Paused, the client's appsrc leaves the viewers its encoder
    /// feeds: no frame goes to it, and a console nobody watches is not encoded. True when the
    /// video resumes: the client then needs a keyframe.
    pub fn pause_video(&self, paused: bool) -> bool {
        if self.paused.swap(paused, Ordering::Relaxed) == paused {
            return false;
        }
        for (viewers, src) in self.viewers.iter().zip(&self.srcs) {
            let mut viewers = viewers.lock().unwrap();
            if paused {
                viewers.retain(|s| s != src);
            } else {
                viewers.push(src.clone());
            }
        }
        debug!(paused, "client video");
        !paused
    }

    /// Relay the H.264 of a client camera coming out of webrtcbin's `pad`, the track of `mid`,
    /// as whole access units in Annex B with SPS and PPS before each IDR, which the guest's
    /// decoder takes as they are.
    fn relay_camera(
        pipeline: &gst::Pipeline,
        pad: &gst::Pad,
        mid: String,
        frames: mpsc::UnboundedSender<Signal>,
    ) -> Result<()> {
        let chain = gst::parse::bin_from_description(
            &format!(
                "rtph264depay ! h264parse config-interval=-1 \
                 ! video/x-h264,stream-format=byte-stream,alignment=au \
                 ! appsink name=camera-{mid} sync=false"
            ),
            true,
        )?;
        let sink = chain
            .by_name(&format!("camera-{mid}"))
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .context("no camera appsink")?;
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    let timestamp_us = buffer.pts().map_or(0, |t| t.useconds());
                    let _ = frames.send(Signal::CameraFrame {
                        mid: mid.clone(),
                        timestamp_us,
                        data: map.to_vec(),
                    });
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        pipeline.add(&chain)?;
        chain.sync_state_with_parent()?;
        pad.link(&chain.static_pad("sink").context("no camera chain sink")?)?;
        debug!(mid = sink.name().as_str(), "client camera track");
        Ok(())
    }

    /// The mids of the camera tracks, once negotiated.
    pub fn camera_mids(&self) -> Vec<String> {
        // After the screens and the sound.
        (self.screens as u32 + 1..)
            .map_while(|index: u32| {
                self.webrtc
                    .emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>(
                        "get-transceiver",
                        &[&(index as i32)],
                    )
            })
            .filter_map(|t| t.property::<Option<String>>("mid"))
            .collect()
    }

    /// Ask the client for a keyframe on the camera track of `mid` (RTCP PLI).
    pub fn camera_keyframe(&self, mid: &str) {
        if let Some(sink) = self.pipeline.by_name(&format!("camera-{mid}")) {
            sink.send_event(
                gst_video::UpstreamForceKeyUnitEvent::builder()
                    .all_headers(true)
                    .build(),
            );
        }
    }

    /// Drop the stream coming out of webrtcbin's `pad`: an unlinked pad would stop the pipeline.
    fn drop_stream(pipeline: &gst::Pipeline, pad: &gst::Pad) -> Result<()> {
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("async", false)
            .build()?;
        pipeline.add(&sink)?;
        sink.sync_state_with_parent()?;
        pad.link(
            &sink
                .static_pad("sink")
                .context("fakesink without a sink pad")?,
        )?;
        Ok(())
    }

    /// Decode the client's sound coming out of webrtcbin's `pad`, for the guest's microphone.
    fn decode_mic(
        pipeline: &gst::Pipeline,
        pad: &gst::Pad,
        mic: Arc<Mic>,
        speaker: u64,
    ) -> Result<()> {
        let decoder = gst::parse::bin_from_description(
            &format!(
                "queue ! rtpopusdepay ! opusdec ! audioconvert ! audioresample ! {} \
                 ! appsink name=mic sync=false",
                qdw_audio::mic::CANONICAL
            ),
            true,
        )?;
        let appsink = decoder
            .by_name("mic")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .context("no appsink")?;
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |appsink| {
                    let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                    mic.push(speaker, &map);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        pipeline.add(&decoder)?;
        decoder.sync_state_with_parent()?;
        pad.link(&decoder.static_pad("sink").context("no decoder sink")?)?;
        debug!(speaker, "client microphone");
        Ok(())
    }

    /// A reliable, ordered data channel.
    fn data_channel(webrtc: &gst::Element, label: &str) -> Result<gst_webrtc::WebRTCDataChannel> {
        webrtc
            .emit_by_name::<Option<gst_webrtc::WebRTCDataChannel>>(
                "create-data-channel",
                &[&label, &None::<gst::Structure>],
            )
            .with_context(|| format!("creating the {label} data channel"))
    }

    /// Send a message on a data channel.
    pub fn send(&self, channel: Channel, message: Vec<u8>) {
        let index = Channel::ALL
            .iter()
            .position(|c| *c == channel)
            .unwrap_or_default();
        if let Err(e) =
            self.channels[index].send_data_full(Some(&gst::glib::Bytes::from_owned(message)))
        {
            warn!(?channel, error = %e, "data channel message not sent");
        }
    }

    /// Log what the client reports of its reception (RTCP receiver reports): lost packets,
    /// jitter, round trip. At debug level.
    pub fn report_reception(&self) {
        let promise = gst::Promise::with_change_func(|reply| {
            let Ok(Some(stats)) = reply else { return };
            for (_, value) in stats.iter() {
                let Ok(stat) = value.get::<gst::Structure>() else {
                    continue;
                };
                if stat.name() != "remote-inbound-rtp" {
                    continue;
                }
                let kind = stat.get::<String>("kind").unwrap_or_default();
                debug!(
                    kind,
                    lost = stat.get::<i64>("packets-lost").unwrap_or_default(),
                    fraction_lost = stat.get::<f64>("fraction-lost").unwrap_or_default(),
                    jitter = stat.get::<f64>("jitter").unwrap_or_default(),
                    round_trip = stat.get::<f64>("round-trip-time").unwrap_or_default(),
                    "client reception"
                );
            }
        });
        self.webrtc
            .emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
    }

    pub fn apply_answer(&self, sdp: &str) -> Result<()> {
        let sdp =
            gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes()).context("parsing the answer")?;
        let answer =
            gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Answer, sdp);
        // An answer webrtcbin refuses ends the session, rather than leave it waiting.
        let closed = self.closed.clone();
        let promise = gst::Promise::with_change_func(move |reply| {
            let error = match reply {
                Ok(Some(reply)) => reply.get::<gst::glib::Error>("error").ok(),
                Ok(None) => None,
                Err(e) => Some(gst::glib::Error::new(
                    gst::CoreError::Failed,
                    &format!("{e:?}"),
                )),
            };
            if let Some(error) = error {
                warn!(%error, "the client's answer is refused");
                let _ = closed.send(Signal::Closed);
            }
        });
        self.webrtc
            .emit_by_name::<()>("set-remote-description", &[&answer, &promise]);
        Ok(())
    }

    pub fn add_ice_candidate(&self, mline_index: u32, candidate: &str) {
        self.webrtc
            .emit_by_name::<()>("add-ice-candidate", &[&mline_index, &candidate]);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for (viewers, src) in self.viewers.iter().zip(&self.srcs) {
            viewers.lock().unwrap().retain(|s| s != src);
        }
        self.audio_viewers
            .lock()
            .unwrap()
            .retain(|viewer| viewer.src != self.audio_src);
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offer of an HEVC session gives the profile of the stream (RFC 7798 7.1), which the
    /// payloader does not: a session fed by real encoders (the GPU's for the video), its offer
    /// read. Skipped where no GPU encodes HEVC.
    #[test]
    fn an_hevc_offer_gives_the_stream_profile() {
        gst::init().unwrap();
        if !qdw_video::gpus::gpus().iter().any(|gpu| {
            gpu.codecs
                .iter()
                .any(|s| s.codec == Codec::Hevc && s.reduced.eight)
        }) {
            eprintln!("no HEVC encoder: skipped");
            return;
        }
        let viewers = Viewers::default();
        let audio_viewers = AudioViewers::default();
        let (signals_tx, signals) = std::sync::mpsc::channel();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let session = Session::start(
            Codec::Hevc,
            false,
            false,
            vec![viewers.clone()],
            audio_viewers.clone(),
            None,
            Bitrates {
                min: 1000,
                start: 2000,
                max: 4000,
            },
            tx,
            |_| {},
        )
        .unwrap();
        // The sound flows first: the offer waits for it.
        let audio_out = audio_viewers.clone();
        let _audio = qdw_audio::AudioEncoder::new(move |frame| {
            fan_out_audio(&audio_out, frame);
        })
        .unwrap();
        let file = rustix::fs::memfd_create("fb", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
        rustix::fs::ftruncate(&file, 320 * 240 * 4).unwrap();
        let settings = qdw_video::encode::Settings {
            codec: Codec::Hevc,
            full_chroma: false,
            bitrate: 2000,
            ten_bits: false,
            software: false,
            max_bitrate: 30_000,
        };
        let video_out = viewers.clone();
        let mut encoder = qdw_video::encode::Encoder::new(&settings, move |frame| {
            fan_out(&video_out, frame);
        })
        .unwrap();
        encoder
            .set_shared_framebuffer(
                &file,
                320,
                240,
                qdw_video::encode::PixelFormat::Bgrx,
                0,
                320 * 4,
            )
            .unwrap();
        std::thread::spawn(move || {
            while let Some(signal) = rx.blocking_recv() {
                if let Signal::Offer(sdp) = signal {
                    let _ = signals_tx.send(sdp);
                }
            }
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let sdp = loop {
            assert!(std::time::Instant::now() < deadline, "no offer");
            encoder.keyframe().unwrap();
            if let Ok(sdp) = signals.recv_timeout(std::time::Duration::from_millis(100)) {
                break sdp;
            }
        };
        let fmtp = sdp
            .lines()
            .find(|l| l.starts_with(&format!("a=fmtp:{VIDEO_PT} ")))
            .unwrap_or_else(|| panic!("no fmtp for the video in {sdp}"));
        for parameter in [
            "profile-id=1",
            "tier-flag=0",
            "level-id=186",
            "profile-compatibility-indicator=60000000",
            "interop-constraints=900000000000",
        ] {
            assert!(
                fmtp.split([' ', ';']).any(|p| p == parameter),
                "{parameter} not in {fmtp}"
            );
        }
        drop(session);
        encoder.finish();
    }
}
