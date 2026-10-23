//! The forwarder uplink (`docs/protocol/deployment-design.md` §5.4): one
//! connection from this host to the server's forwarder that carries each
//! forwarded encoder group's frames once. Every track watches one group as
//! an observer and sends through the same packetizer, pacing, repair and
//! RS-FEC as a participant's track; the forwarder passes the packets to the
//! participants unchanged.

use super::encode_group::{self, REGISTRY};
use super::media::{self, Owner, Sending, Timeline};
use crate::features::host::format::{Codec, Negotiated};
use crate::features::host::parameters::{self, Bounds};
use crate::features::host::track::VideoTrack;
use crate::features::host::transport::Transport;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::track::track_local::TrackLocal;

/// One track per encoder the host may run.
pub(crate) const TRACKS: usize = encode_group::MAX_ENCODERS;
/// Before the first group output: a stream is not limited below this.
const START: Bounds = Bounds {
    reservation: 2_000_000,
    maximum: 50_000_000,
    initial: 8_000_000,
    probe: 20_000_000,
};
/// Missing keyframes are asked for again after this.
const KEYFRAME_RETRY: Duration = Duration::from_millis(300);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The uplink sends while it is open.
#[derive(Clone)]
struct UplinkOwner(CancellationToken);

impl Owner for UplinkOwner {
    fn requested(&self) -> bool {
        !self.0.is_cancelled()
    }
    fn fail(&self, message: String) {
        tracing::warn!(%message, "forwarder uplink stream failed");
        self.0.cancel();
    }
    fn frame(&self) {}
}

struct Running {
    serial: u64,
    cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    observer: Option<std::thread::JoinHandle<()>>,
}

impl Running {
    fn stop(mut self) {
        self.cancel.cancel();
        for task in self.tasks.drain(..) {
            task.abort();
        }
        if let Some(thread) = self.observer.take() {
            std::thread::spawn(move || {
                let _ = thread.join();
            });
        }
    }
}

struct Slot {
    track: Arc<VideoTrack>,
    sender: Arc<RTCRtpSender>,
    transport: Transport,
    running: Option<Running>,
}

