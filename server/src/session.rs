// SPDX-License-Identifier: MPL-2.0

//! One viewer: a WebRTC peer connection that carries a console's encoded stream.
//!
//! ```text
//! appsrc (encoded frames) ! queue ! RTP payloader ! webrtcbin
//! ```
//!
//! The server makes the offer; the signalling (SDP and ICE candidates) goes through the
//! caller, as `Signal`s out and `apply_answer`/`add_ice_candidate` in.

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
    CLIPBOARD_CHANNEL, CONTROL_CHANNEL, DRIVE_CHANNEL, INPUT_CHANNEL, POINTER_CHANNEL,
    PRINTER_CHANNEL,
};
use qdw_video::encode::Codec;

use crate::audio::Mic;

/// The payload type of the video. Fixed: there is one video stream for now.
const VIDEO_PT: u32 = 96;

/// The payload type of the sound: apart from the video's, which may grow to several screens.
const AUDIO_PT: u32 = 111;

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
}

impl Channel {
    const ALL: [Self; 6] = [
        Self::Input,
        Self::Pointer,
        Self::Clipboard,
        Self::Control,
        Self::Drive,
        Self::Printer,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Input => INPUT_CHANNEL,
            Self::Pointer => POINTER_CHANNEL,
            Self::Clipboard => CLIPBOARD_CHANNEL,
            Self::Control => CONTROL_CHANNEL,
            Self::Drive => DRIVE_CHANNEL,
            Self::Printer => PRINTER_CHANNEL,
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

pub struct Session {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    /// The data channels, in the order of [`Channel::ALL`].
    channels: Vec<gst_webrtc::WebRTCDataChannel>,
    src: gst_app::AppSrc,
    viewers: Viewers,
    /// The client paused the video: its appsrc is out of the viewers.
    paused: Arc<AtomicBool>,
    audio_src: gst_app::AppSrc,
    audio_viewers: AudioViewers,
}

impl Session {
    /// Start a session for a console's stream, encoded with `codec`, and the guest's sound.
    /// `keyframe` is called when the client needs a keyframe (a lost frame, or the start).
    pub fn start(
        codec: Codec,
        full_chroma: bool,
        viewers: Viewers,
        audio_viewers: AudioViewers,
        mic: Option<Arc<Mic>>,
        signals: mpsc::UnboundedSender<Signal>,
        keyframe: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        let (payloader, encoding) = codec.rtp();
        let description = format!(
            "appsrc name=src is-live=true format=time do-timestamp=true caps={caps} \
             ! queue \
             ! {payloader} pt={VIDEO_PT} mtu=1200 \
             ! application/x-rtp,media=video,encoding-name={encoding},payload={VIDEO_PT},clock-rate=90000{extra},\
rtcp-fb-nack-pli=(boolean)true \
             ! capsfilter name=rtp \
             appsrc name=audio is-live=true format=time caps={audio_caps} \
             ! queue leaky=downstream max-size-time=40000000 max-size-buffers=0 max-size-bytes=0 \
             ! rtpopuspay pt={AUDIO_PT} \
             ! application/x-rtp,media=audio,encoding-name=OPUS,payload={AUDIO_PT},clock-rate=48000 \
             ! capsfilter name=audiortp",
            caps = codec.stream_caps(),
            extra = codec.rtp_extra(full_chroma),
            audio_caps = qdw_audio::STREAM_CAPS,
        );
        let pipeline = gst::parse::launch(&description)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        let rtp = pipeline.by_name("rtp").context("no RTP capsfilter")?;
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
        let src = pipeline
            .by_name("src")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .context("no appsrc")?;

        // A keyframe request from the browser (PLI) travels upstream to our appsrc.
        let keyframe = Arc::new(keyframe);
        let on_pli = keyframe.clone();
        let paused = Arc::new(AtomicBool::new(false));
        let pli_paused = paused.clone();
        src.static_pad("src").context("no appsrc pad")?.add_probe(
            gst::PadProbeType::EVENT_UPSTREAM,
            move |_, info| {
                if let Some(event) = info.event()
                    && gst_video::UpstreamForceKeyUnitEvent::parse(event).is_ok()
                {
                    // A paused client gets no video: it may ask for keyframes all the same.
                    if !pli_paused.load(Ordering::Relaxed) {
                        debug!("keyframe requested by the browser");
                        on_pli();
                    }
                }
                gst::PadProbeReturn::Ok
            },
        );

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
                let direction = if index == 1 {
                    gst_webrtc::WebRTCRTPTransceiverDirection::Sendrecv
                } else {
                    gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly
                };
                transceiver.set_property("direction", direction);
                // The video: a client that lost packets asks for them again (NACK) and gets
                // them on a retransmission stream (RTX), rather than a whole new keyframe.
                if index == 0 {
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

        let closed_on_error = signals.clone();
        let closed_on_input = signals.clone();
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
                on_connected();
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
                    warn!(
                        source = ?error.src().map(|s| s.path_string()),
                        error = %error.error(),
                        debug = ?error.debug(),
                        "viewer pipeline error"
                    );
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
        rtp.link(&webrtc)?;
        audio_rtp.link(&webrtc)?;
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
        viewers.lock().unwrap().push(src.clone());
        audio_viewers.lock().unwrap().push(AudioViewer {
            src: audio_src.clone(),
            offset: None,
        });
        Ok(Self {
            pipeline,
            webrtc,
            channels,
            src,
            viewers,
            paused,
            audio_src,
            audio_viewers,
        })
    }

    /// Pause or resume the video. Paused, the client's appsrc leaves the viewers its encoder
    /// feeds: no frame goes to it, and a console nobody watches is not encoded. True when the
    /// video resumes: the client then needs a keyframe.
    pub fn pause_video(&self, paused: bool) -> bool {
        if self.paused.swap(paused, Ordering::Relaxed) == paused {
            return false;
        }
        let mut viewers = self.viewers.lock().unwrap();
        if paused {
            viewers.retain(|src| src != &self.src);
        } else {
            viewers.push(self.src.clone());
        }
        debug!(paused, "client video");
        !paused
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
        self.webrtc
            .emit_by_name::<()>("set-remote-description", &[&answer, &None::<gst::Promise>]);
        Ok(())
    }

    pub fn add_ice_candidate(&self, mline_index: u32, candidate: &str) {
        self.webrtc
            .emit_by_name::<()>("add-ice-candidate", &[&mline_index, &candidate]);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.viewers.lock().unwrap().retain(|src| src != &self.src);
        self.audio_viewers
            .lock()
            .unwrap()
            .retain(|viewer| viewer.src != self.audio_src);
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
