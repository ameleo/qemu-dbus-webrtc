// SPDX-License-Identifier: MPL-2.0

//! One viewer: a WebRTC peer connection by str0m, without a media framework.
//!
//! str0m does no I/O of its own: a task of the session owns its state, the UDP sockets of its
//! host candidates, and turns what comes (datagrams, the encoders' frames, the client's
//! signalling, messages for the data channels, timeouts) into its inputs, one at a time, each
//! followed by all the outputs it gives (datagrams to send, events), as str0m requires.
//!
//! The server makes the offer, with its candidates in it; the client's answer and candidates
//! come back through `apply_answer` and `add_ice_candidate`. str0m packetizes the frames (RFC
//! 7798 for HEVC, the AV1 RTP specification, the VP9 payload format), paces them to its
//! bandwidth estimate (Google congestion control from the client's transport-wide feedback,
//! with probes), retransmits what the client misses (NACK, RTX), and carries the data channels
//! (SCTP over DTLS).

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use str0m::bwe::{Bitrate, BweKind};
use str0m::change::{SdpAnswer, SdpPendingOffer};
use str0m::channel::ChannelId;
use str0m::format::{Codec as RtpCodec, FormatParams};
use str0m::media::{
    Direction, Frequency, KeyframeRequestKind, MediaData, MediaKind, MediaTime, Mid,
};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use qdw_protocol::MAX_CAMERA_STREAMS;
use qdw_video::encode::{Codec, HevcFmtp};

use crate::audio::Mic;
use crate::transport::{
    Bitrates, Channel, MAX_SCREENS, Signal, SoundViewers, Start, VideoViewers, Viewer, leave,
};
use qdw_audio::OpusFrame;
use qdw_video::encode::EncodedFrame;

/// The payload types: the screens' video (all its tracks in the same codec), its
/// retransmissions, the clients' cameras in H.264 and theirs, the sound. str0m tells the tracks
/// of a bundle apart by their mid, not by their payload type.
const VIDEO_PT: u8 = 96;
const VIDEO_RTX_PT: u8 = 97;
const CAMERA_PT: u8 = 104;
const CAMERA_RTX_PT: u8 = 105;
const AUDIO_PT: u8 = 111;

/// A connection whose ICE checks fail this long is over: a short loss of the network lets it
/// come back.
const DISCONNECTED_GRACE: Duration = Duration::from_secs(10);

/// What the data channels may hold for sending, all together, until the client acknowledges
/// it: the largest message must fit (a drive's 1 MiB reads, with their header).
const MAX_BUFFERED: usize = 2 * 1024 * 1024;

/// The datagrams of a session wait at most this many for its task.
const INCOMING: usize = 1024;

/// What the session's task is asked to do.
enum Command {
    /// A screen's encoded frame, as it came out of its encoder at `at`.
    Video {
        screen: usize,
        at: Instant,
        data: Arc<[u8]>,
    },
    /// An Opus frame of the guest's sound, stamped by the encoder.
    Audio {
        at: Instant,
        pts: Duration,
        data: Arc<[u8]>,
    },
    Send(Channel, Vec<u8>),
    Answer(String),
    Candidate(String),
    CameraKeyframe(String),
}

/// A datagram for the session: where it came from, to which of its sockets.
struct Datagram {
    local: SocketAddr,
    source: SocketAddr,
    data: Vec<u8>,
}

pub struct Peer {
    commands: mpsc::UnboundedSender<Command>,
    task: tokio::task::JoinHandle<()>,
    /// The viewers of each screen's encoder and of the sound, where the session is the viewer
    /// `id`.
    viewers: Vec<VideoViewers>,
    audio_viewers: SoundViewers,
    id: u64,
    /// The client paused the video: it is out of the viewers.
    paused: Arc<AtomicBool>,
    camera_mids: Vec<String>,
}

