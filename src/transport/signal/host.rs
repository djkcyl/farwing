//! Host role of the presence connection: every controller and viewer of this
//! device is one participant with its own media peer (`docs/protocol/v2-design.md`
//! §4). Participants never displace each other; the display layout is shared.
use super::*;
use crate::i18n::t;
use crate::features::host::desktop;
use crate::features::host::peer::Peer;
use farwing_protocol::realtime::{
    SessionEnd, SessionIncoming, reason,
};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

pub(crate) enum Event {
    Change,
    Candidate(String, RTCIceCandidateInit),
    Poll,
    Prepared(String, Result<PreparedPeer>),
    Power(crate::features::host::power::Event),
}
pub(crate) struct PreparedPeer {
    peer: Peer,
}

/// One connected controller or viewer, keyed by its server session.
struct Participant {
    participant_id: String,
    attempt_id: String,
    /// Generation of the latest offer this host answered.
    generation: u64,
    /// Device identities of the current session attempt.
    binding: super::identity::Binding,
    /// The participant's lease with its bound capabilities.
    granted: crate::features::host::Lease,
    relation: farwing_protocol::api::Relation,
    purpose: farwing_protocol::api::Purpose,
    /// The server's limit on this controller, applied to every grant.
    caps_limit: Option<Vec<String>>,
    peer: Option<Peer>,
    /// Cancels the preparation task while it runs.
    preparing: Option<tokio_util::sync::CancellationToken>,
    early_rtc: VecDeque<Envelope>,
}

/// Host status while the server cannot be reached but direct connections can.

/// The "direct connections only" notice, in whichever language it was set.
fn is_direct_only(message: &str) -> bool {
    crate::i18n::Language::ALL
        .iter()
        .any(|language| crate::i18n::messages(*language).session.host_direct_only == message)
}

/// Tell the server whether this host accepts connections. An unreachable
/// server holds requests until it returns; that must not hold this session
/// (direct connections, closing it on exit) for longer than this.
async fn publish(
    client: &crate::session::host_client::HostClient,
    allowed: bool,
    wait: Duration,
) -> Result<()> {
    tokio::time::timeout(wait, client.publish_policy(allowed))
        .await
        .unwrap_or_else(|_| {
            Err(crate::account::api::ApiFailure::new(
                crate::account::api::ErrorCode::Network,
                "timed out",
            )
            .with_reason(farwing_protocol::error::reason::TIMED_OUT)
            .into())
        })
}
/// Withdrawing waits briefly: `sync_policy` publishes what is still due once
/// the server answers, and a host that left is offline there anyway.
const WITHDRAW_WAIT: Duration = Duration::from_secs(3);

pub(crate) struct Session {
    power: crate::features::host::power::Inbox,
    client: crate::session::host_client::HostClient,
    desired: tokio::sync::watch::Receiver<Option<crate::features::host::AccessRequest>>,
    authorization: Option<crate::features::host::Lease>,
    participants: BTreeMap<String, Participant>,
    preparations: tokio::task::JoinSet<(String, Result<PreparedPeer>)>,
    candidate_tx: tokio::sync::mpsc::UnboundedSender<(String, RTCIceCandidateInit)>,
    candidates: tokio::sync::mpsc::UnboundedReceiver<(String, RTCIceCandidateInit)>,
    /// The display layout shared by all participants.
    displays: Option<Arc<crate::features::host::displays::Session>>,
    initialized: bool,
    /// Publication of a changed device policy (files, ports, power, assist).
    policy_sync: Option<tokio::task::JoinHandle<()>>,
    /// PAKE keys of code-verified assistance sessions, by server session.
    /// Visitor and paired sessions this host let in, by server session,
    /// with their PAKE key when a code was used. Kept until the session
    /// ends, so a reconnection of the same session is still bound.
    assist_keys: HashMap<String, Option<[u8; 32]>>,
    /// Publish the device policy again even if it looks unchanged: it was
    /// enabled while the server was unreachable.
    republish: bool,
}
impl Session {
    pub(crate) fn new(client: crate::session::host_client::HostClient) -> Self {
        crate::features::host::forwarding::set_client(client.clone());
        let (candidate_tx, candidates) = tokio::sync::mpsc::unbounded_channel();
        Self {
            power: client.host.power.bind(),
            desired: client.host.subscribe(),
            client,
            authorization: None,
            participants: BTreeMap::new(),
            preparations: tokio::task::JoinSet::new(),
            candidate_tx,
            candidates,
            displays: None,
            initialized: false,
            policy_sync: None,
            assist_keys: HashMap::new(),
            republish: false,
        }
    }