pub(crate) struct Uplink {
    pc: Arc<RTCPeerConnection>,
    slots: Mutex<Vec<Slot>>,
    cancel: CancellationToken,
    state: watch::Receiver<RTCPeerConnectionState>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// The host's media engine for the forwarder: every codec and extension of
/// the shared tables (the forwarder accepts them all).
fn media_engine() -> Result<MediaEngine> {
    let mut media = MediaEngine::default();
    farwing_rtc_wire::register_codecs(&mut media)?;
    farwing_rtc_wire::register_header_extensions(
        &mut media,
        &[farwing_rtc_wire::DEPENDENCY_DESCRIPTOR_URI],
    )?;
    Ok(media)
}

impl Uplink {
    /// A connection with [`TRACKS`] send-only video tracks; returns it and
    /// its offer (candidates gathered).
    pub(crate) async fn new(ice_servers: Vec<RTCIceServer>) -> Result<(Self, String)> {
        let cancel = CancellationToken::new();
        let owner = cancel.clone();
        let transport = Transport::for_owner(
            Arc::new(move || !owner.is_cancelled()),
            cancel.clone(),
            START,
        );
        let mut registry = Registry::new();
        registry.add(Box::new(transport.clone()));
        registry.add(Box::new(crate::transport::rtcp_timing::RtcpTiming::new()));
        let mut settings = crate::transport::rtc::ConnectionCore::settings();
        settings.enable_sender_rtx(true);
        settings.enable_sender_rsfec(true);
        let api = APIBuilder::new()
            .with_media_engine(media_engine()?)
            .with_interceptor_registry(registry)
            .with_setting_engine(settings)
            .build();
        let pc = Arc::new(
            api.new_peer_connection(RTCConfiguration {
                ice_servers,
                ..Default::default()
            })
            .await
            .context("create forwarder uplink connection")?,
        );
        let mut slots = Vec::new();
        for index in 0..TRACKS {
            let track = Arc::new(VideoTrack::new(
                index,
                Codec::H264,
                Arc::new(Negotiated::deferred()),
            ));
            let slot_transport = if index == 0 {
                transport.clone()
            } else {
                transport.fork(index)
            };
            let transceiver = pc
                .add_transceiver_from_track(
                    track.clone() as Arc<dyn TrackLocal + Send + Sync>,
                    Some(RTCRtpTransceiverInit {
                        direction: RTCRtpTransceiverDirection::Sendonly,
                        send_encodings: Vec::new(),
                    }),
                )
                .await?;
            let sender = transceiver.sender().await;
            for encoding in sender.get_parameters().await.encodings {
                slot_transport.register_ssrc(encoding.ssrc);
            }
            slots.push(Slot {
                track,
                sender,
                transport: slot_transport,
                running: None,
            });
        }
        let (state_tx, state) = watch::channel(RTCPeerConnectionState::New);
        let state_transport = transport.clone();
        let dtls = Arc::downgrade(&slots[0].sender.transport());
        pc.on_peer_connection_state_change(Box::new(move |connection_state| {
            tracing::info!(state = ?connection_state, "forwarder uplink state changed");
            state_transport.network(connection_state == RTCPeerConnectionState::Connected);
            let _ = state_tx.send(connection_state);
            let transport = state_transport.clone();
            let dtls = dtls.clone();
            Box::pin(async move {
                if connection_state == RTCPeerConnectionState::Connected
                    && let Some(dtls) = dtls.upgrade()
                {
                    transport.srtp_overhead(dtls.rtp_authentication_overhead().await);
                }
            })
        }));
        let route_transport = transport.clone();
        slots[0]
            .sender
            .transport()
            .ice_transport()
            .on_selected_candidate_pair_change(Box::new(move |pair| {
                let route = crate::transport::rtc::connection_route(
                    pair.local.typ,
                    &pair.local.address,
                    pair.remote.typ,
                    &pair.remote.address,
                );
                tracing::info!(local_type = ?pair.local.typ, remote_type = ?pair.remote.typ, route,
                    "forwarder uplink path");
                route_transport.route_changed();
                route_transport.selected_route(route);
                let transport = route_transport.clone();
                let tcp = pair.local.protocol == webrtc::ice_transport::ice_protocol::RTCIceProtocol::Tcp;
                let ipv6 = pair
                    .local
                    .address
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_ipv6());
                Box::pin(async move {
                    let network = if ipv6 { 40 } else { 20 } + if tcp { 20 } else { 8 };
                    transport.route(network, 1280);
                })
            }));
        let mut tasks = Vec::new();
        for slot in &slots {
            let repairs = slot.transport.clone();
            tasks.push(tokio::spawn(async move { repairs.repairs().await }));
            let fec = slot.transport.clone();
            tasks.push(tokio::spawn(async move { fec.fec_worker().await }));
        }
        let control = transport.clone();
        tasks.push(tokio::spawn(async move { control.control().await }));
        let probes = transport.clone();
        tasks.push(tokio::spawn(async move { probes.probes().await }));
        // Transport-wide feedback may name SSRC 0.
        let zero_transport = transport.clone();
        let zero_cancel = cancel.clone();
        let zero_dtls = slots[0].sender.transport();
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
                    if let Some(feedback) = report.as_any().downcast_ref::<webrtc::rtcp::transport_feedbacks::transport_layer_cc::TransportLayerCc>() {
                        zero_transport.feedback(feedback);
                    }
                }
            }
        }));
        let offer = pc.create_offer(None).await?;
        let mut gathered = pc.gathering_complete_promise().await;
        pc.set_local_description(offer).await?;
        // The forwarder answers checks from any address: host candidates
        // suffice, relayed ones need not be waited for.
        let _ = tokio::time::timeout(Duration::from_secs(1), gathered.recv()).await;
        let offer = pc
            .local_description()
            .await
            .context("missing uplink offer")?
            .sdp;
        Ok((
            Self {
                pc,
                slots: Mutex::new(slots),
                cancel,
                state,
                tasks,
            },
            offer,
        ))
    }

    /// The forwarder's answer.
    pub(crate) async fn answer(&self, sdp: String) -> Result<()> {
        self.pc
            .set_remote_description(RTCSessionDescription::answer(sdp)?)
            .await
            .context("install the forwarder's answer")
    }

    /// Failed or closed for good.
    pub(crate) fn failed(&self) -> bool {
        matches!(
            *self.state.borrow(),
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
        )
    }

    /// Watch the forwarded encoder groups: each gets a track (while there
    /// are tracks), groups no longer forwarded release theirs. Returns each
    /// forwarded participant's tracks: `(participant, its track) -> uplink
    /// track ID`.
    pub(crate) fn reconcile(&self) -> BTreeMap<(u64, usize), String> {
        let forwarded = REGISTRY.forwarded();
        let mut slots = lock(&self.slots);
        // Release tracks of groups that ended or are no longer forwarded.
        for slot in slots.iter_mut() {
            let keep = slot.running.as_ref().is_some_and(|running| {
                !running.cancel.is_cancelled()
                    && forwarded.iter().any(|f| f.group.serial() == running.serial)
            });
            if !keep && let Some(running) = slot.running.take() {
                running.stop();
            }
        }
        let mut mapping = BTreeMap::new();
        for group in &forwarded {
            let serial = group.group.serial();
            let index = match slots
                .iter()
                .position(|s| s.running.as_ref().is_some_and(|r| r.serial == serial))
            {
                Some(index) => index,
                None => {
                    let Some(index) = slots.iter().position(|s| s.running.is_none()) else {
                        tracing::warn!(serial, "no uplink track left for a forwarded group");
                        continue;
                    };
                    let running = start(
                        index,
                        &slots[index],
                        group,
                        &self.cancel,
                        Arc::downgrade(&self.pc),
                    );
                    tracing::info!(track = index, key = ?group.group.key(), "uplink track follows an encoder group");
                    slots[index].running = Some(running);
                    index
                }
            };
            for member in &group.members {
                mapping.insert(*member, format!("video_{index}"));
            }
        }
        mapping
    }

    pub(crate) async fn close(&self) {
        self.cancel.cancel();
        let running: Vec<Running> = lock(&self.slots)
            .iter_mut()
            .filter_map(|s| s.running.take())
            .collect();
        for running in running {
            running.stop();
        }
        for task in &self.tasks {
            task.abort();
        }
        let _ = self.pc.close().await;
    }
}