impl Peer {
    /// Start a session for the streams of the guest's screens, one video track each in the
    /// order of `viewers`, and the guest's sound; its offer goes to `signals` at once.
    /// `keyframe` is called with a screen's index when the client needs a keyframe of it (a
    /// lost frame, or the start).
    pub fn start(
        start: Start,
        signals: mpsc::UnboundedSender<Signal>,
        keyframe: impl Fn(usize) + Send + Sync + 'static,
    ) -> Result<Self> {
        let Start {
            codec,
            full_chroma,
            ten_bits,
            viewers,
            audio_viewers,
            mic,
            bitrates,
        } = start;
        let screens = viewers.len().min(MAX_SCREENS);
        let ten_bits = ten_bits && codec.has_ten_bits();

        let mut config = Rtc::builder()
            .set_crypto_provider(Arc::new(str0m_rust_crypto::default_provider()))
            .clear_codecs()
            .enable_bwe(Some(Bitrate::kbps(u64::from(bitrates.start))))
            .set_sctp_max_buffered_amount(MAX_BUFFERED)
            .set_stats_interval(Some(Duration::from_secs(5)));
        let codecs = config.codec_config();
        // The cameras' H.264 first: a client sends its cameras in the first codec of the
        // offer it takes, and the guest takes H.264 only. The screens' codec comes in each
        // video track too; what goes on them is the server's choice. Any level: the client
        // sends what its camera gives.
        codecs.add_h264(CAMERA_PT.into(), Some(CAMERA_RTX_PT.into()), true, 0x42e01f);
        match codec {
            Codec::Hevc => {
                let fmtp = Codec::hevc_fmtp(full_chroma, ten_bits);
                codecs.add_h265(
                    VIDEO_PT.into(),
                    Some(VIDEO_RTX_PT.into()),
                    fmtp.profile_id,
                    0,
                    fmtp.level_id,
                );
            }
            Codec::Av1 => codecs.add_config(
                VIDEO_PT.into(),
                Some(VIDEO_RTX_PT.into()),
                RtpCodec::Av1,
                Frequency::NINETY_KHZ,
                None,
                FormatParams {
                    // 4:4:4 is the High profile (AV1 RTP specification 7.2.1).
                    profile: Some(u8::from(full_chroma)),
                    // Level 6.3, the highest the encoder reaches, with which every lower
                    // level complies: the stream's own follows the screen's size.
                    level_idx: Some(19),
                    tier: Some(0),
                    ..FormatParams::default()
                },
            ),
            Codec::Vp9 => codecs.add_config(
                VIDEO_PT.into(),
                Some(VIDEO_RTX_PT.into()),
                RtpCodec::Vp9,
                Frequency::NINETY_KHZ,
                None,
                FormatParams {
                    // 4:4:4 in 8 bits is profile 1.
                    profile_id: Some(u32::from(full_chroma)),
                    ..FormatParams::default()
                },
            ),
        }
        codecs.add_config(
            AUDIO_PT.into(),
            None,
            RtpCodec::Opus,
            Frequency::FORTY_EIGHT_KHZ,
            Some(2),
            FormatParams {
                // 10 ms frames, in stereo both ways (RFC 7587 7.1).
                min_p_time: Some(10),
                stereo: Some(true),
                sprop_stereo: Some(true),
                use_inband_fec: Some(true),
                ..FormatParams::default()
            },
        );
        let mut rtc = config.build(Instant::now());
        // The capacity the estimation probes for: up to the most the video may take. No
        // minimum: a still desktop sends little, and padding it up would only load the link.
        rtc.bwe()
            .set_bitrate(Bitrate::ZERO, Bitrate::kbps(u64::from(bitrates.max)));

        // A host candidate on each address of the host, in a socket of its own: the answer's
        // checks say which one reaches the client.
        let mut sockets = HashMap::new();
        for address in host_addresses()? {
            let socket = match std::net::UdpSocket::bind((address, 0)) {
                Ok(socket) => socket,
                Err(e) => {
                    debug!(%address, error = %e, "no socket");
                    continue;
                }
            };
            socket.set_nonblocking(true)?;
            let local = socket.local_addr()?;
            let candidate =
                Candidate::host(local, "udp").map_err(|e| anyhow!("candidate {local}: {e}"))?;
            rtc.add_local_candidate(candidate);
            sockets.insert(local, Arc::new(UdpSocket::from_std(socket)?));
        }
        if sockets.is_empty() {
            bail!("no address to receive the client on");
        }

        let mut change = rtc.sdp_api();
        let stream = Some("qdw".to_owned());
        let screen_mids: Vec<Mid> = (0..screens)
            .map(|i| {
                change.add_media(
                    MediaKind::Video,
                    Direction::SendOnly,
                    stream.clone(),
                    Some(format!("screen{i}")),
                    None,
                )
            })
            .collect();
        // The sound both ways: the client's microphone comes back on its track.
        let audio_mid = change.add_media(
            MediaKind::Audio,
            Direction::SendRecv,
            stream.clone(),
            Some("sound".to_owned()),
            None,
        );
        let camera_mids: Vec<Mid> = (0..MAX_CAMERA_STREAMS)
            .map(|_| change.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None))
            .collect();
        let channels: HashMap<ChannelId, Channel> = Channel::ALL
            .iter()
            .map(|&channel| (change.add_channel(channel.label().to_owned()), channel))
            .collect();
        let (offer, pending) = change.apply().context("no offer")?;
        let offer = offer.to_sdp_string();
        let offer = match codec {
            Codec::Hevc => with_hevc_fmtp(offer, Codec::hevc_fmtp(full_chroma, ten_bits)),
            _ => offer,
        };
        let _ = signals.send(Signal::Offer(offer));

