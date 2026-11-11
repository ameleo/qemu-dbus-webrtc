// SPDX-License-Identifier: MPL-2.0

//! One viewer: a WebRTC peer connection that carries a console's encoded stream.
//!
//! ```text
//! appsrc (encoded frames) ! queue ! RTP payloader ! webrtcbin
//! ```
//!
//! The server makes the offer; the signalling (SDP and ICE candidates) goes through the
//! caller, as `Signal`s out and `apply_answer`/`add_ice_candidate` in.

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

use qdw_protocol::{CLIPBOARD_CHANNEL, INPUT_CHANNEL, POINTER_CHANNEL};
use qdw_video::encode::Codec;

/// The payload type of the video. Fixed: there is one video stream for now.
const VIDEO_PT: u32 = 96;

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
}

impl Channel {
    const ALL: [Self; 3] = [Self::Input, Self::Pointer, Self::Clipboard];

    fn label(self) -> &'static str {
        match self {
            Self::Input => INPUT_CHANNEL,
            Self::Pointer => POINTER_CHANNEL,
            Self::Clipboard => CLIPBOARD_CHANNEL,
        }
    }
}

/// The appsrcs of a console's current viewers, which its encoder feeds.
pub type Viewers = Arc<Mutex<Vec<gst_app::AppSrc>>>;

pub struct Session {
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    /// The data channels, in the order of [`Channel::ALL`].
    channels: Vec<gst_webrtc::WebRTCDataChannel>,
    src: gst_app::AppSrc,
    viewers: Viewers,
}

impl Session {
    /// Start a session for a console's stream, encoded with `codec`. `keyframe` is called when
    /// the browser needs a keyframe (a lost frame, or the start).
    pub fn start(
        codec: Codec,
        viewers: Viewers,
        signals: mpsc::UnboundedSender<Signal>,
        keyframe: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self> {
        let (payloader, encoding) = codec.rtp();
        let description = format!(
            "appsrc name=src is-live=true format=time do-timestamp=true caps={caps} \
             ! queue \
             ! {payloader} pt={VIDEO_PT} mtu=1200 \
             ! application/x-rtp,media=video,encoding-name={encoding},payload={VIDEO_PT},clock-rate=90000,\
rtcp-fb-nack-pli=(boolean)true \
             ! capsfilter name=rtp",
            caps = codec.stream_caps(),
        );
        let pipeline = gst::parse::launch(&description)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        let rtp = pipeline.by_name("rtp").context("no RTP capsfilter")?;
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
        src.static_pad("src").context("no appsrc pad")?.add_probe(
            gst::PadProbeType::EVENT_UPSTREAM,
            move |_, info| {
                if let Some(event) = info.event()
                    && gst_video::UpstreamForceKeyUnitEvent::parse(event).is_ok()
                {
                    debug!("keyframe requested by the browser");
                    on_pli();
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
            // The video transceiver, there by construction (see below).
            let video = webrtc.emit_by_name::<Option<gst_webrtc::WebRTCRTPTransceiver>>(
                "get-transceiver",
                &[&0i32],
            )?;
            if offered.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return None;
            }
            // Send only: the client receives the video.
            video.set_property(
                "direction",
                gst_webrtc::WebRTCRTPTransceiverDirection::Sendonly,
            );
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

        pipeline.add(&webrtc)?;
        // The video first, then the data channels, which webrtcbin makes from READY on. It
        // negotiates once caps arrive on the video, after PLAYING: one offer covers all three.
        rtp.link(&webrtc)?;
        pipeline.set_state(gst::State::Ready)?;
        let mut channels = Vec::new();
        for channel in Channel::ALL {
            let data_channel = Self::data_channel(&webrtc, channel.label())?;
            // Nothing else in these callbacks: they run on webrtcbin's thread, where calling
            // into it deadlocks.
            let messages = closed_on_input.clone();
            data_channel.connect_on_message_data(move |_, data| {
                if let Some(data) = data {
                    let _ = messages.send(Signal::Message(channel, data.to_vec()));
                }
            });
            let opened = closed_on_input.clone();
            data_channel.connect_on_open(move |_| {
                let _ = opened.send(Signal::Open(channel));
            });
            channels.push(data_channel);
        }
        pipeline.set_state(gst::State::Playing)?;
        viewers.lock().unwrap().push(src.clone());
        Ok(Self {
            pipeline,
            webrtc,
            channels,
            src,
            viewers,
        })
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
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