/// Starts uplink track `index` on `group`.
fn start(
    index: usize,
    slot: &Slot,
    group: &encode_group::Forwarded,
    uplink: &CancellationToken,
    connection: std::sync::Weak<RTCPeerConnection>,
) -> Running {
    let cancel = uplink.child_token();
    let timeline = Timeline::new();
    let keyframe = Arc::new(AtomicBool::new(true));
    let (frames_tx, frames) = mpsc::channel(1);
    let owner = UplinkOwner(cancel.clone());
    let reports = media::spawn_reports(
        index,
        &timeline,
        slot.sender.clone(),
        connection,
        cancel.clone(),
        owner.clone(),
        Arc::new(AtomicBool::new(true)),
    );
    let track_feedback = Arc::new(crate::features::host::recovery::TrackFeedback::default());
    let feedback = media::spawn_feedback(
        index,
        slot.sender.clone(),
        slot.transport.clone(),
        cancel.clone(),
        Arc::default(),
        track_feedback.clone(),
        keyframe.clone(),
    );
    let sending = media::spawn_sending(
        Sending {
            index,
            capture_index: 0,
            timeline: timeline.clone(),
            track: slot.track.clone(),
            extension_sender: slot.sender.clone(),
            sending_formats: Arc::new(Negotiated::deferred()),
            sending_keyframe: keyframe.clone(),
            transport: slot.transport.clone(),
            send_handle: owner,
            sent_feedback: track_feedback,
            send_cancel: cancel.clone(),
            sent_first_frame: Arc::new(AtomicBool::new(false)),
            worker_started: Instant::now(),
        },
        frames,
    );
    let mut membership = REGISTRY.observe(&group.group);
    let transport = slot.transport.clone();
    let observer_cancel = cancel.clone();
    let started = timeline.started;
    let observer = std::thread::Builder::new()
        .name("host-uplink".into())
        .spawn(move || {
            transport.configure(START);
            let mut awaiting = true;
            let mut asked: Option<Instant> = None;
            while !observer_cancel.is_cancelled() {
                if membership.ended() {
                    tracing::info!(track = index, "forwarded encoder group ended");
                    observer_cancel.cancel();
                    break;
                }
                if let Some(output) = membership.output_changed() {
                    transport.configure(parameters::automatic(
                        output.format,
                        output.quality,
                        output.budget_size,
                        output.fps,
                        false,
                        4,
                    ));
                    transport.fps(output.fps);
                }
                let ready = transport.media_ready();
                membership.vote(ready.then(|| encode_group::Vote {
                    peak: transport.media_rate(),
                    ratio: transport.cwnd_ratio(),
                    fps: 0,
                }));
                if keyframe.swap(false, Ordering::AcqRel) {
                    awaiting = true;
                    asked = None;
                }
                let due = asked.is_none_or(|at| at.elapsed() >= KEYFRAME_RETRY);
                let Some(received) = membership.receive(Duration::from_millis(20)) else {
                    if awaiting && ready && due {
                        membership.request_keyframe();
                        asked = Some(Instant::now());
                    }
                    continue;
                };
                if received.gap || !ready {
                    awaiting = true;
                }
                if !ready {
                    continue;
                }
                if awaiting && !received.frame.keyframe {
                    if due {
                        membership.request_keyframe();
                        asked = Some(Instant::now());
                    }
                    continue;
                }
                let Some(encoded) = received.frame.encoded(started) else {
                    continue;
                };
                if encoded.keyframe {
                    awaiting = false;
                    asked = None;
                }
                if frames_tx.blocking_send(encoded).is_err() {
                    break;
                }
            }
        })
        .ok();
    Running {
        serial: group.group.serial(),
        cancel,
        tasks: vec![reports, feedback, sending],
        observer,
    }
}
