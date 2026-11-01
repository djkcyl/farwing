//! What a device sends into a voice channel (`docs/protocol/chat-design.md`
//! §7.3, §8): one connection to the forwarder carries the microphone, the
//! camera's two encodings, the screen share, its 720p30 copy and its sound.
//! Pictures go through the host's packetizer, pacing, repair and RS-FEC
//! (`media.rs`), encrypted per frame in end-to-end channels; each comes
//! from a capture thread of its own that encodes it once or twice.

use super::media::{self, Owner, Sending, Timeline};
use crate::features::host::format::{Codec, Negotiated};
use crate::features::host::parameters::Bounds;
use crate::features::host::track::VideoTrack;
use crate::features::host::transport::Transport;
use crate::media::capture::{Screen, Source};
use crate::media::encoding::{Format, FrameControl, QualityTarget, Rate};
use crate::media::geometry::Tier;
use crate::platform::windows::encoder::{Encoded, Encoder};
use anyhow::{Context, Result};
use farwing_protocol::chat::track;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

/// The connection's sending: it may use up to this (a 1080p60 share, its
/// 720p copy and a camera).
const BOUNDS: Bounds = Bounds {
    reservation: 300_000,
    maximum: 14_000_000,
    initial: 2_500_000,
    probe: 8_000_000,
};
/// The camera: 720p30 at most 1.2 Mbps and 320×180 at 15 fps, 80 kbps.
const CAM_HI: (u32, u32, u32) = (1280, 720, 1_200_000);
const CAM_LO: (u32, u32, u32) = (320, 180, 80_000);
const CAM_HI_FPS: u32 = 30;
const CAM_LO_FPS: u32 = 15;
/// A share's 720p30 copy, encoded while someone watches it.
const SCREEN_720: (u32, u32, u32) = (1280, 720, 3_000_000);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// A screen share's picture: 720p or 1080p at 30 or 60 frames per second
/// (`chat-design.md` §8.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Quality {
    P720F30,
    P720F60,
    P1080F30,
    P1080F60,
}

impl Quality {
    pub(crate) const ALL: [Quality; 4] = [Self::P720F30, Self::P720F60, Self::P1080F30, Self::P1080F60];

    /// Size limit, frames per second and rate limit.
    fn shape(self) -> ((u32, u32), u32, u32) {
        match self {
            Self::P720F30 => ((1280, 720), 30, 3_000_000),
            Self::P720F60 => ((1280, 720), 60, 4_000_000),
            Self::P1080F30 => ((1920, 1080), 30, 4_000_000),
            Self::P1080F60 => ((1920, 1080), 60, 6_000_000),
        }
    }

    /// As the voice state names it (`720p30`…).
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::P720F30 => "720p30",
            Self::P720F60 => "720p60",
            Self::P1080F30 => "1080p30",
            Self::P1080F60 => "1080p60",
        }
    }
}

/// The camera for a voice channel: `id`'s (the first one that opens if
/// unset or gone), 720p30 when it has it.
pub(crate) fn camera_spec(id: Option<&str>) -> Result<crate::media::camera::Spec> {
    use crate::platform::windows::camera;
    let cameras = camera::list()?;
    let chosen = id
        .and_then(|id| cameras.iter().find(|c| c.id == id && c.unavailable.is_none()))
        .or_else(|| cameras.iter().find(|c| c.unavailable.is_none()))
        .context(crate::i18n::t().voice.no_camera)?;
    let spec = camera::default_spec(chosen)?;
    let p720 = chosen
        .modes
        .iter()
        .filter(|m| m.width == 1280 && m.height == 720 && m.fps() >= 30)
        .max_by_key(|m| m.fps());
    Ok(match p720 {
        Some(mode) => crate::media::camera::Spec::new(chosen, mode.clone(), spec.color, spec.adapter),
        None => spec,
    })
}

/// The uplink sends while it is open.
#[derive(Clone)]
struct UplinkOwner(CancellationToken);

impl Owner for UplinkOwner {
    fn requested(&self) -> bool {
        !self.0.is_cancelled()
    }
    fn fail(&self, message: String) {
        tracing::warn!(%message, "voice picture stream failed");
    }
    fn frame(&self) {}
}

