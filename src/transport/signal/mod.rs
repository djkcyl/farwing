//! Minimal Engine.IO v4 / Socket.IO v4 framing used by the native signal client.
//!
//! Network I/O is intentionally separate from this codec so captured frames can
//! be regression-tested without contacting a live account or storing credentials.

use crate::account::api::RoomSession;
use crate::media::ConnectionMediaProfile;
use crate::media::decoder::detect_native_decoder_support;
use crate::session::negotiation::build_control_frames;
use crate::transport::rtc::NativePeer;
use anyhow::{Context, Result, anyhow, bail};
pub use negotiation::{ControlIceServer, ControlSessionInfo};
use negotiation::{ForwardSettingEvent, RemoteSoac, parse_forward_setting, parse_remote_soac};
use serde_json::Value;
use std::collections::VecDeque;
use std::future::pending;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use transport::SignalTransport;
pub(crate) use transport::{SignalFailure, SocketState};
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
pub use wire::{EnginePacket, SocketPacket, decode, encode_event, encode_pong};
use wire::{encode_binary_event, gzip_sdp};

pub const AUTH_HEADER: &str = "X-NRD-AUTH";
pub const RECONNECT_HEADER: &str = "X-NRD-RECONN-KEY";
pub const CONTROLLING_HEADER: &str = "X-NRD-CONTROLLING";
const STREAMER_VERSION_HEADER: &str = "streamer_version";
const STREAMER_FLAG_HEADER: &str = "streamer_flag";
const STREAMER_VERSION: &str = crate::protocol::official_version::SDK;
const STREAMER_FLAG: &str = r#"{"sdp_flags":{"gzip_sdp":true}}"#;
const ROOM_INFO_ACK_ID: u64 = 1;
const CONTROL_ACK_ID: u64 = 2;
const ROOM_INFO_TIMEOUT: Duration = Duration::from_secs(10);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
const WEBRTC_CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
const RECONNECT_KEY_CHECK_INTERVAL: Duration = Duration::from_secs(30);
const RECONNECT_KEY_MAX_AGE: Duration = Duration::from_secs(300);
const RECONNECT_KEY_ACK_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_SDP_BYTES: u64 = 1024 * 1024;

pub(crate) fn is_loopback_host(host: &str) -> bool {
    use std::net::IpAddr;
    if let Ok(address) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        return address.is_loopback();
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "localhost" || host.ends_with(".localhost")
}

pub(crate) mod host;
mod tls;
mod transport;

pub struct SignalSession {
    self_hosted: bool,
    transport: SignalTransport,
    room_info: Value,
    reconnect_key: Option<String>,
    last_reconnect_key_refresh: Option<Instant>,
    next_refresh_ack_id: u64,
    deferred: VecDeque<EnginePacket>,
}

// Owned by the socket worker, never by a cancellable caller's read future.
struct PendingBinaryEvent {
    namespace: String,
    id: Option<u64>,
    event: String,
    args: Vec<Value>,
    expected_attachments: usize,
    attachments: Vec<Vec<u8>>,
}

#[derive(Default)]
struct NegotiationProgress {
    local_candidates_sent: usize,
    remote_candidates_installed: usize,
    remote_soac_events: usize,
    answer_installed: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum NegotiationEvent {
    OfferCreated {
        sdp_bytes: usize,
        compressed_bytes: usize,
    },
    OfferSent,
    LocalCandidateSent {
        count: usize,
    },
    AnswerInstalled {
        sdp_bytes: usize,
    },
    RemoteCandidateInstalled {
        count: usize,
    },
    Connected,
}

type NegotiationReporter<'a> = &'a (dyn Fn(NegotiationEvent) + Send + Sync);

fn report_negotiation(reporter: Option<NegotiationReporter<'_>>, event: NegotiationEvent) {
    if let Some(reporter) = reporter {
        reporter(event);
    }
}

fn handle_signal_app_data(args: &[Value], binary: &[Vec<u8>], peer: &NativePeer) -> bool {
    let Some(data) = args.first().and_then(|value| value.get("data")) else {
        return false;
    };
    if data.get("type").and_then(Value::as_str) != Some("signal_app_data") {
        return false;
    }
    let payload = data.get("signal_app_data");
    let attachment = payload
        .and_then(|value| value.get("binary_data"))
        .filter(|value| value.get("_placeholder").and_then(Value::as_bool) == Some(true))
        .and_then(|value| value.get("num"))
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .and_then(|index| binary.get(index));
    if let Some(attachment) = attachment {
        // GameViewer F95A80 filters by the current SDK session handle, not by
        // ice_id. This SignalSession owns exactly that room generation.
        if let Err(error) = peer.stream_control_handle().handle_protocol_message(
            attachment,
            crate::features::stream_control::PbMessageSource::Signal,
        ) {
            tracing::warn!(%error, bytes = attachment.len(), "invalid protobuf signal_app_data");
        }
    } else {
        tracing::warn!("signal_app_data omitted its binary attachment");
    }
    true
}

/// Tracks completion of a received switch request for diagnostics. Duplicate
/// suppression belongs to the remote sender-side monitor, not this state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingNetworkSwitch {
    attempt_switch_type: u8,
}