        let (commands_tx, commands) = mpsc::unbounded_channel();
        let (incoming_tx, incoming) = mpsc::channel(INCOMING);
        let readers = sockets
            .iter()
            .map(|(&local, socket)| {
                AbortOnDrop(tokio::spawn(read(
                    local,
                    socket.clone(),
                    incoming_tx.clone(),
                )))
            })
            .collect();
        let paused = Arc::new(AtomicBool::new(false));
        let mic = mic.map(|mic| {
            let speaker = mic.viewer();
            (mic, speaker)
        });
        let state = State {
            rtc,
            sockets,
            _readers: readers,
            pending: Some(pending),
            screen_mids,
            audio_mid,
            camera_mids: camera_mids.clone(),
            channels,
            open: HashMap::new(),
            outgoing: HashMap::new(),
            signals: signals.clone(),
            keyframe: Box::new(keyframe),
            paused: paused.clone(),
            bitrates,
            estimate: None,
            start: Instant::now(),
            mic,
            decoder: None,
            cameras: HashMap::new(),
            disconnected: None,
            send_errors: 0,
        };
        let task = tokio::spawn(async move {
            let ended = run(state, commands, incoming).await;
            if let Err(e) = ended {
                warn!(error = %format!("{e:#}"), "peer connection");
            }
            let _ = signals.send(Signal::Closed);
        });

        let id = crate::transport::viewer_id();
        for (screen, viewers) in viewers.iter().enumerate().take(screens) {
            viewers
                .lock()
                .unwrap()
                .push(video_viewer(id, screen, commands_tx.clone()));
        }
        audio_viewers
            .lock()
            .unwrap()
            .push(audio_viewer(id, commands_tx.clone()));
        Ok(Self {
            commands: commands_tx,
            task,
            viewers: viewers.into_iter().take(screens).collect(),
            audio_viewers,
            id,
            paused,
            camera_mids: camera_mids.iter().map(|m| m.to_string()).collect(),
        })
    }

    /// Pause or resume the video. Paused, the client leaves the viewers its encoders feed: no
    /// frame goes to it, and a console nobody watches is not encoded. True when the video
    /// resumes: the client then needs a keyframe.
    pub fn pause_video(&self, paused: bool) -> bool {
        if self.paused.swap(paused, Ordering::Relaxed) == paused {
            return false;
        }
        for (screen, viewers) in self.viewers.iter().enumerate() {
            if paused {
                leave(viewers, self.id);
            } else {
                viewers
                    .lock()
                    .unwrap()
                    .push(video_viewer(self.id, screen, self.commands.clone()));
            }
        }
        debug!(paused, "client video");
        !paused
    }

    /// The mids of the camera tracks.
    pub fn camera_mids(&self) -> Vec<String> {
        self.camera_mids.clone()
    }

    /// Ask the client for a keyframe on the camera track of `mid` (RTCP PLI).
    pub fn camera_keyframe(&self, mid: &str) {
        let _ = self.commands.send(Command::CameraKeyframe(mid.to_owned()));
    }

    /// Send a message on a data channel: it waits for the channel to open, and for room.
    pub fn send(&self, channel: Channel, message: Vec<u8>) {
        let _ = self.commands.send(Command::Send(channel, message));
    }

    /// What the client reports of its reception is logged as it comes, at debug level.
    pub fn report_reception(&self) {}

    pub fn apply_answer(&self, sdp: &str) -> Result<()> {
        let _ = self.commands.send(Command::Answer(sdp.to_owned()));
        Ok(())
    }

    /// A candidate of the client: str0m takes it for the whole bundle, whatever its m-line.
    pub fn add_ice_candidate(&self, _mline_index: u32, candidate: &str) {
        let _ = self.commands.send(Command::Candidate(candidate.to_owned()));
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        for viewers in &self.viewers {
            leave(viewers, self.id);
        }
        leave(&self.audio_viewers, self.id);
        self.task.abort();
    }
}

/// A viewer of a screen's video, for the session's task.
fn video_viewer(
    id: u64,
    screen: usize,
    commands: mpsc::UnboundedSender<Command>,
) -> Viewer<EncodedFrame> {
    Viewer::new(id, move |frame: &EncodedFrame| {
        let _ = commands.send(Command::Video {
            screen,
            at: Instant::now(),
            data: frame.data.clone(),
        });
    })
}