/// One picture track: where its frames go, and its share of the link.
pub(crate) struct VideoSlot {
    pub kind: &'static str,
    frames: mpsc::Sender<Encoded>,
    keyframe: Arc<AtomicBool>,
    transport: Transport,
    timeline: Arc<Timeline>,
}

impl VideoSlot {
    /// What it may send now (bit/s), within `cap`.
    fn rate(&self, cap: u32) -> u32 {
        self.transport.media_rate().clamp(cap.min(150_000), cap)
    }

    /// A receiver or the packetizer asked for a keyframe.
    fn take_keyframe(&self) -> bool {
        self.keyframe.swap(false, Ordering::AcqRel)
    }

    /// The track sends now, within `maximum` bit/s (`None`: it does not).
    fn activate(&self, maximum: Option<u32>) {
        match maximum {
            Some(maximum) => self.transport.configure(Bounds {
                reservation: (maximum / 4).min(1_000_000),
                maximum,
                initial: maximum / 2,
                probe: 0,
            }),
            None => self.transport.pause(),
        }
    }

    fn timestamp(&self, captured: Instant) -> i64 {
        (captured.saturating_duration_since(self.timeline.started).as_nanos() / 100) as i64
    }

    /// `false` once the uplink is gone.
    fn send(&self, frame: Encoded) -> bool {
        self.frames.blocking_send(frame).is_ok()
    }
}

/// A capture thread and how to stop it.
struct Worker {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Worker {
    fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            std::thread::spawn(move || {
                let _ = thread.join();
            });
        }
    }
}

/// What a share shows.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Shown {
    Screen(Screen),
    /// A window (its handle) and the program it belongs to.
    Window { hwnd: isize, pid: u32 },
}

impl Shown {
    /// Whose sound goes with it (§9.2): all but this program's for a
    /// screen, the window's program's for a window.
    fn sound(&self) -> crate::platform::windows::process_loopback::Target {
        use crate::platform::windows::process_loopback::Target;
        match self {
            Self::Screen(_) => Target::AllBut(std::process::id()),
            Self::Window { pid, .. } => Target::Only(*pid),
        }
    }
}

/// The share's state the capture thread reads and sets.
struct ShareControl {
    quality: Mutex<Quality>,
    /// Someone watches the 720p30 copy.
    copy: AtomicBool,
    /// The shared window is minimized (its last picture stays).
    minimized: AtomicBool,
    /// The capture stopped (the window closed, the screen went away).
    ended: AtomicBool,
}

/// A running share.
struct Share {
    worker: Worker,
    control: Arc<ShareControl>,
    shown: Shown,
    sound: Option<crate::platform::windows::process_loopback::Capture>,
}

pub(crate) struct ChatUplink {
    pub pc: Arc<RTCPeerConnection>,
    /// The microphone's and the share's sound tracks (the voice engine and
    /// the share's sound write them).
    pub mic: Arc<TrackLocalStaticRTP>,
    pub mic_sender: Arc<RTCRtpSender>,
    pub screen_audio: Arc<TrackLocalStaticRTP>,
    slots: Vec<Arc<VideoSlot>>,
    camera: Mutex<Option<Worker>>,
    screen: Mutex<Option<Share>>,
    /// Seals the share's sound in an end-to-end channel.
    cipher: Option<super::uplink::Cipher>,
    cancel: CancellationToken,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Told of each connection state (the connection has one handler,
    /// the transport's).
    on_state: Arc<Mutex<Option<StateHook>>>,
}

type StateHook = Box<dyn Fn(RTCPeerConnectionState) + Send + Sync>;

fn opus_track(id: &str) -> Arc<TrackLocalStaticRTP> {
    Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: "audio/opus".into(),
            clock_rate: 48_000,
            channels: 2,
            sdp_fmtp_line: farwing_rtc_wire::OPUS_FMTP.into(),
            ..Default::default()
        },
        id.to_owned(),
        "me".to_owned(),
    ))
}

