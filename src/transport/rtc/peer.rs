use super::channels::{DATA_CHANNEL_LABELS, DataChannels, official_data_channel_init};
use super::clock::{RemoteNtpEstimator, observe_remote_ntp};
use super::core::ConnectionCore;
use super::feedback::{RTP_DEFAULT_RTT, send_full_intra_request, send_picture_loss_indication};
use super::negotiation::{
    UU_LOCAL_SEND_START_BPS, candidate_is_relay, negotiated_mixed_kcp_version, register_uu_codecs,
    register_uu_header_extensions, remove_relay_candidates_from_sdp,
    uu_transport_feedback_interval,
};
use super::receive::{TrackForwardContext, forward_remote_track};
use super::statistics::{connection_route, sample_network_performance};
use super::tracks::{
    ForwardedTrack, MediaKind, RtpForwardConfig, RtpForwarder, VideoTrackRegistry, VideoTrackSource,
};
use super::workers::std_mutex_lock;
use crate::diagnostics::performance::PerformanceMonitor;
use crate::features::stream_control::StreamControlHandle;
use crate::media::{ConnectionMediaProfile, TransportChoice, VideoCodec};
use crate::transport::rsfec::RsFecConfig;
use crate::transport::rtcp_timing::RtcpTiming;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, broadcast, mpsc, watch};
use webrtc::api::interceptor_registry::{
    configure_twcc_receiver_with_builder, configure_twcc_sender_only,
};
use webrtc::api::media_engine::{MIME_TYPE_OPUS, MediaEngine};
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::interceptor::report::receiver::ReceiverReport;
use webrtc::interceptor::twcc::receiver::Receiver as TransportFeedbackReceiver;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTPCodecType};
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::stats::StatsReportType;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
// Peer construction, negotiation lifecycle and coordinated shutdown.

#[derive(Clone, Default)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

impl From<IceServer> for RTCIceServer {
    fn from(value: IceServer) -> Self {
        Self {
            urls: value.urls,
            username: value.username,
            credential: value.credential,
        }
    }
}

pub struct NativePeer {
    core: ConnectionCore,
    pub(super) connection_states: Mutex<mpsc::UnboundedReceiver<RTCPeerConnectionState>>,
    pub(super) local_candidate_tx: broadcast::Sender<Option<RTCIceCandidateInit>>,
    pub(super) performance: PerformanceMonitor,
    pub(super) nack_rtt_micros: Arc<AtomicU64>,
    pub(super) rtcp_timing: RtcpTiming,
    pub(super) p2p_only: AtomicBool,
    pub(super) ice_servers: Vec<IceServer>,
    pub(super) data_channels: DataChannels,
    pub(super) video_tracks: VideoTrackRegistry,
}

impl NativePeer {
    pub(crate) fn port_mapping(&self) -> Arc<crate::features::port_mapping::Transport> {
        Arc::clone(&self.data_channels.port_mapping)
    }
    pub async fn new(ice_servers: Vec<IceServer>, transport: TransportChoice) -> Result<Self> {
        let profile = crate::media::ConnectionMediaOptions::default()
            .resolve(crate::media::LocalDisplayInfo::FALLBACK)?;
        Self::new_with_profile(ice_servers, transport, profile).await
    }