/// A viewer of the guest's sound, for the session's task.
fn audio_viewer(id: u64, commands: mpsc::UnboundedSender<Command>) -> Viewer<OpusFrame> {
    Viewer::new(id, move |frame: &OpusFrame| {
        let _ = commands.send(Command::Audio {
            at: Instant::now(),
            pts: frame.pts,
            data: frame.data.clone(),
        });
    })
}

/// The addresses of the host a client may reach: neither loopback nor IPv6 link-local (which
/// would need its interface in the candidate).
fn host_addresses() -> Result<Vec<IpAddr>> {
    Ok(if_addrs::get_if_addrs()
        .context("listing the host's addresses")?
        .into_iter()
        .map(|interface| interface.ip())
        .filter(|ip| match ip {
            IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_unspecified(),
            IpAddr::V6(v6) => {
                !v6.is_loopback() && !v6.is_unspecified() && !v6.is_unicast_link_local()
            }
        })
        .collect())
}

/// The video's fmtp line of the offer, for HEVC: with the parameters str0m does not write
/// (profile-compatibility-indicator, interop-constraints: see `Codec::hevc_fmtp`).
fn with_hevc_fmtp(sdp: String, fmtp: HevcFmtp) -> String {
    let prefix = format!("a=fmtp:{VIDEO_PT} ");
    sdp.split_inclusive("\r\n")
        .map(|line| {
            if line.starts_with(&prefix) {
                format!("{prefix}{fmtp}\r\n")
            } else {
                line.to_owned()
            }
        })
        .collect()
}

/// Aborts a task when dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Read the datagrams of a socket for the session.
async fn read(local: SocketAddr, socket: Arc<UdpSocket>, incoming: mpsc::Sender<Datagram>) {
    let mut buffer = vec![0; 2048];
    loop {
        match socket.recv_from(&mut buffer).await {
            Ok((length, source)) => {
                let datagram = Datagram {
                    local,
                    source,
                    data: buffer[..length].to_vec(),
                };
                if incoming.send(datagram).await.is_err() {
                    return;
                }
            }
            // An ICMP error of an earlier datagram, on Linux: the socket goes on.
            Err(e) => debug!(%local, error = %e, "receiving"),
        }
    }
}

/// What a client camera's track needs to relay its frames.
#[derive(Default)]
struct Camera {
    /// The last parameter sets, put before an IDR that lacks them: the guest's decoder wants
    /// them before each.
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    /// Frames were lost: the next ones are dropped until a keyframe.
    broken: bool,
}

/// The session's task's state.
struct State {
    rtc: Rtc,
    sockets: HashMap<SocketAddr, Arc<UdpSocket>>,
    _readers: Vec<AbortOnDrop>,
    /// The offer, until its answer.
    pending: Option<SdpPendingOffer>,
    screen_mids: Vec<Mid>,
    audio_mid: Mid,
    camera_mids: Vec<Mid>,
    channels: HashMap<ChannelId, Channel>,
    /// The open data channels.
    open: HashMap<Channel, ChannelId>,
    /// The messages for each channel, in order, until it takes them.
    outgoing: HashMap<Channel, VecDeque<Vec<u8>>>,
    signals: mpsc::UnboundedSender<Signal>,
    keyframe: Box<dyn Fn(usize) + Send + Sync>,
    paused: Arc<AtomicBool>,
    bitrates: Bitrates,
    /// The last estimate given, in kbit/s.
    estimate: Option<u32>,
    /// The origin of the video's RTP timestamps.
    start: Instant,
    /// The guest's microphone, and the session's id as a speaker.
    mic: Option<(Arc<Mic>, u64)>,
    decoder: Option<qdw_audio::Decoder>,
    cameras: HashMap<Mid, Camera>,
    /// Since when the ICE checks fail.
    disconnected: Option<Instant>,
    send_errors: u64,
}

async fn run(
    mut state: State,
    mut commands: mpsc::UnboundedReceiver<Command>,
    mut incoming: mpsc::Receiver<Datagram>,
) -> Result<()> {
    loop {
        // Every input is followed by all its outputs, before the next (see str0m's run loop).
        let mut deadline = state.drain()?;
        while state.write_outgoing()? {
            deadline = state.drain()?;
        }
        if !state.rtc.is_alive() {
            info!("the peer connection closed");
            return Ok(());
        }
        if let Some(since) = state.disconnected {
            if since.elapsed() >= DISCONNECTED_GRACE {
                info!("the client is unreachable: the peer connection ends");
                return Ok(());
            }
            deadline = deadline.min(since + DISCONNECTED_GRACE);
        }
        tokio::select! {
            biased;
            datagram = incoming.recv() => {
                let Some(datagram) = datagram else { return Ok(()) };
                state.receive(datagram)?;
            }
            command = commands.recv() => match command {
                Some(command) => state.command(command)?,
                None => return Ok(()),
            },
            () = tokio::time::sleep_until(deadline.into()) => {
                state.rtc.handle_input(Input::Timeout(Instant::now()))?;
            }
        }
    }
}