impl ChatUplink {
    /// The connection with all its tracks (pictures start later); not yet
    /// offered.
    pub(crate) async fn new(ice_servers: Vec<RTCIceServer>, cipher: Option<super::uplink::Cipher>) -> Result<Self> {
        let cancel = CancellationToken::new();
        let owner = cancel.clone();
        let transport = Transport::for_owner(Arc::new(move || !owner.is_cancelled()), cancel.clone(), BOUNDS);
        let mut registry = Registry::new();
        registry.add(Box::new(transport.clone()));
        registry.add(Box::new(crate::transport::rtcp_timing::RtcpTiming::new()));
        let mut settings = crate::transport::rtc::ConnectionCore::settings();
        settings.enable_sender_rtx(true);
        settings.enable_sender_rsfec(true);
        let mut media = MediaEngine::default();
        farwing_rtc_wire::register_codecs(&mut media)?;
        farwing_rtc_wire::register_header_extensions(&mut media, &[farwing_rtc_wire::DEPENDENCY_DESCRIPTOR_URI])?;
        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(settings)
            .build();
        let pc = Arc::new(
            api.new_peer_connection(RTCConfiguration {
                ice_servers,
                ..Default::default()
            })
            .await
            .context("create voice uplink")?,
        );
        let sendonly = || {
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Sendonly,
                send_encodings: Vec::new(),
            })
        };
        let mic = opus_track(track::MIC);
        let mic_sender = pc
            .add_transceiver_from_track(mic.clone() as Arc<dyn TrackLocal + Send + Sync>, sendonly())
            .await?
            .sender()
            .await;
        let mut tasks = Vec::new();
        let mut slots = Vec::new();
        for (index, kind) in [track::CAM_HI, track::CAM_LO, track::SCREEN, track::SCREEN_720].into_iter().enumerate() {
            let video = Arc::new(VideoTrack::named(kind, Codec::H264, Arc::new(Negotiated::deferred())));
            let slot_transport = if index == 0 { transport.clone() } else { transport.fork(index) };
            let sender = pc
                .add_transceiver_from_track(video.clone() as Arc<dyn TrackLocal + Send + Sync>, sendonly())
                .await?
                .sender()
                .await;
            for encoding in &sender.get_parameters().await.encodings {
                slot_transport.register_ssrc(encoding.ssrc);
            }
            let timeline = Timeline::new();
            let keyframe = Arc::new(AtomicBool::new(true));
            let (frames_tx, frames) = mpsc::channel(1);
            let owner = UplinkOwner(cancel.clone());
            tasks.push(media::spawn_reports(
                index,
                &timeline,
                sender.clone(),
                Arc::downgrade(&pc),
                cancel.clone(),
                owner.clone(),
                Arc::new(AtomicBool::new(true)),
            ));
            let track_feedback = Arc::new(crate::features::host::recovery::TrackFeedback::default());
            tasks.push(media::spawn_feedback(
                index,
                sender.clone(),
                slot_transport.clone(),
                cancel.clone(),
                Arc::default(),
                track_feedback.clone(),
                keyframe.clone(),
            ));
            tasks.push(media::spawn_sending(
                Sending {
                    index,
                    capture_index: 0,
                    timeline: timeline.clone(),
                    track: video,
                    extension_sender: sender.clone(),
                    sending_formats: Arc::new(Negotiated::deferred()),
                    sending_keyframe: keyframe.clone(),
                    transport: slot_transport.clone(),
                    send_handle: owner,
                    sent_feedback: track_feedback,
                    send_cancel: cancel.clone(),
                    sent_first_frame: Arc::new(AtomicBool::new(false)),
                    worker_started: Instant::now(),
                    encrypt: cipher.clone(),
                },
                frames,
            ));
            let repairs = slot_transport.clone();
            tasks.push(tokio::spawn(async move { repairs.repairs().await }));
            let fec = slot_transport.clone();
            tasks.push(tokio::spawn(async move { fec.fec_worker().await }));
            slots.push(Arc::new(VideoSlot {
                kind,
                frames: frames_tx,
                keyframe,
                transport: slot_transport,
                timeline,
            }));
        }
        // Nothing is shown yet.
        for slot in &slots {
            slot.activate(None);
        }
        let screen_audio = opus_track(track::SCREEN_AUDIO);
        pc.add_transceiver_from_track(screen_audio.clone() as Arc<dyn TrackLocal + Send + Sync>, sendonly())
            .await?;
        let state_transport = transport.clone();
        let dtls = Arc::downgrade(&mic_sender.transport());
        let on_state: Arc<Mutex<Option<StateHook>>> = Arc::default();
        let hook = Arc::clone(&on_state);
        pc.on_peer_connection_state_change(Box::new(move |state| {
            state_transport.network(state == RTCPeerConnectionState::Connected);
            if let Some(hook) = lock(&hook).as_ref() {
                hook(state);
            }
            let transport = state_transport.clone();
            let dtls = dtls.clone();
            Box::pin(async move {
                if state == RTCPeerConnectionState::Connected
                    && let Some(dtls) = dtls.upgrade()
                {
                    transport.srtp_overhead(dtls.rtp_authentication_overhead().await);
                }
            })
        }));
        let route_transport = transport.clone();
        mic_sender
            .transport()
            .ice_transport()
            .on_selected_candidate_pair_change(Box::new(move |pair| {
                route_transport.route_changed();
                let transport = route_transport.clone();
                let tcp = pair.local.protocol == webrtc::ice_transport::ice_protocol::RTCIceProtocol::Tcp;
                let ipv6 = pair.local.address.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_ipv6());
                Box::pin(async move {
                    transport.route(if ipv6 { 40 } else { 20 } + if tcp { 20 } else { 8 }, 1280);
                })
            }));
        let control = transport.clone();
        tasks.push(tokio::spawn(async move { control.control().await }));
        let probes = transport.clone();
        tasks.push(tokio::spawn(async move { probes.probes().await }));
        // Transport-wide feedback names SSRC 0.
        let zero_transport = transport.clone();
        let zero_cancel = cancel.clone();
        let zero_dtls = mic_sender.transport();
        tasks.push(tokio::spawn(async move {
            let stream = loop {
                if zero_cancel.is_cancelled() {
                    return;
                }
                if let Some(stream) = zero_dtls.rtcp_read_stream(0).await {
                    break stream;
                }
                tokio::select! { _ = zero_cancel.cancelled() => return, _ = tokio::time::sleep(Duration::from_millis(20)) => {} }
            };
            let mut bytes = vec![0u8; 65536];
            loop {
                let reports = tokio::select! { _ = zero_cancel.cancelled() => break, r = stream.read_rtcp(&mut bytes) => r };
                let Ok(reports) = reports else { break };
                for report in reports {
                    if let Some(feedback) = report
                        .as_any()
                        .downcast_ref::<webrtc::rtcp::transport_feedbacks::transport_layer_cc::TransportLayerCc>()
                    {
                        zero_transport.feedback(feedback);
                    }
                }
            }
        }));
        Ok(Self {
            pc,
            mic,
            mic_sender,
            screen_audio,
            slots,
            camera: Mutex::new(None),
            screen: Mutex::new(None),
            cipher,
            cancel,
            tasks: Mutex::new(tasks),
            on_state,
        })
    }

    /// `hook` is told of each connection state from now on.
    pub(crate) fn on_state(&self, hook: impl Fn(RTCPeerConnectionState) + Send + Sync + 'static) {
        *lock(&self.on_state) = Some(Box::new(hook));
    }

    fn slot(&self, kind: &str) -> Arc<VideoSlot> {
        Arc::clone(self.slots.iter().find(|s| s.kind == kind).expect("every picture kind has a slot"))
    }

    /// Each track's media section (after the offer): (kind, mid).
    pub(crate) async fn mids(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for transceiver in self.pc.get_transceivers().await {
            let (Some(mid), Some(local)) = (transceiver.mid(), transceiver.sender().await.track().await) else {
                continue;
            };
            out.push((local.id().to_owned(), mid.to_string()));
        }
        out
    }

    pub(crate) fn camera_on(&self) -> bool {
        lock(&self.camera).is_some()
    }

    /// Starts sending the camera (`spec`), both encodings.
    pub(crate) fn start_camera(&self, spec: crate::media::camera::Spec) {
        let (hi, lo) = (self.slot(track::CAM_HI), self.slot(track::CAM_LO));
        hi.keyframe.store(true, Ordering::Release);
        lo.keyframe.store(true, Ordering::Release);
        hi.activate(Some(CAM_HI.2));
        lo.activate(Some(CAM_LO.2));
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("voice-camera".into())
            .spawn(move || {
                if let Err(e) = camera_loop(&spec, &hi, &lo, &flag) {
                    tracing::warn!(error = %format!("{e:#}"), "voice camera stopped");
                }
            })
            .ok();
        if let Some(old) = lock(&self.camera).replace(Worker { stop, thread }) {
            old.stop();
        }
    }

    pub(crate) fn stop_camera(&self) {
        if let Some(worker) = lock(&self.camera).take() {
            worker.stop();
            self.slot(track::CAM_HI).activate(None);
            self.slot(track::CAM_LO).activate(None);
        }
    }

    /// What is shared now.
    pub(crate) fn shown(&self) -> Option<Shown> {
        lock(&self.screen).as_ref().map(|s| s.shown.clone())
    }

    /// Starts sharing `shown` at `quality`, with its sound or not; true
    /// when the sound started too.
    pub(crate) fn start_screen(&self, shown: Shown, quality: Quality, audio: bool) -> bool {
        let (main, copy) = (self.slot(track::SCREEN), self.slot(track::SCREEN_720));
        main.keyframe.store(true, Ordering::Release);
        copy.keyframe.store(true, Ordering::Release);
        main.activate(Some(quality.shape().2));
        let control = Arc::new(ShareControl {
            quality: Mutex::new(quality),
            copy: AtomicBool::new(false),
            minimized: AtomicBool::new(false),
            ended: AtomicBool::new(false),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (flag, shared, what) = (stop.clone(), control.clone(), shown.clone());
        let thread = std::thread::Builder::new()
            .name("voice-screen".into())
            .spawn(move || {
                if let Err(e) = screen_loop(&what, &main, &copy, &shared, &flag) {
                    tracing::warn!(error = %format!("{e:#}"), "voice screen share stopped");
                }
                shared.ended.store(true, Ordering::Release);
            })
            .ok();
        let sound = audio.then(|| self.start_sound(&shown)).flatten();
        let with_sound = sound.is_some();
        let share = Share {
            worker: Worker { stop, thread },
            control,
            shown,
            sound,
        };
        if let Some(old) = lock(&self.screen).replace(share) {
            old.worker.stop();
        }
        with_sound
    }

    /// The share's sound on or off while sharing; true when it is on.
    pub(crate) fn set_sound(&self, audio: bool) -> bool {
        let shown = match lock(&self.screen).as_ref() {
            Some(share) if share.sound.is_some() == audio => return audio,
            Some(share) => share.shown.clone(),
            None => return false,
        };
        let sound = audio.then(|| self.start_sound(&shown)).flatten();
        let on = sound.is_some();
        if let Some(share) = lock(&self.screen).as_mut() {
            share.sound = sound;
        }
        on
    }

    fn start_sound(&self, shown: &Shown) -> Option<crate::platform::windows::process_loopback::Capture> {
        start_sound(Arc::clone(&self.screen_audio), self.cipher.clone(), shown.sound())
            .inspect_err(|e| tracing::warn!(error = %format!("{e:#}"), "voice share sound not started"))
            .ok()
    }

    pub(crate) fn stop_screen(&self) {
        if let Some(share) = lock(&self.screen).take() {
            share.worker.stop();
            self.slot(track::SCREEN).activate(None);
            self.slot(track::SCREEN_720).activate(None);
        }
    }

    /// The share as the capture last saw it: (minimized, ended).
    pub(crate) fn share_state(&self) -> Option<(bool, bool)> {
        lock(&self.screen).as_ref().map(|s| {
            (
                s.control.minimized.load(Ordering::Acquire),
                s.control.ended.load(Ordering::Acquire),
            )
        })
    }

    /// The share's quality, changed while sharing (same encoder, new rate).
    pub(crate) fn set_quality(&self, quality: Quality) {
        if let Some(share) = lock(&self.screen).as_ref() {
            *lock(&share.control.quality) = quality;
            self.slot(track::SCREEN).activate(Some(quality.shape().2));
        }
    }

    /// Someone watches the share's 720p30 copy (`voice.demand`).
    pub(crate) fn set_copy(&self, watched: bool) {
        if let Some(share) = lock(&self.screen).as_ref() {
            if share.control.copy.swap(watched, Ordering::AcqRel) != watched {
                let copy = self.slot(track::SCREEN_720);
                copy.keyframe.store(true, Ordering::Release);
                copy.activate(watched.then_some(SCREEN_720.2));
            }
        }
    }

    pub(crate) async fn close(&self) {
        self.stop_camera();
        self.stop_screen();
        self.cancel.cancel();
        for task in lock(&self.tasks).drain(..) {
            task.abort();
        }
        let _ = self.pc.close().await;
    }
}

/// An encoder for pictures of `size` at `rate`: hardware first.
fn encoder(device: &windows::Win32::Graphics::Direct3D11::ID3D11Device, size: (u32, u32), rate: Rate) -> Result<Encoder> {
    Encoder::hardware_format(device, size, Format::AVC, rate, crate::media::encoding::Color::Sdr)
        .or_else(|e| {
            tracing::debug!(error = %format!("{e:#}"), "voice picture: software encoder");
            Encoder::software_format(device, size, Format::AVC, rate)
        })
}

fn rate(bps: u32, fps: u32, tier: Tier) -> Rate {
    Rate {
        target: bps,
        peak: bps + bps / 2,
        fps,
        tier,
        quality_target: QualityTarget { bitrate: bps, fps },
        smooth: false,
    }
}

/// One encoding of a capture: its encoder (made for the first picture's
/// size) and slot.
struct Encoding<'a> {
    slot: &'a VideoSlot,
    cap: u32,
    fps: u32,
    tier: Tier,
    encoder: Option<(Encoder, (u32, u32))>,
    last: Option<Instant>,
}