    /// A server connection is (back) up: publish that this host accepts
    /// connections, as each new presence connection did before direct
    /// sessions kept the host session alive across reconnects.
    pub(crate) fn server_connected(&mut self) {
        let republish = std::mem::take(&mut self.republish);
        if (republish || self.authorization.is_some()) && !self.client.offline() {
            self.policy_sync = None;
            let client = self.client.clone();
            let lease = self.authorization.clone();
            self.policy_sync = Some(tokio::spawn(async move {
                match client.publish_policy(true).await {
                    // The server is back: drop the direct-only notice.
                    Ok(()) => {
                        if let Some(lease) = lease
                            && lease.requested()
                            && is_direct_only(&client.host.status().message)
                        {
                            lease.update(true, false, t().features.host_waiting);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(error = %format!("{error:#}"), "device policy publication failed");
                        tokio::time::sleep(Duration::from_secs(10)).await;
                    }
                }
            }));
        }
    }

    /// The server connection was lost for good: its participants cannot be
    /// reached any more; direct participants stay.
    pub(crate) async fn retain_direct(&mut self, signal: &mut SignalSession) {
        let lost: Vec<_> = self
            .participants
            .keys()
            .filter(|sid| !signal.is_direct(sid))
            .cloned()
            .collect();
        for sid in lost {
            self.remove(&sid, signal, None).await;
        }
    }

    /// Publish local permission changes the server enforces before sessions.
    fn sync_policy(&mut self) {
        if self.policy_sync.as_ref().is_some_and(|task| !task.is_finished())
            || !self.client.is_active()
            || self.client.offline()
        {
            return;
        }
        let desired = self.client.desired_policy();
        if desired == self.client.published_policy() {
            return;
        }
        let client = self.client.clone();
        self.policy_sync = Some(tokio::spawn(async move {
            if let Err(error) = client.publish(desired).await {
                tracing::warn!(error = %format!("{error:#}"), "device policy publication failed");
                // Retry with the next change or poll, not in a tight loop.
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }));
    }
    pub(crate) async fn next(&mut self) -> Event {
        if !self.initialized {
            self.initialized = true;
            return Event::Change;
        }
        tokio::select! {
            event=self.power.next()=>Event::Power(event),
            _=self.desired.changed()=>Event::Change,
            Some(joined)=self.preparations.join_next(), if !self.preparations.is_empty()=>match joined {
                Ok((sid, result)) => Event::Prepared(sid, result),
                Err(error) => {
                    tracing::warn!(%error, "host preparation task ended abnormally");
                    Event::Poll
                }
            },
            Some((sid, candidate))=self.candidates.recv()=>Event::Candidate(sid, candidate),
            _=tokio::time::sleep(Duration::from_millis(250))=>Event::Poll,
        }
    }

    pub(crate) async fn apply(&mut self, event: Event, signal: &mut SignalSession) -> Result<()> {
        match event {
            Event::Power(crate::features::host::power::Event::Prepare(request)) => {
                self.power.prepare(request, self.client.clone());
            }
            Event::Power(crate::features::host::power::Event::Ready(result)) => {
                let operation = async {
                    let request = result?;
                    anyhow::ensure!(
                        request.valid() && self.client.is_active(),
                        "{}",
                        t().features.power_request_cancelled
                    );
                    self.client.host.begin_power()?;
                    // Tell every participant first, so its viewer waits for the
                    // host to come back instead of reporting a closed session.
                    use crate::protocol::host_event::kind;
                    let event = match request.action() {
                        crate::features::host::power::Action::Reboot => kind::RESTARTING,
                        crate::features::host::power::Action::Shutdown => kind::SHUTTING_DOWN,
                    };
                    crate::features::host::participants::broadcast_host_event(event);
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    self.end_all(signal, reason::HOST_CLOSED).await;
                    // Keep the presence access lease: Windows may cancel shutdown for
                    // unsaved applications, and the owner must still be reconnectable.
                    let client = self.client.clone();
                    let action = request.action();
                    let (completed, result) = tokio::sync::oneshot::channel();
                    // Thread-scoped privileges cannot leak into a reused runtime worker.
                    std::thread::Builder::new()
                        .name("host-power".into())
                        .spawn(move || {
                            let _ = completed.send(request.execute(&client));
                        })
                        .context(t().session.power_start_failed)?;
                    result.await.context(t().session.power_interrupted)??;
                    tracing::info!(?action, "Windows accepted remote power request");
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                if let Err(error) = &operation {
                    tracing::warn!(%error,"host power request was not completed");
                }
                self.client
                    .host
                    .power_result(operation.map_err(|e| e.to_string()));
            }
            Event::Prepared(sid, result) => self.prepared(sid, result, signal).await?,
            Event::Change => {
                let started = std::time::Instant::now();
                let request = self.desired.borrow_and_update().clone();
                self.authorization = None;
                self.end_all(signal, reason::REVOKED).await;
                if let Some(request) = request {
                    let lease = self.client.host.lease(&request);
                    if !lease.requested() {
                        return Ok(());
                    }
                    lease.update(false, false, t().features.host_enabling);
                    // Without a server, only direct connections reach this host.
                    let offline = self.client.offline();
                    let published = if offline {
                        Ok(())
                    } else {
                        publish(&self.client, true, Duration::from_secs(10)).await
                    };
                    match published {
                        Ok(()) if lease.requested() && self.client.is_active() => {
                            lease.update(
                                true,
                                false,
                                if offline {
                                    t().session.host_waiting_direct
                                } else {
                                    t().features.host_waiting
                                },
                            );
                            self.authorization = Some(lease);
                            tracing::info!(
                                elapsed_ms = started.elapsed().as_millis(),
                                offline,
                                "host access enabled"
                            );
                        }
                        Ok(()) => {
                            if !offline {
                                let _ = publish(&self.client, false, WITHDRAW_WAIT).await;
                            }
                        }
                        Err(error)
                            if crate::transport::direct::host::accepting()
                                && crate::account::api::failure_code(&error)
                                    .is_some_and(|code| code.retryable()) =>
                        {
                            tracing::warn!(error = %format!("{error:#}"), "server unreachable; host access enabled for direct connections");
                            lease.update(true, false, t().session.host_direct_only);
                            self.authorization = Some(lease);
                            self.republish = true;
                        }
                        Err(error) => {
                            let _ = publish(&self.client, false, WITHDRAW_WAIT).await;
                            lease.update(false, false, t().session.host_service_unavailable);
                            lease.fail((t().session.host_enable_failed)(&format!("{error:#}")));
                        }
                    }
                } else {
                    self.client.host.update(false, false, t().features.host_disabled);
                    if self.client.offline() {
                        return Ok(());
                    }
                    if let Err(error) = publish(&self.client, false, WITHDRAW_WAIT).await {
                        if !self.client.host.requested() {
                            self.client.host.update(
                                false,
                                false,
                                (t().session.host_disabled_unpublished)(&format!("{error:#}")),
                            );
                        }
                    }
                    tracing::info!(
                        elapsed_ms = started.elapsed().as_millis(),
                        "host access disabled"
                    );
                }
            }
            Event::Candidate(sid, candidate) => {
                let Some(participant) = self.participants.get(&sid) else {
                    return Ok(());
                };
                let Some(peer) = participant.peer.as_ref() else {
                    return Ok(());
                };
                let Some(candidate) = peer.local_candidate(candidate).await else {
                    return Ok(());
                };
                let candidate = negotiation::from_candidate_init(&candidate);
                signal.send(
                    Envelope::new(
                        types::RTC_CANDIDATE,
                        RtcCandidate {
                            attempt_id: participant.attempt_id.clone(),
                            candidate,
                        },
                    )
                    .with_sid(&sid)
                    .with_generation(participant.generation),
                )?;
            }
            Event::Poll => self.poll(signal).await?,
        }
        Ok(())
    }

    async fn prepared(
        &mut self,
        sid: String,
        result: Result<PreparedPeer>,
        signal: &mut SignalSession,
    ) -> Result<()> {
        let Some(participant) = self.participants.get_mut(&sid) else {
            // The participant left while its media was being prepared.
            if let Ok(prepared) = result {
                prepared.peer.close().await;
            }
            return Ok(());
        };
        participant.preparing = None;
        match result {
            Ok(mut prepared)
                if self.client.is_active()
                    && self.client.host.requested()
                    && !prepared.peer.access_revoked() =>
            {
                // Candidates of every participant arrive on one channel.
                let mut candidates = std::mem::replace(
                    &mut prepared.peer.candidates,
                    tokio::sync::mpsc::unbounded_channel().1,
                );
                let forward = self.candidate_tx.clone();
                let key = sid.clone();
                tokio::spawn(async move {
                    while let Some(candidate) = candidates.recv().await {
                        if forward.send((key.clone(), candidate)).is_err() {
                            break;
                        }
                    }
                });
                participant.peer = Some(prepared.peer);
                let early: Vec<_> = self
                    .participants
                    .get_mut(&sid)
                    .map(|p| p.early_rtc.drain(..).collect())
                    .unwrap_or_default();
                for envelope in early {
                    self.rtc(&sid, &envelope, signal).await?;
                }
            }
            Ok(prepared) => {
                prepared.peer.close().await;
                self.remove(&sid, signal, Some(reason::REVOKED)).await;
            }
            Err(error) => {
                tracing::warn!(error=%format!("{error:#}"),"host connection preparation failed");
                participant
                    .granted
                    .fail((t().session.host_session_failed)(&format!("{error:#}")));
                // A local display/codec failure belongs to this incoming
                // connection; it must not tear down the presence connection.
                self.remove(&sid, signal, Some(reason::MEDIA_FAILED)).await;
            }
        }
        Ok(())
    }

    async fn poll(&mut self, signal: &mut SignalSession) -> Result<()> {
        self.sync_policy();
        if !self.client.host.requested() {
            self.end_all(signal, reason::REVOKED).await;
            return Ok(());
        }
        let status = self.client.host.status();
        let sids: Vec<_> = self.participants.keys().cloned().collect();
        for sid in sids {
            let Some(participant) = self.participants.get_mut(&sid) else {
                continue;
            };
            // A trusted device removed by another process on this profile
            // (`Farwing trusted remove`) leaves as if removed here.
            if participant.relation == farwing_protocol::api::Relation::Trusted
                && crate::transport::direct::store::no_longer_trusted(&participant.binding.peer_key)
            {
                self.remove(&sid, signal, Some(reason::KICKED)).await;
                continue;
            }
            let Some(peer) = participant.peer.as_ref() else {
                continue;
            };
            if peer.access_revoked() {
                // Host access as a whole is still on (checked above): the
                // host user ended this participant.
                self.remove(&sid, signal, Some(reason::KICKED)).await;
                continue;
            }
            if peer.ended() {
                tracing::info!(error = ?status.error, "host cancelled media session retiring");
                // Source/encoding failure can invalidate verified values;
                // a normal remote close or network loss does not.
                if status.error.is_some() {
                    self.client.host.invalidate_media().await;
                }
                // A lost connection or a media failure is not a user kick:
                // the server frees the participant at once (instead of
                // after its resume window) and a controller that is still
                // there connects again.
                self.remove(&sid, signal, Some(farwing_protocol::realtime::reason::CONNECTION_LOST)).await;
                continue;
            }
        }
        if self.participants.is_empty() {
            // Each participant releases the layout when its peer closes; the
            // last one restores it.
            self.displays = None;
        }
        Ok(())
    }

    pub(crate) async fn event(&mut self, envelope: &Envelope, signal: &mut SignalSession) -> Result<()> {
        if !self.client.is_active() {
            return Ok(());
        }
        if !self.client.host.requested() {
            if envelope.kind == types::SESSION_INCOMING {
                // A controller can still submit a request while its device
                // list is stale. Refuse it without constructing a peer.
                if let Some(sid) = envelope.sid.as_deref() {
                    let _ = signal
                        .send_connected(
                            Envelope::new(
                                types::SESSION_END,
                                SessionEnd {
                                    reason: reason::REVOKED.into(),
                                },
                            )
                            .with_sid(sid),
                        )
                        .await;
                }
                tracing::info!("host incoming connection refused by local access policy");
            }
            return Ok(());
        }
        let sid = envelope.sid.clone().unwrap_or_default();
        match envelope.kind.as_str() {
            types::SESSION_INCOMING => {
                if let Err(error) = self.incoming(envelope, signal).await {
                    tracing::warn!(error = %format!("{error:#}"), "host incoming session refused");
                    if !sid.is_empty() {
                        let why = if error
                            .chain()
                            .any(|cause| cause.is::<crate::features::host::ParticipantLimit>())
                        {
                            reason::HOST_CLOSED
                        } else {
                            reason::MEDIA_FAILED
                        };
                        let _ = signal
                            .send_connected(
                                Envelope::new(
                                    types::SESSION_END,
                                    SessionEnd {
                                        reason: why.into(),
                                    },
                                )
                                .with_sid(&sid),
                            )
                            .await;
                    }
                    return Err(error);
                }
            }
            types::RTC_OFFER | types::RTC_CANDIDATE => {
                let Some(participant) = self.participants.get_mut(&sid) else {
                    return Ok(());
                };
                if participant.peer.is_none() {
                    let bytes: usize = participant
                        .early_rtc
                        .iter()
                        .map(|v| v.data.to_string().len())
                        .sum();
                    anyhow::ensure!(
                        participant.early_rtc.len() < farwing_protocol::limits::MAX_CANDIDATES
                            && bytes + envelope.data.to_string().len() <= 8 * 1024 * 1024,
                        "提前协商数据过多"
                    );
                    participant.early_rtc.push_back(envelope.clone());
                } else {
                    self.rtc(&sid, envelope, signal).await?;
                }
            }
            types::SESSION_BUDGET => {
                let Some(peer) = self.participants.get(&sid).and_then(|p| p.peer.as_ref()) else {
                    return Ok(());
                };
                let budget: farwing_protocol::realtime::SessionBudget =
                    envelope.body().context("出口预算格式无效")?;
                peer.set_server_budget(budget.budget_kbps);
            }
            types::SESSION_RELAYS => {
                let Some(participant) = self.participants.get(&sid) else {
                    return Ok(());
                };
                let relays: farwing_protocol::realtime::SessionRelays =
                    envelope.body().context("中转更新格式无效")?;
                let Some(peer) = participant.peer.as_ref() else {
                    return Ok(());
                };
                if let (Some(forward), Some(key)) = (relays.forward, peer.participant()) {
                    crate::features::host::forwarding::set_allowed(key, forward);
                }
                peer.renew_ice_servers(
                    relays.ice_servers
                        .into_iter()
                        .map(|s| webrtc::ice_transport::ice_server::RTCIceServer {
                            urls: s.urls,
                            username: s.username,
                            credential: s.credential,
                            ..Default::default()
                        })
                        .collect(),
                );
                tracing::info!(relay = relays.relay.allowed, "the session's relays renewed");
            }
            types::SESSION_PERMISSIONS => {
                let Some(participant) = self.participants.get(&sid) else {
                    return Ok(());
                };
                // Only the server decides for own devices and friends;
                // visitors and trusted devices get what this host allows.
                if matches!(
                    participant.relation,
                    farwing_protocol::api::Relation::Own | farwing_protocol::api::Relation::Friend
                ) {
                    let permissions: farwing_protocol::realtime::SessionPermissions =
                        envelope.body().context("权限变更格式无效")?;
                    let grants = match &participant.caps_limit {
                        Some(limit) => permissions.grants.into_iter().filter(|c| limit.contains(c)).collect(),
                        None => permissions.grants,
                    };
                    participant.granted.set_grants(grants);
                    tracing::info!(grants = ?participant.granted.grants(), "participant capabilities changed");
                    // Without what the session is for (`view` for the
                    // desktop), it ends.
                    let required = participant.purpose.required_cap();
                    if !participant.granted.granted(required) {
                        tracing::info!(required, "participant lost the session's capability");
                        self.remove(
                            &sid,
                            signal,
                            Some(farwing_protocol::realtime::reason::PERMISSION_REVOKED),
                        )
                        .await;
                    }
                }
            }
            types::SESSION_ENDED => {
                if self.participants.contains_key(&sid) {
                    let ended: SessionEnd = envelope.body().unwrap_or(SessionEnd {
                        reason: String::new(),
                    });
                    tracing::info!(reason = %ended.reason, "host matching session end received");
                    self.assist_keys.remove(&sid);
                    self.remove(&sid, signal, None).await;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn incoming(&mut self, envelope: &Envelope, signal: &SignalSession) -> Result<()> {
        let authorization = self.authorization.clone().context("本机被控尚未就绪")?;
        anyhow::ensure!(authorization.requested(), "本次被控许可已失效");
        tracing::info!("host received an authenticated incoming session");
        let encoding_settings = self.client.host.encoding_settings();
        let session_id = envelope
            .sid
            .clone()
            .filter(|id| farwing_protocol::ids::is_uuid(id))
            .context("会话标识缺失或无效")?;
        anyhow::ensure!(
            !self.participants.contains_key(&session_id),
            "会话已存在"
        );
        let incoming: SessionIncoming = envelope.body().context("会话参数无效")?;
        anyhow::ensure!(
            !incoming.participant_id.is_empty()
                && incoming.participant_id.len() <= 256
                && farwing_protocol::ids::is_uuid(&incoming.attempt_id),
            "会话参与者标识缺失或无效"
        );
        let mut options =
            crate::features::stream_control::publisher::ConnectOptions::from_wire(&incoming.options);
        // The server authenticated the purpose, relation and controller;
        // nothing in the controller-supplied options is trusted for them.
        use crate::features::stream_control::publisher::kind;
        use farwing_protocol::api::Relation;
        options.kind = match incoming.purpose {
            farwing_protocol::api::Purpose::Desktop
            | farwing_protocol::api::Purpose::Audio
            | farwing_protocol::api::Purpose::Camera => kind::DESKTOP,
            farwing_protocol::api::Purpose::Files | farwing_protocol::api::Purpose::Diagnostics => kind::FILES,
            farwing_protocol::api::Purpose::Ports => kind::PORTS,
        };
        crate::account::api::validate_device_id(&incoming.controller.device_id)
            .context("会话主控身份无效")?;
        options.device_id = incoming.controller.device_id.clone();
        // The controller's SDP must be signed by the device key the server
        // (or the direct handshake) stated; code-verified assistance and
        // pairing are additionally bound to the PAKE.
        let direct = signal
            .router()
            .is_some_and(|router| router.is_direct(&session_id));
        let direct_key = signal
            .router()
            .and_then(|router| router.take_session_key(&session_id));
        if let Some(key) = direct_key {
            self.assist_keys.insert(session_id.clone(), Some(key));
        }
        let pake = if let Some(key) = self.assist_keys.get(&session_id) {
            *key
        } else if incoming.relation == farwing_protocol::api::Relation::Assist && !direct {
            // A visitor through the server gets in only as a request this
            // host let in (its code or its user's confirmation), never on
            // the server's word alone. Direct visitors were let in by the
            // direct endpoint.
            let admission = self
                .client
                .host
                .assistance
                .take_admission(&incoming.controller.device_id)
                .context("本机没有放行这位访客")?;
            self.assist_keys.insert(session_id.clone(), admission);
            admission
        } else {
            None
        };
        let binding = super::identity::Binding {
            local: super::identity::Local {
                identity: self.client.client_identity()?,
                pake,
            },
            role: super::identity::Role::Host,
            peer_device: incoming.controller.device_id.clone(),
            peer_key: incoming.controller.public_key.clone(),
            session_id: session_id.clone(),
            attempt_id: incoming.attempt_id.clone(),
        };
        binding.check_trust()?;
        let file_only = options.kind == kind::FILES;
        let ports_only = options.kind == kind::PORTS;
        let data_only = file_only || ports_only;
        // The peer HELLO negotiates the in-session feature set.
        let audio_only = incoming.purpose == farwing_protocol::api::Purpose::Audio;
        // Cameras only: prepared like a listening session, the displays untouched.
        let camera_only = incoming.purpose == farwing_protocol::api::Purpose::Camera;
        tracing::info!(kind=options.kind, relation=?incoming.relation,
            decoders=?options.decoders, stream=?options.stream,
            "host incoming media capabilities");
        // A visitor (access code) sees the screen: no files- or ports-only session.
        anyhow::ensure!(
            matches!(options.kind, 1 | 5 | 9) && (!data_only || incoming.relation != Relation::Assist),
            "不支持的被控连接类型"
        );
        anyhow::ensure!(
            data_only
                || !options.decoders.is_empty(),
            "主控解码能力为空"
        );
        let servers = incoming
            .ice_servers
            .iter()
            .map(|s| webrtc::ice_transport::ice_server::RTCIceServer {
                urls: s.urls.clone(),
                username: s.username.clone(),
                credential: s.credential.clone(),
                ..Default::default()
            })
            .collect();
        anyhow::ensure!(
            !self.client.is_guest() || matches!(incoming.relation, Relation::Assist | Relation::Trusted),
            "未登录时只接受访客与受信任设备"
        );
        let authorization = if incoming.relation == Relation::Assist {
            authorization.for_assistance()?
        } else {
            authorization
        };
        // Capabilities of this participant: the server decides for own
        // devices and friends; this host for the devices it trusts and for
        // visitors (`docs/access-model.md` §2.2).
        let grants = match incoming.relation {
            Relation::Own => farwing_protocol::caps::all(),
            Relation::Friend => incoming.grants.clone(),
            Relation::Trusted => {
                let device = crate::transport::direct::store::find_trusted(&incoming.controller.public_key)
                    .filter(|d| d.device_id == incoming.controller.device_id)
                    .context("本机没有信任这台设备")?;
                crate::transport::direct::store::touch_trusted(&device.public_key);
                crate::transport::direct::store::with_view(&device.grants)
            }
            Relation::Assist => authorization.assist_grants(),
        };
        // No more than the server allows this controller (browsers), whatever
        // the relation grants; a camera session no more than the cameras
        // and the microphone.
        let grants = incoming.controller.limit_grants(grants);
        let grants = if camera_only {
            grants
                .into_iter()
                .filter(|c| c == farwing_protocol::caps::CAMERA || c == farwing_protocol::caps::LISTEN)
                .collect()
        } else {
            grants
        };
        anyhow::ensure!(
            !camera_only || authorization.host().camera_settings().allowed,
            "本机未允许远程查看摄像头"
        );
        anyhow::ensure!(
            grants
                .iter()
                .any(|cap| cap == incoming.purpose.required_cap()),
            "主控没有本次会话所需的权限"
        );
        // What later grant changes may give it (a camera session: the
        // cameras and the microphone at most).
        let caps_limit = if camera_only {
            let limit = incoming.controller.effective_caps_limit();
            Some(
                [farwing_protocol::caps::CAMERA, farwing_protocol::caps::LISTEN]
                    .into_iter()
                    .filter(|c| limit.as_ref().is_none_or(|l| l.iter().any(|x| x == c)))
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
            )
        } else {
            incoming.controller.effective_caps_limit()
        };
        let authorization = authorization
            .with_grants(grants)
            .with_limit(caps_limit.as_deref());
        let direct_ports = signal
            .router()
            .and_then(|router| router.session_ports(&session_id));
        tracing::info!(
            relation = ?incoming.relation,
            grants = ?authorization.grants(),
            participants = self.participants.len() + 1,
            direct = signal.is_direct(&session_id),
            "host participant capabilities"
        );
        // A diagnostics session runs like a file session but needs only its
        // own permission.
        anyhow::ensure!(
            incoming.purpose != farwing_protocol::api::Purpose::Files || authorization.file_access(),
            "本机未允许文件传输"
        );
        anyhow::ensure!(
            !ports_only || authorization.port_policy().0,
            "本机未允许端口转发"
        );
        let owner = authorization.peer_lease()?;
        owner.controller(
            &options.device_id,
            &incoming.participant_id,
            &incoming.controller.public_key,
        );
        owner.bind_session(&session_id);
        owner.set_record(
            &incoming.visit,
            &incoming.controller.device_id,
            signal.is_direct(&session_id).then(|| farwing_protocol::api::DirectRecord {
                started_at: chrono::Utc::now().timestamp(),
                relation: incoming.relation,
                purpose: incoming.purpose,
                controller_name: incoming.controller.alias.clone(),
                controller_platform: incoming.controller.platform,
                ..farwing_protocol::api::DirectRecord::default()
            }),
        );
        owner.set_profile(&crate::features::host::participants::Profile {
            nickname: incoming.controller.display_name().to_owned(),
            device_name: incoming.controller.alias.clone(),
            relation: match incoming.relation {
                farwing_protocol::api::Relation::Own => "own",
                farwing_protocol::api::Relation::Friend => "friend",
                farwing_protocol::api::Relation::Assist => "assist",
                farwing_protocol::api::Relation::Trusted => "trusted",
            }
            .to_owned(),
        });
        let cancel = tokio_util::sync::CancellationToken::new();
        let lease = owner.with_cancellation(cancel.clone());
        // The display layout is shared: the first participant creates it
        // (under the host-wide lease, so it outlives that participant).
        let displays = match self.displays.as_ref().and_then(|d| {
            d.is_open().then(|| d.attach()).flatten()
        }) {
            Some(displays) => displays,
            None => {
                let displays = crate::features::host::displays::Session::new(
                    authorization.clone(),
                    &options.device_id,
                )?;
                self.displays = Some(displays.clone());
                displays
            }
        };
        let task_cancel = cancel.clone();
        let relay = incoming.force_relay;
        let sid = session_id.clone();
        // Multi-viewer forwarding: controllers of server sessions; those
        // only listening join it once they watch the desktop
        // (`docs/protocol/deployment-design.md` §5.4).
        let forwarding_session = (direct_ports.is_none() && !data_only)
            .then(|| (session_id.clone(), incoming.forward, options.e2ee_only));
        // A direct session's ICE binds the endpoint's UDP ports (§12.3).
        self.preparations.spawn(crate::transport::direct::with_session_ports(direct_ports, async move {
            let preparation_started = std::time::Instant::now();
            let result = async {
                let mut display_ms = 0;
                let mut media_ms = 0;
                let (screen, config, negotiated, deferred) = if audio_only || data_only || camera_only
                {
                    (
                        None,
                        crate::features::host::VideoConfig {
                            capturing: false,
                            sending: false,
                            ..Default::default()
                        },
                        Arc::new(crate::features::host::format::Negotiated::deferred()),
                        Some(crate::features::host::peer::screens::Deferred {
                            options: {
                                let mut media = options.clone();
                                media.kind = kind::DESKTOP;
                                media
                            },
                            encoding: encoding_settings,
                        }),
                    )
                } else {
                    // Measurement aid: a slow preparation (an encoder check
                    // after a display change takes 10–15 s).
                    if let Some(ms) = std::env::var("FARWING_DEV_HOST_PREPARE_MS")
                        .ok()
                        .and_then(|v| v.parse::<u64>().ok())
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(ms.min(60_000))).await;
                    }
                    let screen = displays.prepare(options.clone()).await?;
                    display_ms = preparation_started.elapsed().as_millis();
                    // Load the GPU driver while the peer connection is prepared.
                    crate::platform::capture::warm_up(&[
                        screen.adapter,
                        screen.render_adapter.unwrap_or(screen.adapter),
                    ]);
                    let media_started = std::time::Instant::now();
                    let media = lease
                        .prepare_media(screen.clone(), encoding_settings.gpu_compatibility)
                        .await?;
                    media_ms = media_started.elapsed().as_millis();
                    anyhow::ensure!(lease.requested(), "本次被控许可已失效");
                    let capabilities = encoding_settings.select(&media.codecs)?;
                    let prepared = desktop::Prepared::new(
                        &options,
                        screen,
                        &capabilities,
                        encoding_settings,
                    )?;
                    (
                        Some(prepared.screen),
                        prepared.config,
                        prepared.negotiated,
                        None,
                    )
                };
                let peer_started = std::time::Instant::now();
                let peer = Peer::new(
                    screen,
                    owner,
                    task_cancel,
                    displays.clone(),
                    servers,
                    relay,
                    config,
                    options.stream,
                    negotiated,
                    deferred,
                    options.clipboard_level(),
                    options.file_capabilities(),
                    data_only,
                    forwarding_session,
                    camera_only,
                )
                .await?;
                tracing::info!(display_ms, media_ms,
                    peer_ms=peer_started.elapsed().as_millis(),
                    total_ms=preparation_started.elapsed().as_millis(),
                    "host connection preparation completed");
                Ok(PreparedPeer { peer })
            }
            .await;
            if result.is_err()
                && let Err(error) = displays.close().await
            {
                tracing::error!(%error, "initial display preparation rollback failed");
            }
            (sid, result)
        }));
        self.participants.insert(
            session_id,
            Participant {
                participant_id: incoming.participant_id,
                attempt_id: incoming.attempt_id,
                generation: 0,
                binding,
                granted: authorization.clone(),
                relation: incoming.relation,
                purpose: incoming.purpose,
                caps_limit,
                peer: None,
                preparing: Some(cancel),
                early_rtc: VecDeque::new(),
            },
        );
        authorization.update(true, false, t().session.host_preparing_screen);
        Ok(())
    }

    async fn rtc(&mut self, sid: &str, envelope: &Envelope, signal: &mut SignalSession) -> Result<()> {
        let Some(participant) = self.participants.get_mut(sid) else {
            return Ok(());
        };
        let peer = participant.peer.as_ref().context("没有活动被控会话")?;
        match envelope.kind.as_str() {
            types::RTC_OFFER => {
                let offer: RtcDescription = envelope.body().context("offer 格式无效")?;
                if offer.attempt_id != participant.attempt_id {
                    return Ok(());
                }
                let generation = envelope.generation.unwrap_or(0);
                if generation != 0 && generation <= participant.generation {
                    tracing::debug!(generation, current = participant.generation, "discarded stale offer");
                    return Ok(());
                }
                anyhow::ensure!(
                    !offer.sdp.is_empty()
                        && offer.sdp.len() <= farwing_protocol::limits::MAX_SDP_BYTES,
                    "offer SDP 缺失或过大"
                );
                let binding = participant.binding.clone();
                if let Err(error) = binding.verify(&offer.sdp) {
                    tracing::warn!(error = %format!("{error:#}"), "controller SDP identity rejected");
                    self.remove(sid, signal, Some(reason::IDENTITY_MISMATCH)).await;
                    return Ok(());
                }
                let answer = peer.answer(offer.sdp, offer.restart, offer.network).await?;
                let answer = binding.sign(answer)?;
                participant.generation = generation;
                signal.send(
                    Envelope::new(
                        types::RTC_ANSWER,
                        RtcDescription {
                            attempt_id: participant.attempt_id.clone(),
                            restart: offer.restart,
                            network: offer.network,
                            sdp: answer,
                        },
                    )
                    .with_sid(sid)
                    .with_generation(generation),
                )?;
            }
            types::RTC_CANDIDATE => {
                let candidate: RtcCandidate = envelope.body().context("候选格式无效")?;
                if candidate.attempt_id != participant.attempt_id {
                    return Ok(());
                }
                peer.add_candidate(negotiation::to_candidate_init(candidate.candidate)?)
                    .await?
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) async fn signaling_restored(&mut self, _signal: &mut SignalSession) -> Result<()> {
        // Every in-session state travels on the peer connection; the server
        // carries nothing to send again.
        Ok(())
    }

    /// End one participant: tell the server (unless it ended the session
    /// itself) and close its peer.
    async fn remove(&mut self, sid: &str, signal: &mut SignalSession, why: Option<&str>) {
        let Some(mut participant) = self.participants.remove(sid) else {
            return;
        };
        if let Some(why) = why {
            // This host ends the session.
            self.assist_keys.remove(sid);
            Self::notify_termination(signal, sid, why).await;
        }
        if let Some(cancel) = participant.preparing.take() {
            // Its task closes the display reference when it fails.
            cancel.cancel();
        }
        if let Some(peer) = participant.peer.take() {
            peer.close().await;
        }
        participant.granted.update(true, false, t().features.host_waiting);
        tracing::info!(
            participant = %participant.participant_id,
            remaining = self.participants.len(),
            "host participant left"
        );
    }

    async fn end_all(&mut self, signal: &mut SignalSession, why: &str) {
        let sids: Vec<_> = self.participants.keys().cloned().collect();
        for sid in sids {
            self.remove(&sid, signal, Some(why)).await;
        }
        self.displays = None;
    }

    async fn notify_termination(signal: &mut SignalSession, sid: &str, why: &str) {
        // The end notice precedes local peer destruction and only ends this
        // participant's session; it is admitted by the current socket or dropped.
        let notice = Envelope::new(
            types::SESSION_END,
            SessionEnd {
                reason: why.to_owned(),
            },
        )
        .with_sid(sid);
        match signal.send_connected(notice).await {
            Ok(()) => tracing::info!(reason = why, "host session termination submitted"),
            Err(error) => {
                tracing::warn!(%error, "host termination notice unavailable; stopping locally without replay")
            }
        }
    }

    pub(crate) async fn close(mut self) {
        let sids: Vec<_> = self.participants.keys().cloned().collect();
        for sid in sids {
            if let Some(mut participant) = self.participants.remove(&sid) {
                if let Some(cancel) = participant.preparing.take() {
                    cancel.cancel();
                }
                if let Some(peer) = participant.peer.take() {
                    tracing::info!("host presence owner closing active media session");
                    peer.close().await;
                }
            }
        }
        self.preparations.abort_all();
        while self.preparations.join_next().await.is_some() {}
        self.displays = None;
        self.client.host.room_closed();
    }
}