    pub(crate) async fn new_with_profile(
        mut ice_servers: Vec<IceServer>,
        transport: TransportChoice,
        profile: ConnectionMediaProfile,
    ) -> Result<Self> {
        let configured_ice_servers = ice_servers.clone();
        let mut ice_url_schemes = BTreeMap::<String, usize>::new();
        for url in ice_servers.iter().flat_map(|server| &server.urls) {
            let scheme = url
                .split_once(':')
                .map_or("unknown", |(scheme, _)| scheme)
                .to_ascii_lowercase();
            *ice_url_schemes.entry(scheme).or_default() += 1;
        }
        if transport == TransportChoice::P2p {
            for server in &mut ice_servers {
                server
                    .urls
                    .retain(|url| url.to_ascii_lowercase().starts_with("stun:"));
            }
            ice_servers.retain(|server| !server.urls.is_empty());
        }
        tracing::debug!(
            ?transport,
            ice_server_count = ice_servers.len(),
            ?ice_url_schemes,
            "creating native ICE transport"
        );
        let mut media_engine = MediaEngine::default();
        register_uu_codecs(&mut media_engine)?;
        register_uu_header_extensions(&mut media_engine)?;
        let mut registry = Registry::new();
        let rtcp_timing = RtcpTiming::new();
        // Register before RR: the report worker must write through the XR
        // adapter, not capture the unwrapped transport writer.
        registry.add(Box::new(rtcp_timing.clone()));
        registry.add(Box::new(
            ReceiverReport::builder()
                .with_jittered_media_intervals(Duration::from_secs(1), Duration::from_secs(5)),
        ));
        let (feedback_interval_tx, feedback_interval_rx) =
            watch::channel(Duration::from_millis(100));
        let registry = configure_twcc_receiver_with_builder(
            registry,
            &mut media_engine,
            TransportFeedbackReceiver::builder().with_interval_updates(feedback_interval_rx),
        )
        .context("register WebRTC transport feedback interceptors")?;
        let registry = configure_twcc_sender_only(registry, &mut media_engine)
            .context("register audio sender transport sequence extension")?;
        let core = ConnectionCore::new(
            media_engine,
            registry,
            ConnectionCore::settings(),
            RTCConfiguration {
                ice_servers: ice_servers.into_iter().map(Into::into).collect(),
                ice_transport_policy: if transport == TransportChoice::Relay {
                    RTCIceTransportPolicy::Relay
                } else {
                    RTCIceTransportPolicy::All
                },
                ..Default::default()
            },
        )
        .await?;
        let connection = core.connection.clone();
        let (local_candidate_tx, _) = broadcast::channel(256);
        let local_candidate_tx_for_handler = local_candidate_tx.clone();
        connection.on_ice_candidate(Box::new(move |candidate| {
            let candidate = candidate.and_then(|candidate| candidate.to_json().ok());
            let _ = local_candidate_tx_for_handler.send(candidate);
            Box::pin(async {})
        }));
        let performance = PerformanceMonitor::new("自动");
        let nack_rtt_micros = Arc::new(AtomicU64::new(
            RTP_DEFAULT_RTT.as_micros().min(u128::from(u64::MAX)) as u64,
        ));
        let transceiver = |direction| RTCRtpTransceiverInit {
            direction,
            send_encodings: Vec::new(),
        };
        let receive_only = || transceiver(RTCRtpTransceiverDirection::Recvonly);
        // The official controller always exposes an `audio_0` Opus sender,
        // even when no microphone samples are produced. A bare sendrecv
        // transceiver omits the MSID/SSRC block and the controlled client does
        // not treat that offer as the desktop-controller profile.
        let audio_track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48_000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;stereo=1;useinbandfec=1".to_owned(),
                ..Default::default()
            },
            "audio_0".to_owned(),
            "audio_0".to_owned(),
        ));
        let audio_transceiver = connection
            .add_transceiver_from_track(
                audio_track.clone() as Arc<dyn TrackLocal + Send + Sync>,
                Some(transceiver(RTCRtpTransceiverDirection::Sendrecv)),
            )
            .await
            .context("add official audio_0 send/receive transceiver")?;
        // The official controller offers five recvonly video m-lines. They are
        // not simulcast layers of one transceiver; the host answers each one
        // independently and uses them for its desktop/auxiliary video tracks.
        for index in 0..5 {
            connection
                .add_transceiver_from_kind(RTPCodecType::Video, Some(receive_only()))
                .await
                .with_context(|| format!("add receive video transceiver {index}"))?;
        }

        let mut local_channels = Vec::with_capacity(DATA_CHANNEL_LABELS.len());
        for label in DATA_CHANNEL_LABELS {
            let channel = connection
                .create_data_channel(label, Some(official_data_channel_init(label)))
                .await
                .with_context(|| format!("create {label}"))?;
            local_channels.push(channel);
        }
        let video_tracks = VideoTrackRegistry::default();
        let data_channels = DataChannels::new(
            local_channels,
            performance.clone(),
            profile,
            core.control.clone(),
            Arc::downgrade(&connection),
        );
        let microphone = data_channels.stream_control.microphone().clone();
        data_channels.workers.spawn(async move {
            microphone.send(audio_track).await;
        });
        let microphone = data_channels.stream_control.microphone().clone();
        let audio_sender = audio_transceiver.sender().await;
        let reports = data_channels.stream_control.microphone().clone();
        let reports_sender = audio_sender.clone();
        let reports_connection = Arc::downgrade(&connection);
        data_channels.workers.spawn(async move {
            reports
                .send_reports(reports_connection, reports_sender)
                .await;
        });
        data_channels.workers.spawn(async move {
            microphone.feedback(audio_sender).await;
        });
        data_channels.workers.spawn(sample_network_performance(
            Arc::clone(&connection),
            performance.clone(),
            Arc::clone(&nack_rtt_micros),
            rtcp_timing.clone(),
            data_channels.stream_control.clone(),
            video_tracks.clone(),
        ));
        let data_channels_for_remote = data_channels.clone();
        connection.on_data_channel(Box::new(move |channel| {
            data_channels_for_remote.attach_remote_channel(channel);
            Box::pin(async {})
        }));

        let (connection_state_tx, connection_states) = mpsc::unbounded_channel();
        let mouse_transport = data_channels.stream_control.clone();
        connection.on_peer_connection_state_change(Box::new(move |state| {
            mouse_transport.set_mouse_transport_ready(state == RTCPeerConnectionState::Connected);
            let connection_state_tx = connection_state_tx.clone();
            // 18A25C/18B44F -> 18874E -> 149E08 -> 1DFE68. An initial
            // estimate exists even without outgoing media. Unknown/Connecting
            // retain the last interval; a usable or lost secure transport updates it.
            let send_bitrate_bps = match state {
                RTCPeerConnectionState::Connected => Some(UU_LOCAL_SEND_START_BPS),
                RTCPeerConnectionState::Disconnected
                | RTCPeerConnectionState::Failed
                | RTCPeerConnectionState::Closed => Some(0),
                _ => None,
            };
            if let Some(bitrate) = send_bitrate_bps {
                let interval = uu_transport_feedback_interval(bitrate);
                if feedback_interval_tx.send_if_modified(|current| {
                    let changed = *current != interval;
                    *current = interval;
                    changed
                }) {
                    tracing::debug!(
                        send_bitrate_bps = bitrate,
                        interval_ms = interval.as_millis(),
                        "updated UU transport feedback budget"
                    );
                }
            }
            Box::pin(async move {
                tracing::debug!(%state, "WebRTC peer state changed");
                let _ = connection_state_tx.send(state);
            })
        }));
        connection.on_ice_connection_state_change(Box::new(move |state| {
            Box::pin(async move {
                tracing::debug!(%state, "ICE connection state changed");
            })
        }));
        connection.on_ice_gathering_state_change(Box::new(move |state| {
            Box::pin(async move {
                tracing::debug!(%state, "ICE gathering state changed");
            })
        }));
        connection.on_signaling_state_change(Box::new(move |state| {
            Box::pin(async move {
                tracing::debug!(%state, "WebRTC signaling state changed");
            })
        }));

        Ok(Self {
            core,
            connection_states: Mutex::new(connection_states),
            local_candidate_tx,
            performance,
            nack_rtt_micros,
            rtcp_timing,
            p2p_only: AtomicBool::new(transport == TransportChoice::P2p),
            ice_servers: configured_ice_servers,
            data_channels,
            video_tracks,
        })
    }

    pub fn performance_monitor(&self) -> PerformanceMonitor {
        self.performance.clone()
    }

    pub(crate) fn video_tracks(&self) -> VideoTrackRegistry {
        self.video_tracks.clone()
    }
    pub(crate) fn pause_video(&self) {
        for track in self.video_tracks.all() {
            track.pause();
        }
    }

    pub(crate) fn viewing_scope(&self) -> Arc<crate::lifecycle::TaskScope> {
        self.data_channels.workers.child("viewing windows")
    }

    pub(crate) fn stream_control_handle(&self) -> StreamControlHandle {
        self.data_channels.stream_control.clone()
    }

    pub(crate) fn set_stream_video_stream(&self, codec: VideoCodec, video_track_index: i32) {
        self.select_video_statistics(video_track_index);
        self.data_channels
            .stream_control
            .set_video_stream(codec, video_track_index);
    }

    pub(crate) fn select_viewed_video_track(&self, video_track_index: i32) {
        self.select_video_statistics(video_track_index);
        self.data_channels
            .stream_control
            .select_viewed_video_track(video_track_index);
    }

    pub(super) fn select_video_statistics(&self, video_track_index: i32) {
        if let Some(track) = self.video_tracks.get(video_track_index) {
            self.rtcp_timing.select_video(track.metadata.ssrc);
        }
        if let Ok(index) = u64::try_from(video_track_index) {
            self.performance.set_active_video_track(index);
        }
    }

    pub(crate) fn allows_network_switch(&self) -> bool {
        !self.p2p_only.load(Ordering::Acquire)
    }

    pub(crate) fn configure_network_control(
        &self,
        transport: TransportChoice,
        server_forced: bool,
    ) {
        let has_turns = self
            .ice_servers
            .iter()
            .flat_map(|server| &server.urls)
            .any(|url| url.to_ascii_lowercase().contains("turns:"));
        self.stream_control_handle().network_control().configure(
            transport == TransportChoice::Relay,
            server_forced,
            has_turns,
        );
    }

    pub(crate) fn accept_manual_network_policy(&self) {
        self.p2p_only.store(false, Ordering::Release);
    }

    pub(crate) async fn selected_transport(&self) -> Option<(bool, String, String)> {
        let pair = self
            .core
            .connection
            .sctp()
            .transport()
            .ice_transport()
            .get_selected_candidate_pair()
            .await?;
        let is_relay = pair.local.typ == RTCIceCandidateType::Relay
            || pair.remote.typ == RTCIceCandidateType::Relay;
        Some((
            is_relay,
            pair.local.relay_protocol,
            pair.remote.relay_protocol,
        ))
    }

    pub(crate) async fn selected_route_details(&self) -> Option<String> {
        let pair = self
            .core
            .connection
            .sctp()
            .transport()
            .ice_transport()
            .get_selected_candidate_pair()
            .await?;
        let route = connection_route(
            pair.local.typ,
            &pair.local.address,
            pair.remote.typ,
            &pair.remote.address,
        );
        let protocol = pair.local.protocol.to_string().to_uppercase();
        let relay = if route == "relay" {
            let relay_protocol = if !pair.local.relay_protocol.is_empty() {
                pair.local.relay_protocol.as_str()
            } else {
                pair.remote.relay_protocol.as_str()
            };
            if relay_protocol.is_empty() {
                String::new()
            } else {
                format!(" / {}", relay_protocol.to_uppercase())
            }
        } else {
            String::new()
        };
        Some(format!(
            "{route} · {protocol}{relay} · {}:{} → {}:{} · {:?}/{:?}",
            pair.local.address,
            pair.local.port,
            pair.remote.address,
            pair.remote.port,
            pair.local.typ,
            pair.remote.typ
        ))
    }

    pub(crate) async fn switch_ice_network(
        &self,
        transport_type: u8,
        attempt_switch_type: u8,
    ) -> Result<()> {
        if transport_type == 0 {
            bail!("unsupported official ICE transport type {transport_type}");
        }
        let tls_only = transport_type != 3 && attempt_switch_type == 2;
        let selected_servers = if tls_only {
            self.ice_servers
                .iter()
                .filter_map(|server| {
                    let mut selected = server.clone();
                    selected
                        .urls
                        .retain(|url| url.to_ascii_lowercase().contains("turns:"));
                    (!selected.urls.is_empty()).then_some(selected)
                })
                .collect::<Vec<_>>()
        } else {
            self.ice_servers.clone()
        };
        if selected_servers.is_empty() && transport_type != 3 {
            bail!(
                "control ACK did not provide a {} server",
                if tls_only { "TURNS" } else { "usable ICE" }
            );
        }
        let policy = if transport_type == 3 {
            RTCIceTransportPolicy::All
        } else {
            RTCIceTransportPolicy::Relay
        };
        self.core
            .configure_ice(
                selected_servers.into_iter().map(Into::into).collect(),
                policy,
            )
            .await?;
        tracing::info!(
            transport_type,
            attempt_switch_type,
            relay_transport = if transport_type == 3 {
                "automatic"
            } else if tls_only {
                "TURNS only"
            } else {
                "relay, UDP preferred"
            },
            "official ICE network switch configuration applied"
        );
        Ok(())
    }

    pub async fn create_offer(&self) -> Result<String> {
        self.core.offer(false, "audio_0").await
    }

    pub async fn create_restart_offer(&self) -> Result<String> {
        self.core.offer(true, "audio_0").await
    }

    pub fn local_ice_candidates(&self) -> broadcast::Receiver<Option<RTCIceCandidateInit>> {
        self.local_candidate_tx.subscribe()
    }

    pub async fn set_remote_answer(&self, mut sdp: String, restart_ice: bool) -> Result<()> {
        tracing::debug!(restart_ice, "installing remote SDP answer");
        let mixed_kcp_version = negotiated_mixed_kcp_version(&sdp)?;
        if self.p2p_only.load(Ordering::Acquire) {
            sdp = remove_relay_candidates_from_sdp(&sdp);
        }
        let answer = RTCSessionDescription::answer(sdp).context("parse remote SDP answer")?;
        let microphone_encoding = crate::media::audio::encoder::remote_config(&answer)?;
        // Receiver::tracks is populated only after async transport startup.
        // Register the negotiated MSIDs, not just tracks that already sent RTP.
        let mut indexes = Vec::new();
        for media in answer.unmarshal()?.media_descriptions {
            if media.media_name.media != "video" || media.media_name.port.value == 0 {
                continue;
            }
            for attribute in media.attributes {
                let Some(value) = attribute.value else {
                    continue;
                };
                let words: Vec<_> = value.split_whitespace().collect();
                let id = if attribute.key == "msid" {
                    words.get(1)
                } else if attribute.key == "ssrc"
                    && words.get(1).is_some_and(|word| word.starts_with("msid:"))
                {
                    words.get(2)
                } else {
                    None
                };
                if let Some(index) = id
                    .and_then(|id| id.strip_prefix("video_"))
                    .and_then(|id| id.parse::<i32>().ok())
                {
                    indexes.push(index);
                }
            }
        }
        self.core
            .connection
            .set_remote_description(answer)
            .await
            .context("install remote SDP answer")?;
        self.data_channels
            .stream_control
            .microphone()
            .configure(microphone_encoding);
        self.data_channels
            .stream_control
            .set_available_video_tracks(indexes);
        let control = self.data_channels.stream_control.clone();
        self.core.activate_control(
            mixed_kcp_version,
            Arc::new(move |_, bytes| {
                control.handle_protocol_message(
                    bytes,
                    crate::features::stream_control::PbMessageSource::Control,
                )
            }),
        )?;
        Ok(())
    }

    pub async fn request_keyframe(&self, media_ssrc: u32) -> Result<()> {
        // 同时发 PLI 与 FIR：部分 host（macOS）只对 FIR 生成关键帧。
        let pli = send_picture_loss_indication(&self.core.connection, media_ssrc).await;
        let fir = send_full_intra_request(&self.core.connection, media_ssrc).await;
        pli.and(fir)
    }

    pub async fn next_connection_state(&self) -> Option<RTCPeerConnectionState> {
        self.connection_states.lock().await.recv().await
    }

    pub fn connection_state(&self) -> RTCPeerConnectionState {
        self.core.connection.connection_state()
    }

    pub fn ice_connection_state(&self) -> RTCIceConnectionState {
        self.core.connection.ice_connection_state()
    }

    /// UU only accepts a manual/automatic ICE-network switch after the
    /// existing transport has reached an established state.  Applying a
    /// relay-only configuration while ICE is still checking can discard the
    /// only usable candidate generation and produces the multi-second freezes
    /// that the official client avoids.
    pub(crate) fn can_switch_ice_network(&self) -> bool {
        matches!(
            self.core.connection.ice_connection_state(),
            RTCIceConnectionState::Connected | RTCIceConnectionState::Completed
        )
    }

    pub async fn ice_diagnostics(&self) -> String {
        let mut local = BTreeMap::<String, usize>::new();
        let mut remote = BTreeMap::<String, usize>::new();
        let mut pairs = BTreeMap::<String, usize>::new();
        let mut requests_sent = 0_u64;
        let mut responses_received = 0_u64;
        for report in self.core.connection.get_stats().await.reports.into_values() {
            match report {
                StatsReportType::LocalCandidate(candidate) => {
                    *local
                        .entry(format!("{:?}", candidate.candidate_type))
                        .or_default() += 1;
                }
                StatsReportType::RemoteCandidate(candidate) => {
                    *remote
                        .entry(format!("{:?}", candidate.candidate_type))
                        .or_default() += 1;
                }
                StatsReportType::CandidatePair(pair) => {
                    *pairs.entry(format!("{:?}", pair.state)).or_default() += 1;
                    requests_sent += pair.requests_sent;
                    responses_received += pair.responses_received;
                }
                _ => {}
            }
        }
        format!(
            "local={local:?}, remote={remote:?}, pairs={pairs:?}, checks_sent={requests_sent}, responses_received={responses_received}"
        )
    }

    pub async fn add_remote_candidate(&self, candidate: RTCIceCandidateInit) -> Result<()> {
        if self.p2p_only.load(Ordering::Acquire) && candidate_is_relay(&candidate.candidate) {
            tracing::trace!(candidate = ?candidate, "ignoring remote relay candidate during P2P-only attempt");
            return Ok(());
        }
        tracing::trace!(candidate = ?candidate, "adding remote ICE candidate to peer");
        self.core
            .connection
            .add_ice_candidate(candidate)
            .await
            .context("install remote ICE candidate")
    }

    /// Install a decrypted-RTP forwarding callback. The returned channel emits
    /// negotiated codec/PT metadata needed by local media consumers.
    pub async fn install_rtp_forwarder(&self, config: RtpForwardConfig) -> Result<RtpForwarder> {
        let audio = self.data_channels.stream_control.audio();
        let (announcement_tx, announcement_rx) = mpsc::unbounded_channel();
        let (forwarding_started, forwarding_ready) = watch::channel(false);
        let tracks = self.video_tracks.clone();
        // The connection owns this callback. A strong self-reference here
        // survives PeerConnection::close, which does not clear on_track.
        let connection = Arc::downgrade(&self.core.connection);
        let workers = Arc::downgrade(&self.data_channels.workers);
        let stop = self.data_channels.workers.shutdown.child_token();
        let track_stop = stop.clone();
        let performance = self.performance.clone();
        let nack_rtt_micros = Arc::clone(&self.nack_rtt_micros);
        let rtcp_timing = self.rtcp_timing.clone();

        self.core.connection
            .on_track(Box::new(move |track, receiver, _| {
                let Some(owner) = workers.upgrade() else {
                    return Box::pin(async {});
                };
                let workers = workers.clone();
                let stop = track_stop.clone();
                let config = config.clone();
                let audio = audio.clone();
                let tracks = tracks.clone();
                let announcement_tx = announcement_tx.clone();
                let mut forwarding_ready = forwarding_ready.clone();
                let connection = connection.clone();
                let mut performance = performance.clone();
                let nack_rtt_micros = Arc::new(AtomicU64::new(nack_rtt_micros.load(Ordering::Relaxed)));
                let rtcp_timing = rtcp_timing.clone();
                let _ = owner.spawn_in(stop.clone(), async move {
                    let Some(connection) = connection.upgrade() else {
                        return;
                    };
                    let kind = track.kind();
                    let id = track.id();
                    let selected = match kind {
                        RTPCodecType::Video => {
                            config
                                .video_track_id
                                .as_ref()
                                .is_none_or(|wanted| wanted == &id)
                        }
                        RTPCodecType::Audio => id == "audio_0",
                        _ => false,
                    };
                    if !selected {
                        return;
                    }

                    let media_kind = match kind {
                        RTPCodecType::Video => MediaKind::Video,
                        RTPCodecType::Audio => MediaKind::Audio,
                        _ => return,
                    };
                    let codec_parameters = track.codec();
                    let audio_generation = (media_kind == MediaKind::Audio).then(|| audio.select_source(
                        &codec_parameters.capability.mime_type, codec_parameters.capability.clock_rate,
                        codec_parameters.capability.channels)).flatten();
                    let codec = codec_parameters.capability.mime_type;
                    let codec_fmtp = codec_parameters.capability.sdp_fmtp_line;
                    let video_annexb_sinks = Arc::new(Mutex::new(Vec::new()));
                    let (video_keyframe_tx, keyframes) = watch::channel(0_u64);
                    let (feedback, feedback_rx) = mpsc::unbounded_channel();
                    let receiver_feedback = (media_kind == MediaKind::Video).then_some(feedback_rx);
                    if media_kind == MediaKind::Video {
                        let Some(index) = id.strip_prefix("video_").and_then(|id| id.parse::<i32>().ok()).filter(|id| *id >= 0) else {
                            tracing::warn!(track_id = %id, "unrecognized UU video track identifier");
                            return;
                        };
                        performance = performance.for_video_track(index as u64);
                        performance.set_video_codec(format!("{codec} · RTP PT {}", track.payload_type()));
                        let (started, ready) = watch::channel(false);
                        forwarding_ready = ready;
                        let source = Arc::new(VideoTrackSource {
                            metadata: ForwardedTrack { kind: media_kind, id: id.clone(), codec: codec.clone(), payload_type: track.payload_type(), ssrc: track.ssrc() },
                            index, performance: performance.clone(), sinks: Arc::clone(&video_annexb_sinks),
                            feedback: feedback.clone(), started, keyframes,
                            nack_rtt_micros: Arc::clone(&nack_rtt_micros),
                        });
                        {
                            let mut entries = std_mutex_lock(&tracks.entries);
                            if entries.contains_key(&index) {
                                tracing::warn!(index, "duplicate live UU video track ignored");
                                return;
                            }
                            entries.insert(index, source);
                        }
                        tracks.changed.notify_waiters();
                    }
                    // TrackRemote narrows its mutable params to the payload type of
                    // the first media packet. RTX apt belongs to the negotiated
                    // receiver codec table and must be captured from the receiver
                    // itself, before packet-driven track updates can erase it.
                    let parameters = receiver.get_parameters().await;
                    let extmap_allow_mixed =
                        connection
                            .local_description()
                            .await
                            .is_some_and(|description| {
                                description
                                    .sdp
                                    .lines()
                                    .take_while(|line| !line.starts_with("m="))
                                    .any(|line| line == "a=extmap-allow-mixed")
                            });
                    let rsfec_config = parameters
                        .codecs
                        .iter()
                        .find(|codec| {
                            codec
                                .capability
                                .mime_type
                                .eq_ignore_ascii_case("video/rs-fec-cm256")
                        })
                        .map(|codec| RsFecConfig::from_fmtp(&codec.capability.sdp_fmtp_line));
                    let video_payload_codecs = parameters
                        .codecs
                        .iter()
                        .filter(|entry| {
                            entry
                                .capability
                                .mime_type
                                .eq_ignore_ascii_case("video/H264")
                                || entry
                                    .capability
                                    .mime_type
                                    .eq_ignore_ascii_case("video/H265")
                                || entry
                                    .capability
                                    .mime_type
                                    .eq_ignore_ascii_case("video/HEVC") || entry.capability.mime_type.eq_ignore_ascii_case("video/AV1")
                        })
                        .map(|entry| {
                            (
                                entry.payload_type,
                                (
                                    entry.capability.mime_type.clone(),
                                    entry.capability.sdp_fmtp_line.clone(),
                                ),
                            )
                        })
                        .collect();
                    let rtx_payload_apt = parameters
                        .codecs
                        .iter()
                        .filter(|entry| {
                            entry.capability.mime_type.eq_ignore_ascii_case("video/rtx")
                        })
                        .filter_map(|entry| {
                            let apt = entry
                                .capability
                                .sdp_fmtp_line
                                .split(';')
                                .find_map(|part| part.trim().strip_prefix("apt="))?
                                .parse::<u8>()
                                .ok()?;
                            Some((entry.payload_type, apt))
                        })
                        .collect();
                    let flexfec_payload_types = parameters
                        .codecs
                        .iter()
                        .filter(|entry| {
                            entry
                                .capability
                                .mime_type
                                .eq_ignore_ascii_case("video/flexfec-03")
                        })
                        .map(|entry| entry.payload_type)
                        .collect();
                    let red_payload_types = parameters
                        .codecs
                        .iter()
                        .filter(|entry| {
                            entry.capability.mime_type.eq_ignore_ascii_case("video/red")
                        })
                        .map(|entry| entry.payload_type)
                        .collect();
                    let ulpfec_payload_types = parameters
                        .codecs
                        .iter()
                        .filter(|entry| {
                            entry
                                .capability
                                .mime_type
                                .eq_ignore_ascii_case("video/ulpfec")
                        })
                        .map(|entry| entry.payload_type)
                        .collect();
                    let ssrc = track.ssrc();
                    let remote_ntp =
                        Arc::new(StdMutex::new(RemoteNtpEstimator::new(Instant::now())));
                    let Some(owner) = workers.upgrade() else {
                        return;
                    };
                    if media_kind == MediaKind::Video {
                        // The first selected viewer, not the last arriving track,
                        // selects the session's RTT display source.
                        let _ = owner.spawn_in(
                            stop.clone(),
                            observe_remote_ntp(
                                Arc::clone(&receiver),
                                ssrc,
                                Arc::clone(&remote_ntp),
                                rtcp_timing,
                            ),
                        );
                    }
                    tracing::info!(
                        ?media_kind,
                        track_id = %id,
                        stream_id = %track.stream_id(),
                        codec = %codec,
                        payload_type = track.payload_type(),
                        ssrc,
                        red_payload_types = ?red_payload_types,
                        ulpfec_payload_types = ?ulpfec_payload_types,
                        flexfec_payload_types = ?flexfec_payload_types,
                        "remote RTP track selected"
                    );
                    let _ = announcement_tx.send(ForwardedTrack {
                        kind: media_kind,
                        id,
                        codec: codec.clone(),
                        payload_type: track.payload_type(),
                        ssrc,
                    });

                    let _ = owner.spawn_in(stop.clone(), async move {
                        forward_remote_track(
                            track,
                            forwarding_ready,
                            video_keyframe_tx,
                            TrackForwardContext {
                                workers,
                                stop,
                                kind: media_kind,
                                codec,
                                codec_fmtp,
                                video_payload_codecs,
                                rtx_payload_apt,
                                red_payload_types,
                                ulpfec_payload_types,
                                flexfec_payload_types,
                                rsfec_config,
                                extmap_allow_mixed,
                                audio,
                                audio_generation,
                                connection,
                                video_annexb_sinks,
                                performance,
                                nack_rtt_micros,
                                remote_ntp,
                                receiver_feedback,
                                receiver_feedback_sender: feedback,
                            },
                        )
                        .await;
                    });
                });
                Box::pin(async {})
            }));

        Ok(RtpForwarder {
            stop,
            announcements: announcement_rx,
            forwarding_started,
            tracks: self.video_tracks.clone(),
            selected_video: None,
        })
    }

    pub async fn close(&self) -> Result<()> {
        self.data_channels.stream_control.microphone().close().await;
        self.data_channels.stream_control.file_transfer().close();
        self.data_channels.stream_control.diagnostics().close();
        self.data_channels.stream_control.clipboard().suspend();
        self.data_channels.port_mapping.close();
        self.data_channels.stream_control.mouse().close().await;
        self.data_channels.workers.close().await;
        self.data_channels.stream_control.audio().close().await;
        self.core.close().await
    }
}

impl Drop for NativePeer {
    fn drop(&mut self) {
        self.data_channels.stream_control.microphone().stop();
        self.data_channels.stream_control.diagnostics().close();
        self.data_channels.stream_control.clipboard().suspend();
        self.data_channels.stream_control.mouse().set_ready(false);
        // Normal paths await close(). This also retires application tasks on
        // an exceptional owner drop instead of leaving them to hold the peer.
        self.data_channels.workers.shutdown.cancel();
    }
}
