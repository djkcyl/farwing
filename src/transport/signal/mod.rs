//! Farwing realtime plane client (Farwing Protocol v2, WSS).
//!
//! A [`SignalSession`] is one realtime connection: either a host's presence
//! connection or a controller's session connection. A controller session of a
//! device whose process already has a presence connection rides on that
//! connection instead of opening its own (`docs/protocol/v2-design.md` §11).
//! Network I/O lives in the transport worker; this module owns session
//! negotiation (SDP/ICE relay, session settings) and the long-running
//! keep-alive loops.

use crate::account::api::RealtimeTicket;
use crate::account::server::ServerConfig;
use crate::media::ConnectionMediaProfile;
use crate::media::decoder::detect_native_decoder_support;
use crate::session::negotiation::build_session_start;
use crate::transport::rtc::NativePeer;
use anyhow::{Context, Result, anyhow, bail};
use farwing_protocol::realtime::{
    Envelope, Hello, RtcCandidate, RtcDescription, SessionStarted, network, types,
};
pub use negotiation::ControlSessionInfo;
use negotiation::{RemoteRtc, SettingEvent, parse_remote_rtc, parse_setting};
use std::collections::VecDeque;
use std::future::pending;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::timeout;
use transport::{Carrier, Incoming, SignalTransport};
pub(crate) use transport::{SignalFailure, SocketState};
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;

const CONTROL_TIMEOUT: Duration =
    Duration::from_millis(farwing_protocol::timing::REQUEST_TIMEOUT_MS as u64);
/// The host answers once its screen and encoder are ready; a host that
/// must check its encoders again (a new display or resolution) takes
/// 10–15 s before answering.
const WEBRTC_ANSWER_TIMEOUT: Duration = Duration::from_secs(40);
/// After the answer, the connection itself.
const WEBRTC_CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
pub(crate) mod host;
pub(crate) mod identity;
pub(crate) mod negotiation;
mod node_probe;
pub(crate) mod router;
mod transport;

pub struct SignalSession {
    transport: SignalTransport,
    hello: Hello,
    deferred: VecDeque<Incoming>,
    next_request: u64,
    /// Controller: generation of the latest offer (initial 1, +1 per restart).
    generation: u64,
    /// Host: direct participants sharing this signalling loop (§12).
    router: Option<std::sync::Arc<router::Router>>,
    /// Controller on a direct connection: the address it dialed. The host's
    /// UDP host-candidate ports are also tried at this address, which reaches
    /// a host behind a router that forwards them (§12.3).
    dialed: Option<std::net::IpAddr>,
    derived: std::collections::HashSet<(std::net::IpAddr, u16)>,
    /// Request IDs are unique on the socket: sessions carried by one
    /// presence connection each have their own prefix.
    request_prefix: String,
    /// Controller on a direct connection: its visit, told to the host in
    /// `session.start` (the server learns it when the session is created).
    visit: String,
}

/// The presence connection of this process that controller sessions of the
/// same device and account may ride on.
struct PresenceCarrier {
    device_id: String,
    user_id: String,
    carrier: Carrier,
    generation: u64,
}

static CARRIER: std::sync::Mutex<Option<PresenceCarrier>> = std::sync::Mutex::new(None);
static CARRIER_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn carrier_slot() -> std::sync::MutexGuard<'static, Option<PresenceCarrier>> {
    CARRIER.lock().unwrap_or_else(|e| e.into_inner())
}

/// Keeps a presence connection offered to controller sessions; withdraws it
/// when dropped.
pub(crate) struct CarrierRegistration(u64);

impl Drop for CarrierRegistration {
    fn drop(&mut self) {
        let mut slot = carrier_slot();
        if slot.as_ref().is_some_and(|c| c.generation == self.0) {
            *slot = None;
        }
    }
}

/// `FARWING_SIGNAL_PRESENCE=off` makes every controller session open its own
/// realtime connection (diagnostics and A/B timing).
fn presence_reuse_enabled() -> bool {
    !std::env::var("FARWING_SIGNAL_PRESENCE").is_ok_and(|value| value.eq_ignore_ascii_case("off"))
}

/// Whether a controller session of `device_id` / `user_id` can be signalled
/// over this process's presence connection right now.
pub(crate) fn presence_available(device_id: &str, user_id: &str) -> bool {
    presence_reuse_enabled()
        && carrier_slot().as_ref().is_some_and(|c| {
            c.device_id == device_id && c.user_id == user_id && c.carrier.is_connected()
        })
}