impl State {
    /// Handle str0m's outputs until it waits: the time of its next timeout.
    fn drain(&mut self) -> Result<Instant> {
        loop {
            match self.rtc.poll_output()? {
                Output::Timeout(at) => return Ok(at),
                Output::Transmit(transmit) => {
                    let sent = self
                        .sockets
                        .get(&transmit.source)
                        .map(|socket| socket.try_send_to(&transmit.contents, transmit.destination));
                    // A datagram the socket cannot take now is lost, as on the network.
                    if let Some(Err(e)) = sent {
                        self.send_errors += 1;
                        if self.send_errors.is_power_of_two() {
                            debug!(error = %e, count = self.send_errors, "datagram not sent");
                        }
                    }
                }
                Output::Event(event) => self.event(event)?,
            }
        }
    }

    fn receive(&mut self, datagram: Datagram) -> Result<()> {
        let Ok(contents) = datagram.data.as_slice().try_into() else {
            // Neither STUN, DTLS nor RTP: not for this session.
            return Ok(());
        };
        let input = Input::Receive(
            Instant::now(),
            Receive {
                proto: Protocol::Udp,
                source: datagram.source,
                destination: datagram.local,
                contents,
            },
        );
        if self.rtc.accepts(&input) {
            self.rtc.handle_input(input)?;
        }
        Ok(())
    }

    fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Video { screen, at, data } => {
                let Some(&mid) = self.screen_mids.get(screen) else {
                    return Ok(());
                };
                let elapsed = at.saturating_duration_since(self.start);
                let time = MediaTime::from_90khz((elapsed.as_micros() * 9 / 100) as u64);
                if let Some(writer) = self.rtc.writer(mid)
                    && let Err(e) = writer.write(VIDEO_PT.into(), at, time, data)
                {
                    debug!(screen, error = %e, "frame not sent");
                }
            }
            Command::Audio { at, pts, data } => {
                let time = MediaTime::new(
                    (pts.as_nanos() * 48_000 / 1_000_000_000) as u64,
                    Frequency::FORTY_EIGHT_KHZ,
                );
                if let Some(writer) = self.rtc.writer(self.audio_mid)
                    && let Err(e) = writer.write(AUDIO_PT.into(), at, time, data)
                {
                    debug!(error = %e, "sound not sent");
                }
            }
            Command::Send(channel, message) => {
                if message.len() > MAX_BUFFERED {
                    warn!(
                        ?channel,
                        bytes = message.len(),
                        "data channel message too large"
                    );
                } else {
                    self.outgoing.entry(channel).or_default().push_back(message);
                }
            }
            Command::Answer(sdp) => {
                let answer = SdpAnswer::from_sdp_string(&sdp).context("parsing the answer")?;
                let pending = self.pending.take().context("an answer without an offer")?;
                self.rtc
                    .sdp_api()
                    .accept_answer(pending, answer)
                    .context("the client's answer is refused")?;
            }
            Command::Candidate(candidate) => {
                match Candidate::from_sdp_string(candidate.trim_start_matches("a=")) {
                    Ok(candidate) => self.rtc.add_remote_candidate(candidate),
                    // An mDNS name (.local) of a browser: the client's checks find it as a
                    // peer reflexive candidate.
                    Err(e) => debug!(candidate, error = %e, "remote candidate not taken"),
                }
            }
            Command::CameraKeyframe(mid) => self.camera_keyframe(&mid),
        }
        Ok(())
    }

    /// Give a data channel one message waiting for it, if one has room: true if one went.
    fn write_outgoing(&mut self) -> Result<bool> {
        for (channel, queue) in &mut self.outgoing {
            let (Some(&id), Some(message)) = (self.open.get(channel), queue.front()) else {
                continue;
            };
            let Some(mut handle) = self.rtc.channel(id) else {
                continue;
            };
            if handle.write(true, message)? {
                queue.pop_front();
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn camera_keyframe(&mut self, mid: &str) {
        let Some(&mid) = self.camera_mids.iter().find(|m| m.to_string() == mid) else {
            return;
        };
        if let Some(mut writer) = self.rtc.writer(mid)
            && let Err(e) = writer.request_keyframe(None, KeyframeRequestKind::Pli)
        {
            debug!(%mid, error = %e, "no keyframe asked of the camera");
        }
    }

    fn event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Connected => {
                info!("peer connection connected");
                // The client needs a keyframe to start, even on a still screen, and only now
                // can it receive one.
                if !self.paused.load(Ordering::Relaxed) {
                    for screen in 0..self.screen_mids.len() {
                        (self.keyframe)(screen);
                    }
                }
            }
            Event::IceConnectionStateChange(state) => {
                info!(?state, "ICE connection");
                self.disconnected = match state {
                    IceConnectionState::Disconnected => {
                        Some(self.disconnected.unwrap_or_else(Instant::now))
                    }
                    _ => None,
                };
            }
            Event::KeyframeRequest(request) => {
                // A paused client gets no video: it may ask for keyframes all the same.
                if let Some(screen) = self.screen_mids.iter().position(|&m| m == request.mid)
                    && !self.paused.load(Ordering::Relaxed)
                {
                    debug!(screen, kind = ?request.kind, "keyframe requested by the client");
                    (self.keyframe)(screen);
                }
            }
            Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => {
                let kbps = (estimate.as_u64() / 1000) as u32;
                let kbps = kbps.clamp(self.bitrates.min, self.bitrates.max);
                if self.estimate != Some(kbps) {
                    self.estimate = Some(kbps);
                    let _ = self.signals.send(Signal::Estimate(kbps));
                }
            }
            Event::ChannelOpen(id, label) => {
                let Some(&channel) = self.channels.get(&id) else {
                    debug!(label, "unknown data channel");
                    return Ok(());
                };
                debug!(?channel, "data channel open");
                self.open.insert(channel, id);
                let _ = self.signals.send(Signal::Open(channel));
            }
            Event::ChannelData(data) => {
                if let Some(&channel) = self.channels.get(&data.id) {
                    debug!(?channel, bytes = data.data.len(), "data channel message");
                    let _ = self.signals.send(Signal::Message(channel, data.data));
                }
            }
            Event::ChannelClose(id) => {
                if let Some(channel) = self.channels.get(&id) {
                    debug!(?channel, "data channel closed");
                    self.open.remove(channel);
                }
            }
            Event::MediaData(data) => self.media(data),
            Event::MediaEgressStats(stats) => debug!(
                mid = %stats.mid,
                packets = stats.packets,
                nacks = stats.nacks,
                plis = stats.plis,
                lost = stats.remote.as_ref().map(|r| r.packets_lost),
                fraction_lost = stats.loss,
                round_trip = ?stats.rtt,
                "client reception"
            ),
            Event::Closed => {
                info!("the client closed its peer connection");
                self.rtc.disconnect();
            }
            _ => {}
        }
        Ok(())
    }

    /// Media from the client: its microphone, or a camera.
    fn media(&mut self, data: MediaData) {
        if data.mid == self.audio_mid {
            let Some((mic, speaker)) = &self.mic else {
                return;
            };
            let decoder = match &mut self.decoder {
                Some(decoder) => decoder,
                None => match qdw_audio::Decoder::new() {
                    Ok(decoder) => self.decoder.insert(decoder),
                    Err(e) => {
                        warn!(error = %format!("{e:#}"), "no decoder for the client microphone");
                        self.mic = None;
                        return;
                    }
                },
            };
            match decoder.decode(&data.data) {
                // In `qdw_audio::mic::CANONICAL`: 48 kHz stereo, 16 bits, little endian.
                Ok(samples) => {
                    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
                    mic.push(*speaker, &bytes);
                }
                Err(e) => debug!(error = %format!("{e:#}"), "client sound not decoded"),
            }
            return;
        }
        if !self.camera_mids.contains(&data.mid) {
            return;
        }
        let mid = data.mid;
        let camera = self.cameras.entry(mid).or_default();
        let types: Vec<u8> = nal_units(&data.data).map(|n| n[0] & 0x1f).collect();
        for unit in nal_units(&data.data) {
            match unit[0] & 0x1f {
                7 => camera.sps = Some(unit.to_vec()),
                8 => camera.pps = Some(unit.to_vec()),
                _ => {}
            }
        }
        let idr = types.contains(&5);
        if !data.contiguous && !idr {
            camera.broken = true;
        }
        if camera.broken && !idr {
            if !data.contiguous {
                self.camera_keyframe(&mid.to_string());
            }
            return;
        }
        camera.broken = false;
        let mut frame = Vec::with_capacity(data.data.len() + 64);
        if idr && !types.contains(&7) {
            for set in [&camera.sps, &camera.pps].into_iter().flatten() {
                frame.extend_from_slice(&[0, 0, 0, 1]);
                frame.extend_from_slice(set);
            }
        }
        frame.extend_from_slice(&data.data);
        let _ = self.signals.send(Signal::CameraFrame {
            mid: mid.to_string(),
            timestamp_us: data.time.rebase(Frequency::MICROS).numer(),
            data: frame,
        });
    }
}