fn relay_protocol_is_tls(protocol: &str) -> bool {
    protocol.eq_ignore_ascii_case("tls")
}

fn selected_transport_matches_switch(
    selected: Option<(bool, String, String)>,
    attempt_switch_type: u8,
) -> bool {
    let Some((is_relay, local_protocol, remote_protocol)) = selected else {
        return false;
    };
    if !is_relay {
        return false;
    }
    match attempt_switch_type {
        2 => relay_protocol_is_tls(&local_protocol) && relay_protocol_is_tls(&remote_protocol),
        _ => true,
    }
}

fn apply_device_capability(
    capability: crate::protocol::capability::DeviceCapability,
    control: &ControlSessionInfo,
    peer: &NativePeer,
) {
    if capability.ice_id != control.ice_id {
        tracing::warn!(ice_id = %capability.ice_id, "ignored capabilities for another ICE session");
        return;
    }
    peer.stream_control_handle().set_capability(
        crate::protocol::capability::DualCapability::negotiate(
            &control.local_capability,
            capability,
        ),
    );
}

fn handle_device_capability(
    args: &[Value],
    control: &ControlSessionInfo,
    peer: &NativePeer,
) -> bool {
    match parse_forward_setting(args, control) {
        Ok(Some(ForwardSettingEvent::DeviceCapability(capability))) => {
            apply_device_capability(capability, control, peer);
            true
        }
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalRole {
    Host,
    Controller,
}

pub(crate) type SignalPushHandler = std::sync::Arc<dyn Fn(&Value) + Send + Sync>;

impl SignalSession {
    pub(crate) fn socket_state(&self) -> tokio::sync::watch::Receiver<SocketState> {
        self.transport.state_receiver()
    }

    pub async fn connect(room: RoomSession, role: SignalRole) -> Result<Self> {
        Self::connect_cancellable(room, role, &tokio_util::sync::CancellationToken::new()).await
    }

    pub(crate) async fn connect_cancellable(
        room: RoomSession,
        role: SignalRole,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Self> {
        Self::connect_observed(room, role, cancel, None).await
    }

    pub(crate) async fn connect_observed(
        room: RoomSession,
        role: SignalRole,
        cancel: &tokio_util::sync::CancellationToken,
        push: Option<SignalPushHandler>,
    ) -> Result<Self> {
        room.validate()?;
        let connect_timeout = Duration::from_millis(room.ws_connect_timeout_ms.max(100) as u64);
        let mut session = Self {
            self_hosted: room.is_self_hosted(),
            transport: SignalTransport::spawn(room, role, push),
            room_info: Value::Null,
            reconnect_key: None,
            last_reconnect_key_refresh: None,
            next_refresh_ack_id: 3,
            deferred: VecDeque::new(),
        };
        let result = tokio::select! {
            _ = cancel.cancelled() => Err(anyhow!("signaling connection cancelled")),
            result = async {
            timeout(connect_timeout, session.wait_namespace())
                .await.map_err(|_| SignalFailure::Socket("initial connection deadline expired (2009)".into()))??;
            timeout(ROOM_INFO_TIMEOUT, session.login_room())
                .await.map_err(|_| SignalFailure::Socket("room_info ACK timed out (2012)".into()))??;
            Ok(())
            } => result,
        };
        if let Err(error) = result {
            let _ = session.graceful_close().await;
            return Err(error);
        }
        tracing::debug!(?role, "signaling room login complete");
        Ok(session)
    }

    async fn wait_namespace(&mut self) -> Result<()> {
        loop {
            match self.transport.next_packet().await? {
                EnginePacket::Message(SocketPacket::Connect { namespace, .. })
                    if namespace == "/" =>
                {
                    return Ok(());
                }
                packet => self.deferred.push_back(packet),
            }
        }
    }

    async fn login_room(&mut self) -> Result<()> {
        self.send_text(encode_event("room_info", &[], Some(ROOM_INFO_ACK_ID))?)
            .await?;
        loop {
            match self.transport.next_packet().await? {
                EnginePacket::Message(SocketPacket::Ack { id, args, .. })
                    if id == ROOM_INFO_ACK_ID =>
                {
                    let info = args
                        .first()
                        .and_then(Value::as_object)
                        .filter(|info| !info.is_empty())
                        .ok_or_else(|| {
                            SignalFailure::Socket("room_info ACK is empty or invalid (2011)".into())
                        })?;
                    self.room_info = Value::Object(info.clone());
                    return Ok(());
                }
                packet => self.deferred.push_back(packet),
            }
        }
    }

    pub(crate) async fn start_control(
        &mut self,
        controller_device_id: &str,
        profile: ConnectionMediaProfile,
        connect_type: crate::session::negotiation::ControlConnectType,
        preferences: Option<crate::features::stream_control::StreamControlPreferences>,
        purpose: crate::session::negotiation::ControlPurpose,
        entitlements: Option<&crate::protocol::entitlements::Snapshot>,
    ) -> Result<ControlSessionInfo> {
        tracing::debug!(?profile, "starting control handshake");
        let decoder_support =
            crate::lifecycle::blocking(move |stop| detect_native_decoder_support(profile, stop))
                .await
                .context("native capability probe task interrupted")??;
        tracing::info!(
            ?decoder_support,
            "advertising native decoder configuration capabilities"
        );
        let frames = build_control_frames(
            controller_device_id,
            CONTROL_ACK_ID,
            &decoder_support,
            profile,
            connect_type,
            preferences,
            purpose,
            entitlements,
        )?;
        let app_control_id = frames.app_control_id;
        self.send_binary_packet(frames.header, frames.attachment)?;

        let mut info = timeout(CONTROL_TIMEOUT, self.wait_control_ack())
            .await
            .context("control handshake timed out")??;
        info.app_control_id = app_control_id;
        info.local_capability = decoder_support;
        tracing::debug!(
            ice_server_count = info.ice_servers.len(),
            force_relay = info.force_relay,
            auto_switch_network = info.auto_switch_network,
            possible_auto_switch_pkt_loss = info.possible_auto_switch_pkt_loss,
            possible_auto_switch_latency = info.possible_auto_switch_latency,
            possible_auto_switch_min_latency = info.possible_auto_switch_min_latency,
            force_auto_switch_pkt_loss = info.force_auto_switch_pkt_loss,
            force_auto_switch_latency = info.force_auto_switch_latency,
            "control handshake accepted"
        );
        Ok(info)
    }

    pub(crate) async fn negotiate(
        &mut self,
        info: &ControlSessionInfo,
        peer: &NativePeer,
        reporter: Option<NegotiationReporter<'_>>,
    ) -> Result<()> {
        let mut local_candidates = peer.local_ice_candidates();
        let mut progress = NegotiationProgress::default();
        let offer = peer.create_offer().await?;
        tracing::trace!(sdp_bytes = offer.len(), "local WebRTC offer");
        let attachment = gzip_sdp(&offer)?;
        tracing::debug!(
            sdp_bytes = offer.len(),
            gzip_bytes = attachment.len(),
            "sending WebRTC offer"
        );
        report_negotiation(
            reporter,
            NegotiationEvent::OfferCreated {
                sdp_bytes: offer.len(),
                compressed_bytes: attachment.len(),
            },
        );
        let payload = serde_json::json!({
            "client_id": info.client_id,
            "data": {
                "app_control_id": info.app_control_id,
                "gzip_sdp": { "_placeholder": true, "num": 0 },
                "ice_id": info.ice_id,
                "ice_network_type": 3,
                "sdp": "",
                "type": "offer"
            }
        });
        let header = encode_binary_event("soac", &[payload], None, 1)?;
        self.send_binary_packet(header, attachment)?;
        report_negotiation(reporter, NegotiationEvent::OfferSent);

        match timeout(
            WEBRTC_CONNECT_TIMEOUT,
            self.exchange_ice(info, peer, &mut local_candidates, &mut progress, reporter),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                let ice = peer.ice_diagnostics().await;
                tracing::error!(
                    peer_state = %peer.connection_state(),
                    ice_state = %peer.ice_connection_state(),
                    local_candidates = progress.local_candidates_sent,
                    remote_soac_events = progress.remote_soac_events,
                    answer_installed = progress.answer_installed,
                    diagnostics = %ice,
                    "WebRTC connection timed out"
                );
                Err(SignalFailure::Peer(format!(
                    "WebRTC did not connect within the official 12-second window (peer: {}, ICE: {}, local candidates sent: {}, remote soac events: {}, answer installed: {}; {})",
                    peer.connection_state(),
                    peer.ice_connection_state(),
                    progress.local_candidates_sent,
                    progress.remote_soac_events,
                    progress.answer_installed,
                    ice
                )).into())
            }
        }
    }

    async fn restart_ice(
        &mut self,
        info: &ControlSessionInfo,
        peer: &NativePeer,
        ice_network_type: u8,
    ) -> Result<()> {
        let offer = peer.create_restart_offer().await?;
        let attachment = gzip_sdp(&offer)?;
        let payload = serde_json::json!({
            "client_id": info.client_id,
            "data": {
                "app_control_id": info.app_control_id,
                "gzip_sdp": { "_placeholder": true, "num": 0 },
                "ice_id": info.ice_id,
                "ice_network_type": ice_network_type,
                "sdp": "",
                "type": "restart_ice"
            }
        });
        self.send_binary_packet(
            encode_binary_event("soac", &[payload], None, 1)?,
            attachment,
        )?;
        tracing::info!(
            "ICE restart offer sent; the persistent SOAC loop will install the answer and candidates while the old selected path remains active"
        );
        Ok(())
    }

    async fn apply_official_network_switch(
        &mut self,
        info: &ControlSessionInfo,
        peer: &NativePeer,
        transport_type: u8,
        attempt_switch_type: u8,
    ) -> Result<()> {
        peer.switch_ice_network(transport_type, attempt_switch_type)
            .await?;
        self.restart_ice(info, peer, transport_type).await
    }

    async fn send_soac_candidate(
        &mut self,
        info: &ControlSessionInfo,
        candidate: &RTCIceCandidateInit,
        relay_only: bool,
    ) -> Result<()> {
        if candidate.candidate.is_empty() {
            return Ok(());
        }
        if relay_only
            && !candidate
                .candidate
                .to_ascii_lowercase()
                .contains(" typ relay")
        {
            return Ok(());
        }
        tracing::trace!(candidate = ?candidate, "sending local ICE candidate");
        let sdp_mid = candidate
            .sdp_mid
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("0");
        let payload = serde_json::json!({
            "client_id": info.client_id,
            "data": {
                "app_control_id": info.app_control_id,
                "candidate": {
                    "candidate": candidate.candidate,
                    "sdpMid": sdp_mid,
                    "sdpMLineIndex": candidate.sdp_mline_index,
                    "usernameFragment": candidate.username_fragment
                },
                "ice_id": info.ice_id,
                "ice_network_type": 0,
                "type": "candidate"
            }
        });
        self.send_text(encode_event("soac", &[payload], None)?)
            .await
    }

    async fn exchange_ice(
        &mut self,
        info: &ControlSessionInfo,
        peer: &NativePeer,
        local_candidates: &mut tokio::sync::broadcast::Receiver<Option<RTCIceCandidateInit>>,
        progress: &mut NegotiationProgress,
        reporter: Option<NegotiationReporter<'_>>,
    ) -> Result<()> {
        use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

        let mut local_gathering_complete = false;
        let mut answer_installed = false;
        let mut pending_events = VecDeque::new();

        loop {
            if peer.connection_state() == RTCPeerConnectionState::Connected {
                self.deferred.append(&mut pending_events);
                self.transport.set_controlling(true);
                report_negotiation(reporter, NegotiationEvent::Connected);
                return Ok(());
            }
            tokio::select! {
                candidate = local_candidates.recv(), if !local_gathering_complete => {
                    match candidate {
                        Ok(Some(candidate)) => {
                            self.send_soac_candidate(info, &candidate, false).await?;
                            progress.local_candidates_sent += 1;
                            report_negotiation(
                                reporter,
                                NegotiationEvent::LocalCandidateSent {
                                    count: progress.local_candidates_sent,
                                },
                            );
                        }
                        Ok(None) => local_gathering_complete = true,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "local ICE candidate broadcast lagged during negotiation");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            local_gathering_complete = true;
                        }
                    }
                },
                packet = self.next_packet() => match packet? {
                    EnginePacket::Message(SocketPacket::Event { namespace, id, event, args, binary })
                        if event == "forward_setting" =>
                    {
                        if !handle_signal_app_data(&args, &binary, peer)
                            && !handle_device_capability(&args, info, peer) {
                            pending_events.push_back(EnginePacket::Message(SocketPacket::Event {
                                namespace, id, event, args, binary,
                            }));
                        }
                    }
                    EnginePacket::Message(SocketPacket::Event { event, args, .. })
                        if event == "soac" =>
                    {
                        progress.remote_soac_events += 1;
                        tracing::trace!(argument_count = args.len(), "received remote SOAC event");
                        let soac = match parse_remote_soac(&args, Some(info)) {
                            Ok(soac) => soac,
                            Err(error) => {
                                tracing::warn!(%error, "discarded invalid SOAC event during negotiation");
                                continue;
                            }
                        };
                        match soac {
                            Some(RemoteSoac::Answer { sdp, restart_ice }) => {
                                tracing::debug!(restart_ice, sdp_bytes = sdp.len(), "installing remote WebRTC answer");
                                tracing::trace!(sdp_bytes = sdp.len(), "remote WebRTC answer");
                                let sdp_bytes = sdp.len();
                                if let Err(error) = peer.set_remote_answer(sdp, restart_ice).await {
                                    tracing::warn!(%error, "remote answer rejected; awaiting a valid answer or peer deadline");
                                    continue;
                                }
                                answer_installed = true;
                                progress.answer_installed = true;
                                report_negotiation(
                                    reporter,
                                    NegotiationEvent::AnswerInstalled { sdp_bytes },
                                );
                            }
                            Some(RemoteSoac::Candidate(candidate)) if answer_installed => {
                                tracing::trace!(candidate = ?candidate, "installing remote ICE candidate");
                                if let Err(error) = peer.add_remote_candidate(candidate).await {
                                    tracing::warn!(%error, "discarded rejected remote ICE candidate");
                                    continue;
                                }
                                progress.remote_candidates_installed += 1;
                                report_negotiation(
                                    reporter,
                                    NegotiationEvent::RemoteCandidateInstalled {
                                        count: progress.remote_candidates_installed,
                                    },
                                );
                            }
                            Some(RemoteSoac::Candidate(_)) => {
                                tracing::debug!("discarded remote ICE candidate before initial answer");
                            }
                            None => {}
                        }
                    }
                    EnginePacket::Ping(payload) => self.send_text(encode_pong(&payload)).await?,
                    EnginePacket::Close | EnginePacket::Message(SocketPacket::Disconnect { .. }) => {
                        bail!("signaling server closed during WebRTC negotiation")
                    }
                    EnginePacket::Message(SocketPacket::ConnectError { .. }) => {
                        bail!("signaling server rejected WebRTC negotiation")
                    }
                    _ => {}
                },
                state = peer.next_connection_state() => match state {
                    Some(RTCPeerConnectionState::Connected) => {
                        self.deferred.append(&mut pending_events);
                        self.transport.set_controlling(true);
                        report_negotiation(reporter, NegotiationEvent::Connected);
                        return Ok(())
                    },
                    Some(RTCPeerConnectionState::Failed) => {
                        return Err(SignalFailure::Peer("native WebRTC peer failed during negotiation".into()).into());
                    }
                    Some(RTCPeerConnectionState::Closed) => {
                        return Err(SignalFailure::Peer("native WebRTC peer closed during negotiation".into()).into());
                    }
                    None => bail!("native WebRTC connection state stream closed"),
                    Some(_) => {}
                }
            }
        }
    }

    async fn wait_control_ack(&mut self) -> Result<ControlSessionInfo> {
        loop {
            match self.transport.next_packet().await? {
                EnginePacket::Message(SocketPacket::Ack { id, args, .. })
                    if id == CONTROL_ACK_ID =>
                {
                    if args.len() < 2 || args[0].as_str() != Some("success") {
                        bail!("remote device rejected the control handshake");
                    }
                    let info: ControlSessionInfo = serde_json::from_value(args[1].clone())
                        .context("control ACK has an unexpected payload")?;
                    if info.code != 0
                        || info.client_id.is_empty()
                        || info.ice_id.is_empty()
                        || (!self.self_hosted && info.ice_servers.is_empty())
                    {
                        bail!("control ACK did not contain a usable session configuration");
                    }
                    return Ok(info);
                }
                EnginePacket::Message(SocketPacket::ConnectError { .. }) => {
                    bail!("Socket.IO rejected the control handshake")
                }
                EnginePacket::Ping(payload) => self.send_text(encode_pong(&payload)).await?,
                packet => self.deferred.push_back(packet),
            }
        }
    }

    pub async fn keep_alive(
        self,
        shutdown: oneshot::Receiver<()>,
        peer: Option<std::sync::Arc<NativePeer>>,
        control: Option<ControlSessionInfo>,
    ) -> Result<()> {
        self.keep_alive_until(
            async {
                let _ = shutdown.await;
                Ok(())
            },
            peer,
            control,
        )
        .await
    }

    pub(crate) async fn keep_alive_until(
        self,
        shutdown: impl Future<Output = Result<()>>,
        peer: Option<std::sync::Arc<NativePeer>>,
        control: Option<ControlSessionInfo>,
    ) -> Result<()> {
        self.keep_alive_inner(shutdown, peer, control, None).await
    }

    pub(crate) async fn keep_alive_host(
        self,
        shutdown: oneshot::Receiver<()>,
        client: crate::session::host_client::HostClient,
    ) -> Result<()> {
        self.keep_alive_inner(
            async {
                let _ = shutdown.await;
                Ok(())
            },
            None,
            None,
            Some(host::Session::new(client)),
        )
        .await
    }

    async fn keep_alive_inner(
        mut self,
        shutdown: impl Future<Output = Result<()>>,
        peer: Option<std::sync::Arc<NativePeer>>,
        control: Option<ControlSessionInfo>,
        mut host: Option<host::Session>,
    ) -> Result<()> {
        let mut next_key_check = tokio::time::Instant::now() + RECONNECT_KEY_CHECK_INTERVAL;
        let mut pending_key_refresh: Option<(u64, tokio::time::Instant)> = None;
        let mut local_candidates = peer.as_ref().map(|peer| peer.local_ice_candidates());
        let mut local_candidate_stream_open = local_candidates.is_some();
        let mut active_ice_network_type = 3_u8;
        let mut pending_network_switch = None::<PendingNetworkSwitch>;
        let network_control = peer
            .as_ref()
            .map(|peer| peer.stream_control_handle().network_control());
        let mut manual_network_requests =
            network_control.as_ref().map(|control| control.subscribe());
        let mut network_switch_check = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(1),
            Duration::from_secs(1),
        );
        network_switch_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let signal_result = tokio::select! {
            biased;
            result = shutdown => result,
            result = async {
            if peer.is_some() != control.is_some() { bail!("controller signaling lifecycle is incomplete"); }
            loop {
            tokio::select! {
                event=async {host.as_mut().expect("host session").next().await},if host.is_some()=>{
                    host.as_mut().expect("host session").apply(event,&mut self).await?;
                }
                changed = async {
                    if let Some(requests) = &mut manual_network_requests { requests.changed().await }
                    else { pending().await }
                }, if manual_network_requests.is_some() => {
                    if changed.is_err() { manual_network_requests = None; continue; }
                    let desired = *manual_network_requests.as_mut().expect("manual request channel").borrow_and_update();
                    let Some(relay) = desired else { continue; };
                    let peer = peer.as_ref().expect("manual switch peer");
                    let info = control.as_ref().expect("manual switch session");
                    let network = network_control.as_ref().expect("manual switch state");
                    if !peer.can_switch_ice_network() || !self.transport.is_connected() {
                        network.fail("当前连接尚未就绪，无法切换线路");
                        continue;
                    }
                    if info.force_relay && !relay {
                        network.fail("本次连接要求使用中转");
                        continue;
                    }
                    // Official manual ABI: enabled -> kRelay/attempt 2 (TURNS),
                    // disabled -> kAll/attempt 3. Existing SOAC owns the answer.
                    let (transport_type, attempt) = if relay { (1, 2) } else { (3, 3) };
                    match self.apply_official_network_switch(info, peer, transport_type, attempt).await {
                        Ok(()) => {
                            peer.accept_manual_network_policy();
                            active_ice_network_type = transport_type;
                            pending_network_switch = None;
                            network.submitted(relay);
                            tracing::info!(relay, "manual network policy submitted");
                        }
                        Err(error) => {
                            tracing::warn!(%error, relay, "manual network switch failed; retaining current path");
                            network.fail("线路切换未完成，可稍后重试");
                        }
                    }
                }
                _ = async {
                    if let Some((_, deadline)) = pending_key_refresh {
                        tokio::time::sleep_until(deadline).await;
                    } else { pending::<()>().await; }
                } => {
                    tracing::warn!("refresh_reconnect_key ACK timed out; keeping the current signaling socket");
                    pending_key_refresh = None;
                    next_key_check = tokio::time::Instant::now() + RECONNECT_KEY_CHECK_INTERVAL;
                }
                _ = tokio::time::sleep_until(next_key_check), if pending_key_refresh.is_none() => {
                    // UU's loop starts with an expired refresh age, checks
                    // after 30s, and waits another 30s after each operation
                    // completes. Header receipt is not a successful refresh.
                    if self.transport.is_connected() && self.last_reconnect_key_refresh.is_none_or(|last| last.elapsed() > RECONNECT_KEY_MAX_AGE) {
                        let ack_id = self.next_refresh_ack_id;
                        self.next_refresh_ack_id = ack_id.wrapping_add(1).max(3);
                        let refresh = encode_event(
                            "refresh_reconnect_key",
                            &[],
                            Some(ack_id),
                        )?;
                        if let Err(error) = self.send_text(refresh).await {
                            tracing::warn!(%error, "refresh_reconnect_key request failed; waiting for the socket state machine");
                        } else {
                            pending_key_refresh = Some((ack_id, tokio::time::Instant::now() + RECONNECT_KEY_ACK_TIMEOUT));
                            tracing::debug!(ack_id, "requested signaling reconnect key refresh");
                        }
                    }
                    next_key_check = tokio::time::Instant::now() + RECONNECT_KEY_CHECK_INTERVAL;
                }
                candidate = async {
                    local_candidates
                        .as_mut()
                        .expect("controller candidate stream exists")
                        .recv()
                        .await
                }, if local_candidate_stream_open => {
                    match candidate {
                        Ok(Some(candidate)) => {
                            let info = control.as_ref().expect("controller control info exists");
                            self.send_soac_candidate(
                                info,
                                &candidate,
                                active_ice_network_type == 1,
                            )
                            .await?;
                            tracing::debug!("sent continual-gathering local ICE candidate");
                        }
                        Ok(None) => {
                            tracing::debug!("local ICE candidate generation completed");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "continual ICE candidate broadcast lagged");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            local_candidate_stream_open = false;
                        }
                    }
                }
                packet = self.next_packet() => {
                    match packet {
                        Err(error) => break Err(error),
                        Ok(EnginePacket::Message(SocketPacket::Connect { .. })) if host.is_some()=>{
                            host.as_mut().expect("host session").signaling_restored(&mut self).await?;
                        }
                        Ok(EnginePacket::Message(SocketPacket::Event {event,args,binary,..})) if host.is_some()=>{
                            if let Err(error)=host.as_mut().expect("host session").event(&event,&args,&binary,&mut self).await {
                                tracing::warn!(%error,"host signaling event rejected");
                            }
                        }
                        Ok(EnginePacket::Message(SocketPacket::Ack { id, args, .. }))
                            if pending_key_refresh.is_some_and(|(pending_id, _)| id == pending_id) =>
                        {
                            if args.len() < 2 || args[0].as_str() != Some("success") {
                                tracing::warn!("refresh_reconnect_key ACK was rejected");
                            } else {
                                if let Some(value) = args[1].get("reconnect_key") {
                                    self.reconnect_key = value.as_str().filter(|key| !key.is_empty()).map(str::to_owned);
                                }
                                // Official success may omit the field when
                                // an existing non-empty key remains valid.
                                if self.reconnect_key.is_some() {
                                    self.transport.set_reconnect_key(self.reconnect_key.clone());
                                    self.last_reconnect_key_refresh = Some(Instant::now());
                                    tracing::debug!(ack_id = id, "signaling reconnect key refreshed");
                                } else {
                                    tracing::warn!(ack_id = id, "refresh_reconnect_key ACK left an empty key");
                                }
                            }
                            pending_key_refresh = None;
                            next_key_check = tokio::time::Instant::now() + RECONNECT_KEY_CHECK_INTERVAL;
                        }
                        Ok(EnginePacket::Message(SocketPacket::Event { event, args, .. }))
                            if event == "soac" =>
                        {
                            if let Some(peer) = peer.as_ref() {
                                let soac = match parse_remote_soac(&args, control.as_ref()) {
                                    Ok(soac) => soac,
                                    Err(error) => {
                                        tracing::warn!(%error, "discarded invalid SOAC event");
                                        continue;
                                    }
                                };
                                match soac {
                                    Some(RemoteSoac::Candidate(candidate)) => {
                                        if let Err(error) = peer.add_remote_candidate(candidate).await {
                                            tracing::warn!(%error, "discarded rejected remote ICE candidate; session retained");
                                        }
                                    }
                                    Some(RemoteSoac::Answer { sdp, restart_ice }) => {
                                        if let Err(error) = peer.set_remote_answer(sdp, restart_ice).await {
                                            tracing::warn!(%error, "remote answer rejected; session retained");
                                        }
                                    }
                                    None => {}
                                }
                            }
                        }
                        Ok(EnginePacket::Message(SocketPacket::Event { event, args, binary, .. }))
                            if event == "forward_setting" =>
                        {
                            let Some(peer) = peer.as_ref() else { continue; };
                            if handle_signal_app_data(&args, &binary, peer) { continue; }
                            let control = control.as_ref().expect("controller control info exists");
                            let parsed = match parse_forward_setting(&args, control) {
                                Ok(parsed) => parsed,
                                Err(error) => {
                                    tracing::warn!(%error, "discarded invalid forward_setting event");
                                    continue;
                                }
                            };
                            match parsed {
                                Some(ForwardSettingEvent::DeviceCapability(capability)) => {
                                    apply_device_capability(capability, control, peer);
                                }
                                Some(ForwardSettingEvent::SenderParaInfo { ice_id, infos }) => {
                                    tracing::debug!(%ice_id, ?infos, "official sender_para_info received");
                                    peer.performance_monitor().set_remote_senders(infos);
                                }
                                Some(ForwardSettingEvent::SwitchNetwork {
                                    transport_type,
                                    ice_id,
                                    attempt_switch_type,
                                }) => {
                                    if !peer.allows_network_switch() {
                                        tracing::debug!("ignored switch_network_notify because the user selected P2P-only transport");
                                        continue;
                                    }
                                    if ice_id != control.ice_id {
                                        tracing::warn!(%ice_id, "ignored switch_network_notify for another ICE session");
                                        continue;
                                    }
                                    if transport_type == 0 {
                                        tracing::warn!(transport_type, attempt_switch_type, "ignored invalid switch_network_notify");
                                        continue;
                                    }
                                    if !peer.can_switch_ice_network() {
                                        tracing::debug!(
                                            ice_state = %peer.ice_connection_state(),
                                            peer_state = %peer.connection_state(),
                                            "ignored switch_network_notify until the current ICE connection is established"
                                        );
                                        continue;
                                    }
                                    if let Err(error) = self
                                        .apply_official_network_switch(
                                            control,
                                            peer,
                                            transport_type,
                                            attempt_switch_type,
                                        )
                                        .await
                                    {
                                        tracing::warn!(
                                            %error,
                                            transport_type,
                                            attempt_switch_type,
                                            "official ICE network switch attempt failed; keeping the old selected path"
                                        );
                                        pending_network_switch = None;
                                        continue;
                                    }
                                    peer.performance_monitor()
                                        .record_network_switch_attempt(attempt_switch_type);
                                    active_ice_network_type = transport_type;
                                    pending_network_switch = Some(PendingNetworkSwitch {
                                        attempt_switch_type,
                                    });
                                }
                                None => {}
                            }
                        }
                        Ok(EnginePacket::Ping(payload)) => {
                            self.send_text(encode_pong(&payload)).await?;
                        }
                        Ok(EnginePacket::Close)
                        | Ok(EnginePacket::Message(SocketPacket::Disconnect { .. })) => {
                            break Err(SignalFailure::NamespaceClosed.into());
                        }
                        Ok(EnginePacket::Message(SocketPacket::ConnectError { .. })) => {
                            break Err(anyhow!("signaling server rejected the room session"));
                        }
                        _ => {}
                    }
                }
                _ = network_switch_check.tick(), if peer.is_some() => {
                    let peer = peer.as_ref().expect("controller peer exists");
                    let selected = peer.selected_transport().await;
                    if let Some(network) = &network_control {
                        network.connected(peer.can_switch_ice_network());
                        network.observe_route(selected.as_ref().is_some_and(|s| s.0));
                    }
                    let Some(pending) = pending_network_switch else { continue; };
                    if selected_transport_matches_switch(
                        selected,
                        pending.attempt_switch_type,
                    ) {
                        tracing::info!(
                            attempt_switch_type = pending.attempt_switch_type,
                            "official ICE network switch converged on the requested relay path"
                        );
                        peer.performance_monitor()
                            .record_network_switch_success(pending.attempt_switch_type);
                        pending_network_switch = None;
                    }
                }
                state = async {
                    match peer.as_ref() {
                        Some(peer) => peer.next_connection_state().await,
                        None => pending().await,
                    }
                } => {
                    use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
                    match state {
                        Some(RTCPeerConnectionState::Connected) => self.transport.set_controlling(true),
                        Some(RTCPeerConnectionState::Disconnected) => {
                            pending_network_switch = None;
                            tracing::warn!(
                                "native WebRTC peer connection temporarily disconnected; preserving the current ICE session"
                            );
                        }
                        Some(RTCPeerConnectionState::Failed) => {
                            break Err(SignalFailure::Peer("native WebRTC ICE connection failed (3021)".into()).into());
                        }
                        Some(RTCPeerConnectionState::Closed) => {
                            self.transport.set_controlling(false);
                            break Err(anyhow!("native WebRTC peer connection closed"));
                        }
                        None if peer.is_some() => {
                            break Err(anyhow!("native WebRTC state stream ended"));
                        }
                        _ => {}
                    }
                }
            }
        }} => result,
        };
        if let Some(network) = &network_control {
            network.close();
        }
        if let Some(host) = host {
            host.close().await;
        }
        if let Some(peer) = &peer {
            // A signaling-only failure can leave DTLS writable. Retire input
            // before waiting for WebSocket close so no new clicks are accepted.
            peer.stream_control_handle().mouse().close().await;
        }
        self.transport.set_controlling(false);
        let socket_result = self.graceful_close().await;
        let peer_result = match peer {
            Some(peer) => peer.close().await,
            None => Ok(()),
        };
        signal_result.and(socket_result).and(peer_result)
    }

    pub(crate) async fn close(mut self) -> Result<()> {
        self.graceful_close().await
    }

    async fn next_packet(&mut self) -> Result<EnginePacket> {
        if let Some(packet) = self.deferred.pop_front() {
            return Ok(packet);
        }
        self.transport.next_packet().await
    }

    async fn send_text(&mut self, value: impl Into<String>) -> Result<()> {
        self.transport
            .send(vec![Message::Text(value.into().into())])
    }

    fn send_binary_packet(&mut self, header: String, attachment: Vec<u8>) -> Result<()> {
        let packet = if self.self_hosted {
            attachment
        } else {
            let mut packet = Vec::with_capacity(attachment.len() + 1);
            packet.push(0x04);
            packet.extend_from_slice(&attachment);
            packet
        };
        self.transport.send(vec![
            Message::Text(header.into()),
            Message::Binary(packet.into()),
        ])
    }

    async fn graceful_close(&mut self) -> Result<()> {
        self.transport.close().await
    }
}

mod negotiation;
mod wire;