/// Wait up to `limit` for a presence connection to become usable (a
/// standalone viewer starts its own at the same time as its first session).
pub(crate) async fn wait_presence(device_id: &str, user_id: &str, limit: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if presence_available(device_id, user_id) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[derive(Default)]
struct NegotiationProgress {
    local_candidates_sent: usize,
    remote_candidates_installed: usize,
    remote_rtc_events: usize,
    answer_installed: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum NegotiationEvent {
    OfferSent,
    LocalCandidateSent { count: usize },
    AnswerInstalled,
    RemoteCandidateInstalled { count: usize },
    Connected,
}

type NegotiationReporter<'a> = &'a (dyn Fn(NegotiationEvent) + Send + Sync);

fn report_negotiation(reporter: Option<NegotiationReporter<'_>>, event: NegotiationEvent) {
    if let Some(reporter) = reporter {
        reporter(event);
    }
}

fn apply_device_capability(
    capability: crate::protocol::capability::DeviceCapability,
    control: &ControlSessionInfo,
    peer: &NativePeer,
) {
    if capability.session_id != control.session_id {
        tracing::warn!("ignored capabilities for another session");
        return;
    }
    peer.stream_control_handle().set_capability(
        crate::protocol::capability::DualCapability::negotiate(
            &control.local_capability,
            capability,
        ),
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalRole {
    Host,
    Controller,
}

/// Receives host-bound server pushes (device changes, assistance, power, WoL).
/// Called on the socket worker: implementations must only queue work.
pub(crate) type SignalPushHandler = std::sync::Arc<dyn Fn(&Envelope) + Send + Sync>;

impl SignalSession {
    pub(crate) fn socket_state(&self) -> tokio::sync::watch::Receiver<SocketState> {
        self.transport.state_receiver()
    }

    pub async fn connect(ticket: RealtimeTicket, role: SignalRole) -> Result<Self> {
        Self::connect_cancellable(ticket, role, &tokio_util::sync::CancellationToken::new()).await
    }

    pub(crate) async fn connect_cancellable(
        ticket: RealtimeTicket,
        role: SignalRole,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Self> {
        Self::connect_observed(ticket, role, cancel, None).await
    }

    pub(crate) async fn connect_observed(
        ticket: RealtimeTicket,
        role: SignalRole,
        cancel: &tokio_util::sync::CancellationToken,
        push: Option<SignalPushHandler>,
    ) -> Result<Self> {
        ticket.validate()?;
        let mut server = ServerConfig::require()?;
        if let Some(moved) = crate::account::server_changed::follow_endpoint(&server, &ticket).await {
            server = moved;
        }
        let connect_timeout = Duration::from_millis(u64::from(ticket.connect_timeout_ms.max(100)));
        let mut transport = SignalTransport::spawn(server, ticket, role, push);
        let mut deferred = VecDeque::new();
        let result = tokio::select! {
            _ = cancel.cancelled() => Err(anyhow!("signaling connection cancelled")),
            result = timeout(connect_timeout, async {
                loop {
                    match transport.next().await? {
                        Incoming::Opened(hello) => return Ok::<_, anyhow::Error>(*hello),
                        message => deferred.push_back(message),
                    }
                }
            }) => result.map_err(|_| SignalFailure::Socket("initial connection deadline expired".into()).into()).and_then(|r| r),
        };
        let hello = match result {
            Ok(hello) => hello,
            Err(error) => {
                let _ = transport.close().await;
                return Err(error);
            }
        };
        tracing::debug!(?role, ?hello, "realtime connection established");
        // The presence connection is up at start-up: discover this network's
        // NAT behaviour in the background before the first session needs it.
        crate::transport::nat::start();
        Ok(Self::with_transport(transport, hello))
    }

    fn with_transport(transport: SignalTransport, hello: Hello) -> Self {
        Self {
            transport,
            hello,
            deferred: VecDeque::new(),
            next_request: 1,
            generation: 0,
            router: None,
            dialed: None,
            derived: Default::default(),
            request_prefix: "c-".to_owned(),
            visit: String::new(),
        }
    }

    /// The visit this controller's session belongs to (direct connections).
    pub(crate) fn set_visit(&mut self, visit: &str) {
        visit.clone_into(&mut self.visit);
    }

    /// Offer this presence connection to controller sessions of the same
    /// device and account in this process (`docs/protocol/v2-design.md` §11).
    pub(crate) fn carry_controller_sessions(
        &self,
        device_id: &str,
        user_id: &str,
    ) -> Option<CarrierRegistration> {
        let carrier = self.transport.carrier()?;
        let generation = CARRIER_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        *carrier_slot() = Some(PresenceCarrier {
            device_id: device_id.to_owned(),
            user_id: user_id.to_owned(),
            carrier,
            generation,
        });
        tracing::debug!("presence connection offered to controller sessions");
        Some(CarrierRegistration(generation))
    }

    /// Controller session `session_id` of `device_id`, signalled over this
    /// process's presence connection.
    pub(crate) fn carried(device_id: &str, session_id: &str) -> Result<Self> {
        let carrier = carrier_slot()
            .as_ref()
            .filter(|c| c.device_id == device_id)
            .map(|c| c.carrier.clone())
            .ok_or_else(|| SignalFailure::Socket(crate::i18n::t().session.online_connection_unavailable.into()))?;
        let transport = SignalTransport::carried(&carrier, session_id)
            .map_err(|error| SignalFailure::Socket(format!("{error:#}")))?;
        let mut hello = Self::local_hello();
        hello.session_id = Some(session_id.to_owned());
        let mut session = Self::with_transport(transport, hello);
        session.request_prefix = format!("p{}-", session_id.get(..8).unwrap_or(session_id));
        Ok(session)
    }

    /// The `hello` of a connection that has no server.
    fn local_hello() -> Hello {
        Hello {
            epoch: 0,
            session_id: None,
            resume_token: String::new(),
            heartbeat_ms: farwing_protocol::direct::PING_INTERVAL_MS as u32,
            idle_timeout_ms: farwing_protocol::direct::IDLE_TIMEOUT_MS as u32,
            resumed: false,
            probe_nodes: Vec::new(),
        }
    }

    /// A host signalling loop without a server: only direct participants.
    pub(crate) fn offline(router: std::sync::Arc<router::Router>) -> Self {
        let mut session = Self::with_transport(
            SignalTransport::offline(),
            Self::local_hello(),
        );
        session.router = Some(router);
        session
    }

    /// Serve direct participants on this presence connection as well.
    pub(crate) fn attach_router(&mut self, router: std::sync::Arc<router::Router>) {
        self.router = Some(router);
    }

    /// A controller's authenticated direct connection to a host
    /// (`crate::transport::direct::client`).
    pub(crate) fn direct<R, W>(reader: R, writer: W, dialed: std::net::IpAddr) -> Self
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let mut session = Self::with_transport(
            SignalTransport::direct(reader, writer),
            Self::local_hello(),
        );
        session.dialed = Some(dialed);
        session
    }

    /// Whether session `sid` of this host belongs to a direct connection.
    pub(crate) fn is_direct(&self, sid: &str) -> bool {
        self.router.as_ref().is_some_and(|router| router.is_direct(sid))
    }

    pub(crate) fn router(&self) -> Option<&std::sync::Arc<router::Router>> {
        self.router.as_ref()
    }

    /// For a remote UDP host candidate: the same port at the dialed address.
    fn derived_candidate(&mut self, candidate: &RTCIceCandidateInit) -> Option<RTCIceCandidateInit> {
        let dialed = self.dialed?;
        if dialed.is_loopback() {
            return None;
        }
        let fields: Vec<&str> = candidate.candidate.split_whitespace().collect();
        if fields.len() < 8
            || !fields[2].eq_ignore_ascii_case("udp")
            || fields[6] != "typ"
            || fields[7] != "host"
        {
            return None;
        }
        let address: std::net::IpAddr = fields[4].parse().ok()?;
        let port: u16 = fields[5].parse().ok()?;
        let priority: u32 = fields[3].parse().ok()?;
        if address == dialed || address.is_ipv4() != dialed.is_ipv4() || !self.derived.insert((dialed, port)) {
            return None;
        }
        let foundation = fields[0].strip_prefix("candidate:").unwrap_or(fields[0]);
        Some(RTCIceCandidateInit {
            candidate: format!(
                "candidate:{foundation}d {} udp {} {dialed} {port} typ srflx raddr {address} rport {port}",
                fields[1],
                (100u32 << 24) | (priority & 0x00FF_FFFF),
            ),
            sdp_mid: candidate.sdp_mid.clone(),
            sdp_mline_index: candidate.sdp_mline_index,
            username_fragment: candidate.username_fragment.clone(),
        })
    }

    /// Install a remote candidate, plus its derived candidate on a direct
    /// connection.
    async fn install_remote_candidate(&mut self, peer: &NativePeer, candidate: RTCIceCandidateInit) -> Result<()> {
        let derived = self.derived_candidate(&candidate);
        peer.add_remote_candidate(candidate).await?;
        if let Some(derived) = derived {
            tracing::debug!("adding the dialed address of a direct host as a remote candidate");
            if let Err(error) = peer.add_remote_candidate(derived).await {
                tracing::debug!(%error, "derived direct candidate rejected");
            }
        }
        Ok(())
    }

    fn request_id(&mut self) -> String {
        let id = format!("{}{}", self.request_prefix, self.next_request);
        self.next_request += 1;
        id
    }

    pub(crate) async fn start_control(
        &mut self,
        local: identity::Local,
        profile: ConnectionMediaProfile,
        preferences: Option<crate::features::stream_control::StreamControlPreferences>,
        purpose: crate::session::negotiation::ControlPurpose,
    ) -> Result<ControlSessionInfo> {
        tracing::debug!(?profile, "starting session handshake");
        let decoder_support = detect_native_decoder_support(profile)?;
        tracing::info!(
            ?decoder_support,
            "advertising native decoder configuration capabilities"
        );
        let mut start = build_session_start(
            &local.identity.device_id,
            &decoder_support,
            profile,
            preferences,
            purpose,
        )?;
        start.visit.clone_from(&self.visit);
        let attempt_id = start.attempt_id.clone();
        let id = self.request_id();
        // A carried session names itself: the presence connection has no
        // session of its own.
        let start_envelope = Envelope::new(types::SESSION_START, &start).with_id(id.clone());
        let start_envelope = match self.hello.session_id.clone().filter(|_| self.request_prefix != "c-") {
            Some(sid) => start_envelope.with_sid(sid),
            None => start_envelope,
        };
        let handshake = std::time::Instant::now();
        self.transport.send(start_envelope)?;
        let started = timeout(CONTROL_TIMEOUT, self.wait_reply(&id))
            .await
            .context("session handshake timed out")??;
        tracing::info!(
            carried = self.request_prefix != "c-",
            elapsed_ms = handshake.elapsed().as_millis() as u64,
            "session.start acknowledged"
        );
        let started: SessionStarted = started.body().context("session.start ack is invalid")?;
        anyhow::ensure!(
            farwing_protocol::ids::is_uuid(&started.session_id)
                && !started.participant_id.is_empty(),
            "session.start ack did not contain a usable session"
        );
        crate::account::api::validate_device_id(&started.host.device_id)
            .context("session.start ack did not identify the host")?;
        let binding = identity::Binding {
            local,
            role: identity::Role::Controller,
            peer_device: started.host.device_id.clone(),
            peer_key: started.host.public_key.clone(),
            session_id: started.session_id.clone(),
            attempt_id: attempt_id.clone(),
        };
        binding.check_trust()?;
        let info = ControlSessionInfo {
            binding,
            relation: started.relation,
            grants: started.grants,
            participant_id: started.participant_id,
            session_id: started.session_id,
            ice_servers: started.ice_servers,
            force_relay: started.force_relay,
            attempt_id,
            local_capability: decoder_support,
        };
        tracing::debug!(?info, "session handshake accepted");
        Ok(info)
    }

    /// Wait for `ack`/`error` of request `id`, deferring everything else.
    async fn wait_reply(&mut self, id: &str) -> Result<Envelope> {
        loop {
            match self.transport.next().await? {
                Incoming::Message(envelope)
                    if envelope.re.as_deref() == Some(id)
                        && matches!(envelope.kind.as_str(), types::ACK | types::ERROR) =>
                {
                    if envelope.kind == types::ERROR {
                        let error: farwing_protocol::ErrorBody = envelope
                            .body()
                            .context("realtime error reply is invalid")?;
                        return Err(crate::account::api::ApiFailure::received(error).into());
                    }
                    return Ok(envelope);
                }
                other => self.deferred.push_back(other),
            }
        }
    }

    fn rtc_offer(
        &self,
        info: &ControlSessionInfo,
        sdp: String,
        restart: bool,
        network: u8,
    ) -> Result<Envelope> {
        Ok(Envelope::new(
            types::RTC_OFFER,
            RtcDescription {
                attempt_id: info.attempt_id.clone(),
                restart,
                network,
                sdp: info.binding.sign(sdp)?,
            },
        )
        .with_sid(&info.session_id)
        .with_generation(self.generation))
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
        tracing::debug!(sdp_bytes = offer.len(), "sending WebRTC offer");
        self.generation = 1;
        self.transport
            .send(self.rtc_offer(info, offer, false, network::ALL)?)?;
        report_negotiation(reporter, NegotiationEvent::OfferSent);

        // Two deadlines: the host's answer, then the connection from it.
        let (answered_tx, mut answered) = tokio::sync::watch::channel(false);
        let result = {
            let exchange =
                self.exchange_ice(info, peer, &mut local_candidates, &mut progress, reporter, &answered_tx);
            tokio::pin!(exchange);
            let deadline = tokio::time::sleep(WEBRTC_ANSWER_TIMEOUT);
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    result = &mut exchange => break Ok(result),
                    _ = &mut deadline => break Err(()),
                    changed = answered.changed(), if !*answered.borrow() => {
                        if changed.is_ok() && *answered.borrow() {
                            deadline
                                .as_mut()
                                .reset(tokio::time::Instant::now() + WEBRTC_CONNECT_TIMEOUT);
                        }
                    }
                }
            }
        };
        match result {
            Ok(result) => result,
            Err(()) => {
                let ice = peer.ice_diagnostics().await;
                tracing::error!(
                    peer_state = %peer.connection_state(),
                    ice_state = %peer.ice_connection_state(),
                    local_candidates = progress.local_candidates_sent,
                    remote_rtc_events = progress.remote_rtc_events,
                    answer_installed = progress.answer_installed,
                    diagnostics = %ice,
                    "WebRTC connection timed out"
                );
                Err(SignalFailure::Peer(format!(
                    "WebRTC did not connect in time (peer: {}, ICE: {}, local candidates sent: {}, remote rtc events: {}, answer installed: {}; {})",
                    peer.connection_state(),
                    peer.ice_connection_state(),
                    progress.local_candidates_sent,
                    progress.remote_rtc_events,
                    progress.answer_installed,
                    ice
                ))
                .into())
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
        self.generation += 1;
        self.transport
            .send(self.rtc_offer(info, offer, true, ice_network_type)?)?;
        tracing::info!(
            generation = self.generation,
            "ICE restart offer sent; the keep-alive loop installs the answer and candidates while the old selected path remains active"
        );
        Ok(())
    }

    async fn apply_network_switch(
        &mut self,
        info: &ControlSessionInfo,
        peer: &NativePeer,
        relay: bool,
    ) -> Result<()> {
        peer.switch_ice_network(relay).await?;
        self.restart_ice(info, peer, if relay { network::RELAY } else { network::ALL })
            .await
    }

    fn send_candidate(
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
        tracing::trace!("sending local ICE candidate");
        self.transport.send(
            Envelope::new(
                types::RTC_CANDIDATE,
                RtcCandidate {
                    attempt_id: info.attempt_id.clone(),
                    candidate: negotiation::from_candidate_init(candidate),
                },
            )
            .with_sid(&info.session_id)
            .with_generation(self.generation),
        )
    }

    /// Whether an answer belongs to the latest offer generation.
    fn current_answer(&self, generation: u64) -> bool {
        generation == 0 || generation == self.generation
    }

    async fn exchange_ice(
        &mut self,
        info: &ControlSessionInfo,
        peer: &NativePeer,
        local_candidates: &mut tokio::sync::broadcast::Receiver<Option<RTCIceCandidateInit>>,
        progress: &mut NegotiationProgress,
        reporter: Option<NegotiationReporter<'_>>,
        answered: &tokio::sync::watch::Sender<bool>,
    ) -> Result<()> {
        use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

        let mut local_gathering_complete = false;
        let mut answer_installed = false;
        let mut pending_events = VecDeque::new();

        loop {
            if peer.connection_state() == RTCPeerConnectionState::Connected {
                self.deferred.append(&mut pending_events);
                report_negotiation(reporter, NegotiationEvent::Connected);
                return Ok(());
            }
            tokio::select! {
                candidate = local_candidates.recv(), if !local_gathering_complete => {
                    match candidate {
                        Ok(Some(candidate)) => {
                            self.send_candidate(info, &candidate, false)?;
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
                incoming = self.next() => match incoming? {
                    Incoming::Message(envelope) if envelope.kind == types::SESSION_SETTING => {
                        match parse_setting(&envelope, info) {
                            Ok(Some(SettingEvent::DeviceCapability(capability))) => {
                                apply_device_capability(capability, info, peer);
                            }
                            Ok(_) => pending_events.push_back(Incoming::Message(envelope)),
                            Err(error) => tracing::warn!(%error, "discarded invalid session.setting during negotiation"),
                        }
                    }
                    Incoming::Message(envelope)
                        if matches!(envelope.kind.as_str(), types::RTC_ANSWER | types::RTC_CANDIDATE) =>
                    {
                        progress.remote_rtc_events += 1;
                        let rtc = match parse_remote_rtc(&envelope, info) {
                            Ok(rtc) => rtc,
                            Err(error) if error.is::<negotiation::IdentityFailure>() => {
                                return Err(error);
                            }
                            Err(error) => {
                                tracing::warn!(%error, "discarded invalid rtc message during negotiation");
                                continue;
                            }
                        };
                        match rtc {
                            Some(RemoteRtc::Answer { sdp, restart_ice, generation }) => {
                                if !self.current_answer(generation) {
                                    tracing::debug!(generation, current = self.generation, "discarded stale answer");
                                    continue;
                                }
                                tracing::debug!(restart_ice, sdp_bytes = sdp.len(), "installing remote WebRTC answer");
                                if let Err(error) = peer.set_remote_answer(sdp, restart_ice).await {
                                    tracing::warn!(%error, "remote answer rejected; awaiting a valid answer or peer deadline");
                                    continue;
                                }
                                answer_installed = true;
                                progress.answer_installed = true;
                                answered.send_replace(true);
                                report_negotiation(reporter, NegotiationEvent::AnswerInstalled);
                            }
                            Some(RemoteRtc::Candidate(candidate)) if answer_installed => {
                                if let Err(error) = self.install_remote_candidate(peer, candidate).await {
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
                            Some(RemoteRtc::Candidate(_)) => {
                                tracing::debug!("discarded remote ICE candidate before initial answer");
                            }
                            None => {}
                        }
                    }
                    other => pending_events.push_back(other),
                },
                state = peer.next_connection_state() => match state {
                    Some(RTCPeerConnectionState::Connected) => {
                        self.deferred.append(&mut pending_events);
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

    pub async fn keep_alive(
        self,
        shutdown: oneshot::Receiver<()>,
        peer: Option<std::sync::Arc<NativePeer>>,
        control: Option<ControlSessionInfo>,
    ) -> Result<()> {
        self.keep_alive_inner(shutdown, peer, control).await
    }

    /// Run a host session on this connection until `until` completes or the
    /// connection fails. The host session outlives the connection: direct
    /// participants stay while the server connection is replaced.
    pub(crate) async fn run_host<F: std::future::Future>(
        &mut self,
        host: &mut host::Session,
        until: F,
    ) -> Result<F::Output> {
        tokio::pin!(until);
        let mut finished = None;
        // `until` keeps being polled while an event is handled: a handler can
        // wait for what `until` holds (the account or guest API while it asks
        // for a presence ticket), which would otherwise never be released.
        macro_rules! polling_until {
            ($work:expr) => {{
                let work = $work;
                tokio::pin!(work);
                loop {
                    tokio::select! {
                        biased;
                        result = &mut work => break result,
                        output = &mut until, if finished.is_none() => finished = Some(output),
                    }
                }
            }};
        }
        loop {
            if let Some(output) = finished.take() {
                return Ok(output);
            }
            tokio::select! {
                biased;
                output = &mut until => return Ok(output),
                event = host.next() => polling_until!(host.apply(event, self))?,
                incoming = self.next() => match incoming? {
                    Incoming::Opened(hello) => {
                        self.hello = *hello;
                        polling_until!(host.signaling_restored(self))?;
                    }
                    Incoming::Message(envelope) => {
                        if let Err(error) = polling_until!(host.event(&envelope, self)) {
                            tracing::warn!(%error, kind = %envelope.kind, "host realtime event rejected");
                        }
                    }
                },
            }
        }
    }

    async fn keep_alive_inner(
        mut self,
        mut shutdown: oneshot::Receiver<()>,
        peer: Option<std::sync::Arc<NativePeer>>,
        control: Option<ControlSessionInfo>,
    ) -> Result<()> {
        let mut local_candidates = peer.as_ref().map(|peer| peer.local_ice_candidates());
        let mut local_candidate_stream_open = local_candidates.is_some();
        let mut active_ice_network_type = network::ALL;
        // Level 1 of recovery: a path that stays disconnected this long gets
        // an in-session ICE restart (`docs/protocol/v2-design.md` §11).
        const ICE_RESTART_AFTER: Duration = Duration::from_secs(2);
        let mut ice_restart_at = None::<tokio::time::Instant>;
        let mut ice_restarts = 0u32;
        // A local network change (adapter, address or default route) also
        // restarts ICE once it settles: the old path may be gone, or a better
        // one may exist.
        let network_changes = std::sync::Arc::new(crate::platform::windows::wol::Changes::default());
        let _network_watcher = peer
            .is_some()
            .then(|| crate::platform::windows::wol::Watcher::new(network_changes.clone()).ok())
            .flatten();
        let watching_network = _network_watcher.is_some();
        let mut network_changed = false;
        let mut network_at = watching_network.then(crate::transport::nat::route::fingerprint);
        // A restart that must happen even on the same default path (after
        // the system resumed, or one the signaling socket was down for).
        let mut forced_restart = false;
        // The restart came due while the signaling socket was down: run it
        // once the socket is back.
        let mut restart_on_reopen = false;
        if peer.is_some() {
            crate::platform::windows::host_lifecycle::watch_resume();
        }
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
            _ = &mut shutdown => Ok(()),
            result = async {
            if peer.is_some() != control.is_some() { bail!("controller signaling lifecycle is incomplete"); }
            loop {
            tokio::select! {
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
                        network.fail(crate::i18n::t().session.route_switch_not_ready);
                        continue;
                    }
                    if info.force_relay && !relay {
                        network.fail(crate::i18n::t().session.route_relay_required);
                        continue;
                    }
                    // Relay: TURNS preferred. Otherwise all candidates.
                    match self.apply_network_switch(info, peer, relay).await {
                        Ok(()) => {
                            peer.accept_manual_network_policy();
                            active_ice_network_type = if relay { network::RELAY } else { network::ALL };
                            network.submitted(relay);
                            tracing::info!(relay, "manual network policy submitted");
                        }
                        Err(error) => {
                            tracing::warn!(%error, relay, "manual network switch failed; retaining current path");
                            network.fail(crate::i18n::t().session.route_switch_incomplete);
                        }
                    }
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
                            self.send_candidate(info, &candidate, active_ice_network_type == network::RELAY)?;
                            tracing::debug!("sent continual-gathering local ICE candidate");
                        }
                        Ok(None) => tracing::debug!("local ICE candidate generation completed"),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(skipped, "continual ICE candidate broadcast lagged");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            local_candidate_stream_open = false;
                        }
                    }
                }
                incoming = self.next() => {
                    let incoming = match incoming {
                        Err(error) => break Err(error),
                        Ok(incoming) => incoming,
                    };
                    match incoming {
                        Incoming::Opened(hello) => {
                            self.hello = *hello;
                            use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
                            if let Some(peer) = peer.as_ref()
                                && (std::mem::take(&mut restart_on_reopen)
                                    || peer.connection_state() == RTCPeerConnectionState::Disconnected)
                            {
                                tracing::info!("signaling is back; restarting ICE");
                                forced_restart = true;
                                ice_restart_at = Some(tokio::time::Instant::now() + Duration::from_millis(200));
                            }
                        }
                        Incoming::Message(envelope) => {
                            let Some(peer) = peer.as_ref() else { continue; };
                            let info = control.as_ref().expect("controller control info exists");
                            match envelope.kind.as_str() {
                                types::RTC_ANSWER | types::RTC_CANDIDATE => {
                                    let rtc = match parse_remote_rtc(&envelope, info) {
                                        Ok(rtc) => rtc,
                                        Err(error) if error.is::<negotiation::IdentityFailure>() => {
                                            return Err(error);
                                        }
                                        Err(error) => {
                                            tracing::warn!(%error, "discarded invalid rtc message");
                                            continue;
                                        }
                                    };
                                    match rtc {
                                        Some(RemoteRtc::Candidate(candidate)) => {
                                            if let Err(error) = self.install_remote_candidate(peer, candidate).await {
                                                tracing::warn!(%error, "discarded rejected remote ICE candidate; session retained");
                                            }
                                        }
                                        Some(RemoteRtc::Answer { sdp, restart_ice, generation }) => {
                                            if !self.current_answer(generation) {
                                                tracing::debug!(generation, current = self.generation, "discarded stale answer");
                                                continue;
                                            }
                                            if let Err(error) = peer.set_remote_answer(sdp, restart_ice).await {
                                                tracing::warn!(%error, "remote answer rejected; session retained");
                                            }
                                        }
                                        None => {}
                                    }
                                }
                                types::SESSION_RELAY_LOST => {
                                    let Ok(lost) = envelope.body::<farwing_protocol::realtime::SessionRelayLost>() else {
                                        tracing::warn!("discarded invalid session.relay_lost");
                                        continue;
                                    };
                                    let servers = lost
                                        .ice_servers
                                        .into_iter()
                                        .map(|s| crate::transport::rtc::IceServer {
                                            urls: s.urls,
                                            username: s.username,
                                            credential: s.credential,
                                        })
                                        .collect();
                                    if let Err(error) = peer.renew_ice_servers(servers).await {
                                        tracing::warn!(%error, "renewed relays not applied");
                                    }
                                    // The relayed path is gone: a new one now rather than
                                    // once ICE notices.
                                    if peer.selected_is_relay().await {
                                        tracing::info!("the server's relay restarted; restarting ICE");
                                        forced_restart = true;
                                        ice_restart_at = Some(tokio::time::Instant::now());
                                    }
                                }
                                types::SESSION_SETTING => {
                                    let parsed = match parse_setting(&envelope, info) {
                                        Ok(parsed) => parsed,
                                        Err(error) => {
                                            tracing::warn!(%error, "discarded invalid session.setting");
                                            continue;
                                        }
                                    };
                                    match parsed {
                                        Some(SettingEvent::DeviceCapability(capability)) => {
                                            apply_device_capability(capability, info, peer);
                                        }
                                        Some(SettingEvent::SenderInfo(infos)) => {
                                            tracing::debug!(?infos, "host sender info received");
                                            peer.performance_monitor().set_remote_senders(infos);
                                        }
                                        None => {}
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ = network_switch_check.tick(), if peer.is_some() => {
                    let peer = peer.as_ref().expect("controller peer exists");
                    if let Some(network) = &network_control {
                        network.connected(peer.can_switch_ice_network());
                        network.observe_route(peer.selected_is_relay().await);
                    }
                    peer.report_media_to_ice().await;
                }
                _ = async {
                    match ice_restart_at {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => pending().await,
                    }
                }, if ice_restart_at.is_some() && peer.is_some() => {
                    ice_restart_at = None;
                    let peer = peer.as_ref().expect("controller peer exists");
                    use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
                    // Only a different default path counts as a network change.
                    let forced = (std::mem::take(&mut network_changed) && {
                        let now = Some(crate::transport::nat::route::fingerprint());
                        let moved = now != network_at;
                        if moved {
                            tracing::info!(before = ?network_at, after = ?now, "local network path changed");
                        }
                        network_at = now;
                        moved
                    }) | std::mem::take(&mut forced_restart);
                    let needed = forced || peer.connection_state() == RTCPeerConnectionState::Disconnected;
                    if needed && !self.transport.is_connected() {
                        tracing::info!("ICE restart waits for signaling");
                        restart_on_reopen = true;
                    } else if needed && ice_restarts < 3 {
                        ice_restarts += 1;
                        let info = control.as_ref().expect("controller control info exists");
                        match self.restart_ice(info, peer, active_ice_network_type).await {
                            Ok(()) => tracing::info!(
                                attempt = ice_restarts,
                                network_changed = forced,
                                "ICE restarted"
                            ),
                            Err(error) => tracing::warn!(%error, "ICE restart failed; waiting for ICE"),
                        }
                    }
                }
                _ = crate::platform::windows::host_lifecycle::RESUMED.notified(), if peer.is_some() => {
                    tracing::info!("system resumed; checking the connection");
                    forced_restart = true;
                    ice_restart_at.get_or_insert(tokio::time::Instant::now() + ICE_RESTART_AFTER);
                }
                _ = network_changes.notify.notified(), if watching_network => {
                    // Checked once the burst of notifications settles.
                    network_changed = true;
                    ice_restart_at.get_or_insert(tokio::time::Instant::now() + ICE_RESTART_AFTER);
                }
                state = async {
                    match peer.as_ref() {
                        Some(peer) => peer.next_connection_state().await,
                        None => pending().await,
                    }
                } => {
                    use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
                    match state {
                        Some(RTCPeerConnectionState::Connected) => {
                            ice_restart_at = None;
                            ice_restarts = 0;
                        }
                        Some(RTCPeerConnectionState::Disconnected) => {
                            ice_restart_at.get_or_insert(tokio::time::Instant::now() + ICE_RESTART_AFTER);
                            tracing::warn!(
                                "native WebRTC peer connection temporarily disconnected; restarting ICE if it does not recover"
                            );
                        }
                        Some(RTCPeerConnectionState::Failed) => {
                            break Err(SignalFailure::Peer("native WebRTC ICE connection failed".into()).into());
                        }
                        Some(RTCPeerConnectionState::Closed) => {
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
        if let Some(peer) = &peer {
            // A signaling-only failure can leave DTLS writable. Retire input
            // before waiting for the socket close so no new clicks are accepted.
            peer.stream_control_handle().mouse().close().await;
        }
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

    async fn next(&mut self) -> Result<Incoming> {
        if let Some(incoming) = self.deferred.pop_front() {
            return Ok(incoming);
        }
        match self.router.clone() {
            Some(router) => tokio::select! {
                incoming = self.transport.next() => incoming,
                Some(envelope) = router.recv() => Ok(Incoming::Message(envelope)),
            },
            None => self.transport.next().await,
        }
    }

    pub(crate) fn send(&mut self, envelope: Envelope) -> Result<()> {
        let envelope = match &self.router {
            Some(router) => match router.route(envelope) {
                Ok(()) => return Ok(()),
                Err(envelope) => envelope,
            },
            None => envelope,
        };
        self.transport.send(envelope)
    }

    pub(crate) async fn send_connected(&mut self, envelope: Envelope) -> Result<()> {
        let envelope = match &self.router {
            Some(router) => match router.route(envelope) {
                Ok(()) => return Ok(()),
                Err(envelope) => envelope,
            },
            None => envelope,
        };
        self.transport.send_connected(envelope).await
    }

    async fn graceful_close(&mut self) -> Result<()> {
        self.transport.close().await
    }
}