/// The NAL units of an H.264 access unit in Annex B, without their start codes.
fn nal_units(stream: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&s| {
            // The next start code, with the zero before it of a four-byte one.
            let end = s - 3;
            if end > 0 && stream[end - 1] == 0 {
                end - 1
            } else {
                end
            }
        })
        .chain(std::iter::once(stream.len()))
        .collect();
    starts
        .into_iter()
        .zip(ends)
        .filter(|(start, end)| start < end)
        .map(move |(start, end)| &stream[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nal_units_of_an_access_unit() {
        let stream = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5,
        ];
        let units: Vec<&[u8]> = nal_units(&stream).collect();
        assert_eq!(
            units,
            [&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5][..]]
        );
    }

    /// The HEVC parameters go on the video's fmtp line, and only there.
    #[test]
    fn hevc_fmtp() {
        let sdp = "v=0\r\na=fmtp:96 profile-id=4;tier-flag=0;level-id=186\r\na=fmtp:104 x=1\r\n";
        assert_eq!(
            with_hevc_fmtp(sdp.to_owned(), Codec::hevc_fmtp(true, false)),
            "v=0\r\na=fmtp:96 profile-id=4;tier-flag=0;level-id=186;\
             profile-compatibility-indicator=08000000;interop-constraints=BE0800000000\r\n\
             a=fmtp:104 x=1\r\n"
        );
    }

    /// The offer says what the stream is: its codec's profile, level and constraints.
    #[tokio::test]
    async fn offers_say_the_stream_format() {
        for (codec, full_chroma, ten_bits, expected) in [
            (
                Codec::Hevc,
                true,
                false,
                "a=fmtp:96 profile-id=4;tier-flag=0;level-id=186;\
                 profile-compatibility-indicator=08000000;interop-constraints=BE0800000000",
            ),
            (
                Codec::Hevc,
                false,
                true,
                "a=fmtp:96 profile-id=2;tier-flag=0;level-id=186;\
                 profile-compatibility-indicator=20000000;interop-constraints=B00000000000",
            ),
            (
                Codec::Av1,
                false,
                true,
                "a=fmtp:96 profile=0;level-idx=19;tier=0",
            ),
            (Codec::Vp9, true, false, "a=fmtp:96 profile-id=1"),
        ] {
            let (signals_tx, mut signals) = mpsc::unbounded_channel();
            let _peer = Peer::start(
                Start {
                    codec,
                    full_chroma,
                    ten_bits,
                    viewers: vec![VideoViewers::default()],
                    audio_viewers: SoundViewers::default(),
                    mic: None,
                    bitrates: Bitrates {
                        min: 1000,
                        start: 2000,
                        max: 4000,
                    },
                },
                signals_tx,
                |_| {},
            )
            .unwrap();
            let Some(Signal::Offer(offer)) = signals.recv().await else {
                panic!("no offer");
            };
            let fmtp = offer
                .lines()
                .find(|l| l.starts_with("a=fmtp:96 "))
                .unwrap_or_else(|| panic!("no fmtp in {offer}"));
            assert_eq!(fmtp, expected, "{codec:?}");
            // The cameras' H.264 comes first in each video track.
            assert!(
                offer.contains("m=video 9 UDP/TLS/RTP/SAVPF 104 105 96 97"),
                "{offer}"
            );
        }
    }

    /// A client that is another str0m peer, on this host: the offer it takes, the connection,
    /// the data channels both ways, the video and the sound it receives, the keyframe the
    /// session asks for at the start, the bandwidth estimate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_client_connects() {
        let viewers = VideoViewers::default();
        let audio_viewers = SoundViewers::default();
        let (signals_tx, mut signals) = mpsc::unbounded_channel();
        let keyframes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let asked = keyframes.clone();
        let peer = Peer::start(
            Start {
                codec: Codec::Vp9,
                full_chroma: false,
                ten_bits: false,
                viewers: vec![viewers.clone()],
                audio_viewers: audio_viewers.clone(),
                mic: None,
                bitrates: Bitrates {
                    min: 1000,
                    start: 2000,
                    max: 4000,
                },
            },
            signals_tx,
            move |_| {
                asked.fetch_add(1, Ordering::Relaxed);
            },
        )
        .unwrap();
        let Some(Signal::Offer(offer)) = signals.recv().await else {
            panic!("no offer");
        };
        assert!(offer.contains("a=fmtp:96 profile-id=0"), "{offer}");

        // The client, on the first address of the host.
        let address = host_addresses().unwrap()[0];
        let socket = UdpSocket::bind((address, 0)).await.unwrap();
        let local = socket.local_addr().unwrap();
        let mut client = Rtc::builder()
            .set_crypto_provider(Arc::new(str0m_rust_crypto::default_provider()))
            .build(Instant::now());
        let candidate = Candidate::host(local, "udp").unwrap();
        client.add_local_candidate(candidate.clone());
        let answer = client
            .sdp_api()
            .accept_offer(str0m::change::SdpOffer::from_sdp_string(&offer).unwrap())
            .unwrap();
        peer.apply_answer(&answer.to_sdp_string()).unwrap();
        peer.add_ice_candidate(0, &candidate.to_sdp_string());

        // Frames flow from the start: those before the connection are lost.
        let feeder = std::thread::spawn({
            let (viewers, audio_viewers) = (viewers.clone(), audio_viewers.clone());
            move || {
                for i in 0..300u64 {
                    // A VP9 keyframe's first byte (frame marker, profile 0), then filler.
                    let mut frame = vec![0x82; 3000];
                    frame[1] = i as u8;
                    let frame = EncodedFrame {
                        data: frame.into(),
                        keyframe: true,
                    };
                    crate::transport::fan_out(&viewers, &frame);
                    let sound = OpusFrame {
                        data: vec![0xfc, 0xff, 0xfe].into(),
                        pts: Duration::from_millis(10 * i),
                    };
                    crate::transport::fan_out(&audio_viewers, &sound);
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        });

        let mut buffer = vec![0; 2048];
        let mut video = 0;
        let mut sound = 0;
        let mut opened = Vec::new();
        let mut echoed = false;
        let mut sent_to_client = false;
        let mut estimated = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && !(video > 5 && sound > 5 && echoed && sent_to_client && estimated)
        {
            let timeout = loop {
                match client.poll_output().unwrap() {
                    Output::Timeout(t) => break t,
                    Output::Transmit(t) => {
                        let _ = socket.send_to(&t.contents, t.destination).await;
                    }
                    Output::Event(Event::MediaData(data)) => match data.params.spec().codec {
                        RtpCodec::Vp9 => video += 1,
                        RtpCodec::Opus => sound += 1,
                        _ => {}
                    },
                    Output::Event(Event::ChannelOpen(id, label)) => opened.push((id, label)),
                    Output::Event(Event::ChannelData(data)) => {
                        assert_eq!(data.data, b"to the client");
                        sent_to_client = true;
                    }
                    Output::Event(_) => {}
                }
            };
            // Once the channels are open: a message each way.
            if opened.len() == Channel::ALL.len() && !echoed {
                let (id, _) = opened
                    .iter()
                    .find(|(_, label)| label == Channel::Control.label())
                    .unwrap();
                assert!(
                    client
                        .channel(*id)
                        .unwrap()
                        .write(true, b"from the client")
                        .unwrap()
                );
                peer.send(Channel::Clipboard, b"to the client".to_vec());
                echoed = true;
                continue;
            }
            while let Ok(signal) = signals.try_recv() {
                match signal {
                    Signal::Message(channel, message) => {
                        assert_eq!(
                            (channel, message.as_slice()),
                            (Channel::Control, &b"from the client"[..])
                        );
                    }
                    Signal::Estimate(kbps) => {
                        assert!((1000..=4000).contains(&kbps), "{kbps}");
                        estimated = true;
                    }
                    Signal::Closed => panic!("closed"),
                    _ => {}
                }
            }
            let wait = timeout
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(1));
            let input = match tokio::time::timeout(wait, socket.recv_from(&mut buffer)).await {
                Ok(Ok((length, source))) => Input::Receive(
                    Instant::now(),
                    Receive {
                        proto: Protocol::Udp,
                        source,
                        destination: local,
                        contents: buffer[..length].try_into().unwrap(),
                    },
                ),
                _ => Input::Timeout(Instant::now()),
            };
            client.handle_input(input).unwrap();
        }
        assert_eq!(opened.len(), Channel::ALL.len(), "{opened:?}");
        assert!(
            video > 5 && sound > 5,
            "{video} frames, {sound} sound frames"
        );
        assert!(sent_to_client && estimated);
        assert!(keyframes.load(Ordering::Relaxed) >= 1);
        drop(peer);
        assert!(viewers.lock().unwrap().is_empty() && audio_viewers.lock().unwrap().is_empty());
        feeder.join().unwrap();
    }
}