impl<'a> Encoding<'a> {
    fn new(slot: &'a VideoSlot, cap: u32, fps: u32, tier: Tier) -> Self {
        Self {
            slot,
            cap,
            fps,
            tier,
            encoder: None,
            last: None,
        }
    }

    /// Time for another picture at this encoding's rate.
    fn due(&self, now: Instant) -> bool {
        self.last
            .is_none_or(|t| now.duration_since(t) + Duration::from_millis(3) >= Duration::from_secs(1) / self.fps)
    }

    fn encode(
        &mut self,
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
        size: (u32, u32),
        captured: Instant,
        cancel: &Arc<AtomicBool>,
    ) -> Result<bool> {
        let bps = self.slot.rate(self.cap);
        let mut keyframe = self.slot.take_keyframe();
        if self.encoder.as_ref().is_none_or(|(_, s)| *s != size) {
            self.encoder = Some((encoder(device, size, rate(bps, self.fps, self.tier))?, size));
            keyframe = true;
        }
        if self.last.is_none() {
            tracing::info!(kind = self.slot.kind, width = size.0, height = size.1, bps, "voice picture: first frame");
        }
        let (enc, _) = self.encoder.as_mut().expect("made above");
        keyframe |= enc.configure_rate(rate(bps, self.fps, self.tier))?;
        self.last = Some(captured);
        let frames = enc.encode_with(
            texture,
            self.slot.timestamp(captured),
            FrameControl {
                keyframe,
                ..Default::default()
            },
            cancel,
        )?;
        for frame in frames {
            if !self.slot.send(frame) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn texture_size(texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D) -> (u32, u32) {
    let mut desc = Default::default();
    // SAFETY: a plain query of the texture's description.
    unsafe { texture.GetDesc(&mut desc) };
    (desc.Width, desc.Height)
}

/// Scales `texture` within `maximum` on its device.
fn scaled(
    converter: &mut crate::platform::windows::preprocess::Converter,
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    texture: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    tier: Tier,
    maximum: (u32, u32),
) -> Result<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> {
    let mut desc = Default::default();
    // SAFETY: as above; the device's own context.
    let context = unsafe {
        texture.GetDesc(&mut desc);
        device.GetImmediateContext()?
    };
    converter.convert(
        device,
        &context,
        texture,
        desc,
        windows::Win32::Graphics::Dxgi::Common::DXGI_MODE_ROTATION_IDENTITY,
        1.0,
        None,
        crate::platform::windows::preprocess::Request {
            tier,
            maximum,
            hdr: false,
        },
        None,
    )
}

fn camera_loop(spec: &crate::media::camera::Spec, hi: &VideoSlot, lo: &VideoSlot, stop: &AtomicBool) -> Result<()> {
    let _runtime = crate::platform::windows::encoder::Runtime::new()?;
    let mut source = crate::platform::windows::source::open(&Source::Camera(spec.clone()))?;
    tracing::info!(camera = %spec.name, width = spec.mode.width, height = spec.mode.height, "voice camera started");
    let device = source.device().clone();
    let mut small = crate::platform::windows::preprocess::Converter::new(&device)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let mut big = Encoding::new(hi, CAM_HI.2, CAM_HI_FPS, Tier::P720);
    let mut low = Encoding::new(lo, CAM_LO.2, CAM_LO_FPS, Tier::P720);
    while !stop.load(Ordering::Acquire) {
        let Some(frame) = source.next(100, Tier::P720, false, false, (CAM_HI.0, CAM_HI.1))? else {
            continue;
        };
        if !frame.is_new {
            continue;
        }
        let now = frame.captured;
        if big.due(now) && !big.encode(&device, &frame.texture, texture_size(&frame.texture), now, &cancel)? {
            break;
        }
        if low.due(now) {
            let texture = scaled(&mut small, &device, &frame.texture, Tier::P720, (CAM_LO.0, CAM_LO.1))?;
            if !low.encode(&device, &texture, texture_size(&texture), now, &cancel)? {
                break;
            }
        }
    }
    Ok(())
}

fn quality_name(quality: &Mutex<Quality>) -> &'static str {
    lock(quality).name()
}

/// Where a share's pictures come from.
enum Pictures {
    Desktop(Box<dyn crate::platform::windows::source::FrameSource>),
    Window(crate::platform::windows::window_capture::Capture, crate::platform::windows::preprocess::Converter),
}

impl Pictures {
    fn open(shown: &Shown) -> Result<Self> {
        Ok(match shown {
            Shown::Screen(screen) => Self::Desktop(Box::new(crate::platform::windows::capture::Desktop::open_local(screen)?)),
            Shown::Window { hwnd, .. } => {
                let capture = crate::platform::windows::window_capture::Capture::open(*hwnd)?;
                let converter = crate::platform::windows::preprocess::Converter::new(capture.device())?;
                Self::Window(capture, converter)
            }
        })
    }

    fn device(&self) -> windows::Win32::Graphics::Direct3D11::ID3D11Device {
        match self {
            Self::Desktop(source) => source.device().clone(),
            Self::Window(capture, _) => capture.device().clone(),
        }
    }

    fn backend(&self) -> &'static str {
        match self {
            Self::Desktop(source) => source.backend_name(),
            Self::Window(..) => "WGC",
        }
    }

    /// The next picture within `maximum`: `None` when no new one came
    /// within `timeout` ms.
    fn next(
        &mut self,
        timeout: u32,
        tier: Tier,
        maximum: (u32, u32),
    ) -> Result<Option<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D>> {
        match self {
            Self::Desktop(source) => Ok(source.next(timeout, tier, true, false, maximum)?.map(|f| f.texture)),
            Self::Window(capture, converter) => {
                let Some(texture) = capture.next(Duration::from_millis(u64::from(timeout)))? else {
                    return Ok(None);
                };
                let device = capture.device().clone();
                Ok(Some(scaled(converter, &device, &texture, tier, maximum)?))
            }
        }
    }
}

fn screen_loop(
    shown: &Shown,
    main: &VideoSlot,
    copy: &VideoSlot,
    control: &ShareControl,
    stop: &AtomicBool,
) -> Result<()> {
    let _runtime = crate::platform::windows::encoder::Runtime::new()?;
    let mut source = Pictures::open(shown)?;
    tracing::info!(backend = source.backend(), quality = quality_name(&control.quality), "voice screen share started");
    let device = source.device();
    let window = match shown {
        Shown::Window { hwnd, .. } => Some(*hwnd),
        Shown::Screen(_) => None,
    };
    let mut first = true;
    let mut small = crate::platform::windows::preprocess::Converter::new(&device)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let mut quality = *lock(&control.quality);
    let (_, fps, cap) = quality.shape();
    let mut picture = Encoding::new(main, cap, fps, Tier::P1080);
    let mut p720 = Encoding::new(copy, SCREEN_720.2, 30, Tier::P720);
    // The last picture, encoded again at the rate when the screen is still.
    let mut last: Option<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D> = None;
    while !stop.load(Ordering::Acquire) {
        if let Some(hwnd) = window {
            if !crate::platform::windows::window_capture::exists(hwnd) {
                tracing::info!("voice screen share: the window closed");
                break;
            }
            control
                .minimized
                .store(crate::platform::windows::window_capture::minimized(hwnd), Ordering::Release);
        }
        let now_quality = *lock(&control.quality);
        if now_quality != quality {
            quality = now_quality;
            let (_, fps, cap) = quality.shape();
            picture.fps = fps;
            picture.cap = cap;
            main.keyframe.store(true, Ordering::Release);
        }
        let (maximum, _, _) = quality.shape();
        let tier = if maximum.1 > 720 { Tier::P1080 } else { Tier::P720 };
        let frame = source.next(1000 / picture.fps.max(1), tier, maximum)?;
        let now = Instant::now();
        let texture = match frame {
            Some(texture) => {
                if first {
                    first = false;
                    let (width, height) = texture_size(&texture);
                    tracing::info!(width, height, "voice screen share: first picture");
                }
                last = Some(texture.clone());
                texture
            }
            // A still screen: the last picture again, at least every second.
            None => match &last {
                Some(t) if picture.last.is_none_or(|l| now.duration_since(l) >= Duration::from_secs(1)) => t.clone(),
                _ => continue,
            },
        };
        if picture.due(now) && !picture.encode(&device, &texture, texture_size(&texture), now, &cancel)? {
            break;
        }
        if control.copy.load(Ordering::Acquire) && p720.due(now) {
            let small_texture = scaled(&mut small, &device, &texture, Tier::P720, (SCREEN_720.0, SCREEN_720.1))?;
            if !p720.encode(&device, &small_texture, texture_size(&small_texture), now, &cancel)? {
                break;
            }
        }
    }
    Ok(())
}

/// Samples per channel in one packet of the share's sound (20 ms).
const SOUND_FRAME: usize = 960;
const SOUND_BITRATE: u32 = 128_000;

/// Starts the share's sound (§9): `target`'s, Opus 48 kHz stereo in 20 ms
/// packets on `track`, sealed with `cipher` in an end-to-end channel.
fn start_sound(
    track: Arc<TrackLocalStaticRTP>,
    cipher: Option<super::uplink::Cipher>,
    target: crate::platform::windows::process_loopback::Target,
) -> Result<crate::platform::windows::process_loopback::Capture> {
    use webrtc::track::track_local::TrackLocalWriter;
    let runtime = tokio::runtime::Handle::current();
    let mut encoder = crate::media::audio::encoder::create_encoder(crate::media::audio::encoder::Config {
        stereo: true,
        fec: true,
        dtx: true,
        cbr: false,
        bitrate: SOUND_BITRATE,
        bitrate_limit: SOUND_BITRATE,
        packet_ms: 20,
        max_playback_rate: 48_000,
    })
    .map_err(|e| anyhow::anyhow!("share sound encoder: {e:?}"))?;
    let mut frame: Vec<f32> = Vec::with_capacity(SOUND_FRAME * 4);
    let mut out = [0u8; 1500];
    let mut sequence: u16 = rand::random();
    let mut timestamp: u32 = rand::random();
    let mut sent = false;
    crate::platform::windows::process_loopback::Capture::start(target, move |block| {
        frame.extend_from_slice(block);
        if frame.len() < SOUND_FRAME * 2 {
            return;
        }
        let pcm: Vec<f32> = frame.drain(..SOUND_FRAME * 2).collect();
        let this_ts = timestamp;
        timestamp = timestamp.wrapping_add(SOUND_FRAME as u32);
        let length = match encoder.encode_float_to_slice(&pcm, &mut out) {
            Ok(n) => n,
            Err(e) => {
                tracing::debug!(error = ?e, "share sound frame not encoded");
                return;
            }
        };
        // Silence under DTX: nothing to send.
        if length <= 2 {
            return;
        }
        let payload = match &cipher {
            Some(cipher) => match lock(cipher).encrypt(crate::media::frame_crypto::Codec::Opus, &out[..length]) {
                Ok(p) => bytes::Bytes::from(p),
                Err(_) => return,
            },
            None => bytes::Bytes::copy_from_slice(&out[..length]),
        };
        let packet = webrtc::rtp::packet::Packet {
            header: webrtc::rtp::header::Header {
                version: 2,
                sequence_number: sequence,
                timestamp: this_ts,
                ..Default::default()
            },
            payload,
        };
        sequence = sequence.wrapping_add(1);
        if !sent {
            sent = true;
            tracing::info!(?target, "voice share sound: first packet");
        }
        if let Err(e) = runtime.block_on(track.write_rtp(&packet)) {
            tracing::trace!(error = %e, "share sound packet not written");
        }
    })
}
