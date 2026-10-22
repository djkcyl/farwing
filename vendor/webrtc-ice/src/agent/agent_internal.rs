use portable_atomic::{AtomicBool, AtomicU64};
use std::collections::HashMap;

use super::agent_transport::*;
use super::local_port::{LocalCandidate, LocalPort, PORT_GATHERING};
use super::*;
use crate::candidate::candidate_base::CandidateBaseConfig;
use crate::candidate::candidate_peer_reflexive::CandidatePeerReflexiveConfig;
use crate::priority::PriorityAttr;
use crate::util::*;
use arc_swap::ArcSwapOption;
use stun::textattrs::Username;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

tokio::task_local! { static IN_ICE_CALLBACK: (); }

#[cfg(test)]
mod payload_transition_test {
    use super::*;
    use crate::candidate::candidate_host::CandidateHostConfig;

    // Exercises the real data callback with loopback sockets: steady media,
    // recovery, pruned paths and a controlled-side switch without an extra ping.
    #[tokio::test]
    async fn payload_callbacks_keep_recovery_without_per_packet_sorts(
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let (internal, _events) = AgentInternal::new(&AgentConfig::default());
        let local_socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await?);
        let peer_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let backup_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        let local: LocalCandidate = Arc::new(
            CandidateHostConfig {
                base_config: CandidateBaseConfig {
                    network: "udp".into(),
                    address: "127.0.0.1".into(),
                    port: local_socket.local_addr()?.port(),
                    component: 1,
                    conn: Some(local_socket),
                    ..Default::default()
                },
                ..Default::default()
            }
            .new_candidate_host()?,
        );
        local.mark_local();
        internal.credential_generations.lock().await.insert(
            0,
            IceCredentials {
                username: "local".into(),
                password: "local-secret".into(),
            },
        );
        let mut pairs = Vec::new();
        for peer in [&peer_socket, &backup_socket] {
            let remote: LocalCandidate = Arc::new(
                CandidateHostConfig {
                    base_config: CandidateBaseConfig {
                        network: "udp".into(),
                        address: "127.0.0.1".into(),
                        port: peer.local_addr()?.port(),
                        component: 1,
                        credentials: IceCredentials {
                            username: "peer".into(),
                            password: "peer-secret".into(),
                        },
                        ..Default::default()
                    },
                    ..Default::default()
                }
                .new_candidate_host()?,
            );
            let pair = Arc::new(CandidatePair::new(local.clone(), remote, true));
            pair.state
                .store(CandidatePairState::Succeeded as u8, Ordering::Relaxed);
            pair.write_state.store(0, Ordering::Relaxed);
            pairs.push(pair);
        }
        *internal.agent_conn.checklist.lock().await = pairs.clone();
        internal
            .agent_conn
            .selected_pair
            .store(Some(pairs[0].clone()));
        internal.is_controlling.store(true, Ordering::Relaxed);
        let (_done, mut work) = internal
            .done_and_force_candidate_contact_rx
            .lock()
            .await
            .take()
            .unwrap();
        let arrival = std::time::Instant::now();
        assert!(
            internal
                .handle_non_stun_traffic(&local, peer_socket.local_addr()?, b"first", arrival)
                .await
        );
        assert!(work.try_recv().is_ok()); // receiving false -> true
        for _ in 0..64 {
            assert!(
                internal
                    .handle_non_stun_traffic(&local, peer_socket.local_addr()?, b"steady", arrival)
                    .await
            );
            assert_eq!(work.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        }
        pairs[0].write_state.store(3, Ordering::Relaxed);
        assert!(
            internal
                .handle_non_stun_traffic(&local, peer_socket.local_addr()?, b"recover", arrival)
                .await
        );
        assert_eq!(pairs[0].write_state.load(Ordering::Relaxed), 2); // not writable
        assert!(work.try_recv().is_ok());
        pairs[0].write_state.store(3, Ordering::Relaxed);
        pairs[0].pruned.store(true, Ordering::Relaxed);
        assert!(
            internal
                .handle_non_stun_traffic(&local, peer_socket.local_addr()?, b"pruned", arrival)
                .await
        );
        assert_eq!(pairs[0].write_state.load(Ordering::Relaxed), 3);
        assert_eq!(work.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        pairs[0].pruned.store(false, Ordering::Relaxed);
        pairs[0].write_state.store(0, Ordering::Relaxed);
        internal.is_controlling.store(false, Ordering::Relaxed);
        pairs[1].remote_nomination.store(1, Ordering::Relaxed);
        assert!(
            internal
                .handle_non_stun_traffic(&local, backup_socket.local_addr()?, b"switch", arrival)
                .await
        );
        let mut packet = [0; 2048];
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                backup_socket.recv_from(&mut packet)
            )
            .await
            .is_err(),
            "4.40 data-received proposal must not emit an extra Binding request"
        );
        assert!(Arc::ptr_eq(
            &internal.agent_conn.get_selected_pair().unwrap(),
            &pairs[1]
        ));
        while work.try_recv().is_ok() {}
        assert!(
            internal
                .handle_non_stun_traffic(&local, backup_socket.local_addr()?, b"selected", arrival)
                .await
        );
        assert_eq!(work.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        internal.begin_close_io();
        assert!(
            !internal
                .handle_non_stun_traffic(&local, backup_socket.local_addr()?, b"closed", arrival)
                .await
        );
        Ok(())
    }
}

pub type ChanCandidateTx =
    Arc<Mutex<Option<mpsc::Sender<Option<Arc<dyn Candidate + Send + Sync>>>>>>;

#[derive(Clone, Default)]
pub(crate) struct UfragPwd {
    pub(crate) local_ufrag: String,
    pub(crate) local_pwd: String,
    pub(crate) remote_ufrag: String,
    pub(crate) remote_pwd: String,
}

pub struct AgentInternal {
    pub(super) punch: Arc<super::punch::Coordinator>,
    pub(crate) ice_role_assigned: AtomicBool,
    io_cancel: CancellationToken,
    io_tasks: TaskTracker,
    io_gate: std::sync::Mutex<()>,
    event_tasks: TaskTracker,
    event_cancel: CancellationToken,
    // State owned by the taskLoop
    pub(crate) on_connected_tx: Mutex<Option<mpsc::Sender<()>>>,
    pub(crate) on_connected_rx: Mutex<Option<mpsc::Receiver<()>>>,

    // State for closing
    pub(crate) done_tx: Mutex<Option<mpsc::Sender<()>>>,
    // force candidate to be contacted immediately (instead of waiting for task ticker)
    pub(crate) force_candidate_contact_tx: mpsc::Sender<bool>,
    pub(crate) done_and_force_candidate_contact_rx:
        Mutex<Option<(mpsc::Receiver<()>, mpsc::Receiver<bool>)>>,

    pub(crate) chan_candidate_tx: ChanCandidateTx,
    pub(crate) chan_candidate_pair_tx: Mutex<Option<mpsc::Sender<()>>>,
    pub(crate) chan_state_tx: Mutex<Option<mpsc::UnboundedSender<ConnectionState>>>,

    pub(crate) on_connection_state_change_hdlr: ArcSwapOption<Mutex<OnConnectionStateChangeHdlrFn>>,
    pub(crate) on_selected_candidate_pair_change_hdlr:
        ArcSwapOption<Mutex<OnSelectedCandidatePairChangeHdlrFn>>,
    pub(crate) on_candidate_hdlr: ArcSwapOption<Mutex<OnCandidateHdlrFn>>,

    pub(crate) tie_breaker: AtomicU64,
    pub(crate) is_controlling: AtomicBool,
    pub(crate) lite: AtomicBool,

    pub(crate) connection_state: AtomicU8, //ConnectionState,

    /// libwebrtc runs Port/Connection/BasicIceController callbacks on one
    /// network thread. Serialize the corresponding async Rust callbacks so a
    /// sort-and-switch pass observes one coherent state snapshot.
    pub(crate) controller_serial: Mutex<()>,
    pub(crate) last_ping_sent_nanos: AtomicU64,
    /// The last binding request could not leave the socket (for example no
    /// route from that interface): it did not use the pacing slot.
    pub(crate) ping_send_failed: AtomicBool,
    /// Consecutive pings that could not be sent; bounds the immediate retries.
    pub(crate) ping_send_failures: AtomicU32,
    pub(crate) ever_had_pair: AtomicBool,
    pub(crate) ever_writable: AtomicBool,

    pub(crate) started_ch_tx: Mutex<Option<broadcast::Sender<()>>>,

    pub(crate) ufrag_pwd: Mutex<UfragPwd>,
    pub(crate) credential_generations: Mutex<HashMap<u32, IceCredentials>>,
    pub(crate) remote_credential_generations: Mutex<Vec<IceCredentials>>,
    pub(crate) active_credential_generation: AtomicU32,

    pub(crate) local_candidates: Mutex<HashMap<NetworkType, Vec<Arc<dyn Candidate + Send + Sync>>>>,
    pub(super) local_ports: Mutex<Vec<Arc<LocalPort>>>,
    pub(crate) remote_candidates:
        Mutex<HashMap<NetworkType, Vec<Arc<dyn Candidate + Send + Sync>>>>,

    // LRU of outbound Binding request Transaction IDs
    pub(crate) pending_binding_requests: Mutex<Vec<BindingRequest>>,

    pub(crate) agent_conn: Arc<AgentConn>,

    // the following variables won't be changed after init_with_defaults()
    pub(crate) insecure_skip_verify: bool,
    pub(crate) max_binding_requests: u16,
    pub(crate) max_outstanding_pings: Option<u16>,
    pub(crate) host_acceptance_min_wait: Duration,
    pub(crate) srflx_acceptance_min_wait: Duration,
    pub(crate) prflx_acceptance_min_wait: Duration,
    pub(crate) relay_acceptance_min_wait: Duration,
    // How long connectivity checks can fail before the ICE Agent
    // goes to disconnected
    pub(crate) disconnected_timeout: Duration,
    // How long connectivity checks can fail before the ICE Agent
    // goes to failed
    pub(crate) failed_timeout: Duration,
    // How often should we send keepalive packets?
    // 0 means never
    pub(crate) keepalive_interval: Duration,
    // How often should we run our internal taskLoop to check for state changes when connecting
    pub(crate) check_interval: Duration,

    // Tracks the last time a STUN consent binding request was sent (nanos since UNIX_EPOCH).
    // Used to enforce RFC 7675 consent freshness independently of media traffic.
    pub(crate) last_consent_ping: AtomicU64,

    pub(crate) network_preference: Option<u32>,
    pub(crate) vpn_preference: u32,
    pub(crate) presume_writable_when_fully_relayed: bool,
    pub(crate) probe_punching_disabled: bool,
}

impl AgentInternal {
    pub(super) fn new(config: &AgentConfig) -> (Self, ChanReceivers) {
        // Lifecycle notifications are ordered, not a media buffer. A close
        // invoked by a state callback must be able to enqueue Closed without
        // waiting for that same callback to consume a capacity-one channel.
        let (chan_state_tx, chan_state_rx) = mpsc::unbounded_channel();
        let (chan_candidate_tx, chan_candidate_rx) = mpsc::channel(1);
        let (chan_candidate_pair_tx, chan_candidate_pair_rx) = mpsc::channel(1);
        let (on_connected_tx, on_connected_rx) = mpsc::channel(1);
        let (done_tx, done_rx) = mpsc::channel(1);
        let (force_candidate_contact_tx, force_candidate_contact_rx) = mpsc::channel(1);
        let (started_ch_tx, _) = broadcast::channel(1);

        let ai = AgentInternal {
            punch: Arc::new(super::punch::Coordinator::default()),
            ice_role_assigned: AtomicBool::new(false),
            io_cancel: CancellationToken::new(),
            io_tasks: TaskTracker::new(),
            io_gate: std::sync::Mutex::new(()),
            event_tasks: TaskTracker::new(),
            event_cancel: CancellationToken::new(),
            on_connected_tx: Mutex::new(Some(on_connected_tx)),
            on_connected_rx: Mutex::new(Some(on_connected_rx)),

            done_tx: Mutex::new(Some(done_tx)),
            force_candidate_contact_tx: force_candidate_contact_tx.clone(),
            done_and_force_candidate_contact_rx: Mutex::new(Some((
                done_rx,
                force_candidate_contact_rx,
            ))),

            chan_candidate_tx: Arc::new(Mutex::new(Some(chan_candidate_tx))),
            chan_candidate_pair_tx: Mutex::new(Some(chan_candidate_pair_tx)),
            chan_state_tx: Mutex::new(Some(chan_state_tx)),

            on_connection_state_change_hdlr: ArcSwapOption::empty(),
            on_selected_candidate_pair_change_hdlr: ArcSwapOption::empty(),
            on_candidate_hdlr: ArcSwapOption::empty(),

            tie_breaker: AtomicU64::new(rand::random::<u64>()),
            is_controlling: AtomicBool::new(config.is_controlling),
            lite: AtomicBool::new(config.lite),

            connection_state: AtomicU8::new(ConnectionState::New as u8),
            controller_serial: Mutex::new(()),
            last_ping_sent_nanos: AtomicU64::new(0),
            ping_send_failed: AtomicBool::new(false),
            ping_send_failures: AtomicU32::new(0),
            ever_had_pair: AtomicBool::new(false),
            ever_writable: AtomicBool::new(false),
            insecure_skip_verify: config.insecure_skip_verify,

            started_ch_tx: Mutex::new(Some(started_ch_tx)),

            //won't change after init_with_defaults()
            max_binding_requests: 0,
            host_acceptance_min_wait: Duration::from_secs(0),
            srflx_acceptance_min_wait: Duration::from_secs(0),
            prflx_acceptance_min_wait: Duration::from_secs(0),
            relay_acceptance_min_wait: Duration::from_secs(0),

            // How long connectivity checks can fail before the ICE Agent
            // goes to disconnected
            disconnected_timeout: Duration::from_secs(0),

            // How long connectivity checks can fail before the ICE Agent
            // goes to failed
            failed_timeout: Duration::from_secs(0),

            // How often should we send keepalive packets?
            // 0 means never
            keepalive_interval: Duration::from_secs(0),

            // How often should we run our internal taskLoop to check for state changes when connecting
            check_interval: Duration::from_secs(0),

            ufrag_pwd: Mutex::new(UfragPwd::default()),
            credential_generations: Mutex::new(HashMap::new()),
            remote_credential_generations: Mutex::new(Vec::new()),
            active_credential_generation: AtomicU32::new(0),

            local_candidates: Mutex::new(HashMap::new()),
            local_ports: Mutex::new(Vec::new()),
            remote_candidates: Mutex::new(HashMap::new()),

            // LRU of outbound Binding request Transaction IDs
            pending_binding_requests: Mutex::new(vec![]),

            // AgentConn
            agent_conn: Arc::new({
                let mut conn = AgentConn::new();
                conn.path_check = Some(force_candidate_contact_tx);
                conn
            }),

            last_consent_ping: AtomicU64::new(0),

            max_outstanding_pings: None,
            network_preference: config.network_preference,
            vpn_preference: config.vpn_preference,
            presume_writable_when_fully_relayed: config.presume_writable_when_fully_relayed,
            probe_punching_disabled: config.probe_punching_disabled,
        };

        let chan_receivers = ChanReceivers {
            chan_state_rx,
            chan_candidate_rx,
            chan_candidate_pair_rx,
        };
        (ai, chan_receivers)
    }

    pub(super) fn spawn_io<F>(&self, future: F)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let _gate = self.io_gate.lock().expect("ICE IO task gate poisoned");
        if self.io_cancel.is_cancelled() {
            return;
        }
        let cancel = self.io_cancel.clone();
        self.io_tasks.spawn(async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {},
                _ = future => {},
            }
        });
    }

    pub(super) fn begin_close_io(&self) {
        let _gate = self.io_gate.lock().expect("ICE IO task gate poisoned");
        self.agent_conn.done.store(true, Ordering::SeqCst);
        self.io_cancel.cancel();
        self.io_tasks.close();
    }

    pub(super) fn abandon(&self) {
        self.punch.clear();
        self.begin_close_io();
        self.event_cancel.cancel();
        self.on_candidate_hdlr.store(None);
        self.on_selected_candidate_pair_change_hdlr.store(None);
        self.on_connection_state_change_hdlr.store(None);
    }

    fn spawn_event<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let cancel = self.event_cancel.clone();
        self.event_tasks.spawn(async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {},
                _ = future => {},
            }
        });
    }

    pub(crate) async fn start_connectivity_checks(
        self: &Arc<Self>,
        is_controlling: bool,
        remote_ufrag: String,
        remote_pwd: String,
    ) -> Result<()> {
        {
            let started_ch_tx = self.started_ch_tx.lock().await;
            if started_ch_tx.is_none() {
                return Err(Error::ErrMultipleStart);
            }
        }

        log::debug!("Started agent: isControlling? {is_controlling}");
        self.set_remote_credentials(remote_ufrag, remote_pwd)
            .await?;
        {
            let _serial = self.controller_serial.lock().await;
            self.is_controlling.store(is_controlling, Ordering::SeqCst);
            self.ice_role_assigned.store(true, Ordering::Release);
            // Connection priority reads its Port's current role,
            // not the role that happened to be set when a pair was created.
            for pair in self.agent_conn.checklist.lock().await.iter() {
                pair.ice_role_controlling
                    .store(is_controlling, Ordering::SeqCst);
            }
        }
        self.start().await;
        {
            let mut started_ch_tx = self.started_ch_tx.lock().await;
            started_ch_tx.take();
        }

        self.update_connection_state(ConnectionState::Checking)
            .await;

        self.request_connectivity_check();

        self.connectivity_checks().await;

        Ok(())
    }

    async fn contact(&self, last_connection_state: &mut ConnectionState) {
        let _serial = self.controller_serial.lock().await;
        if self.agent_conn.done.load(Ordering::SeqCst) {
            return;
        }
        self.process_transport_failures().await;
        self.punch.stop_if_direct(self);
        // Failure is a state notification, not allocator teardown. Continual
        // gathering and surviving/new Connections can still recover the channel.
        self.contact_candidates().await;

        *last_connection_state = self.connection_state.load(Ordering::SeqCst).into();
    }

    async fn connectivity_checks(self: &Arc<Self>) {
        const ZERO_DURATION: Duration = Duration::from_secs(0);
        let mut last_connection_state = ConnectionState::Unspecified;
        let (check_interval, keepalive_interval, disconnected_timeout, failed_timeout) = (
            self.check_interval,
            self.keepalive_interval,
            self.disconnected_timeout,
            self.failed_timeout,
        );

        let done_and_force_candidate_contact_rx = {
            let mut done_and_force_candidate_contact_rx =
                self.done_and_force_candidate_contact_rx.lock().await;
            done_and_force_candidate_contact_rx.take()
        };

        if let Some((mut done_rx, mut force_candidate_contact_rx)) =
            done_and_force_candidate_contact_rx
        {
            let ai = Arc::clone(self);
            self.spawn_io(async move {
                loop {
                    let mut interval = ZERO_DURATION;

                    let mut update_interval = |x: Duration| {
                        if x != ZERO_DURATION && (interval == ZERO_DURATION || interval > x) {
                            interval = x;
                        }
                    };

                    match last_connection_state {
                        ConnectionState::New | ConnectionState::Checking => {
                            // While connecting, check candidates more frequently,
                            // and next exactly when the pacing interval since the
                            // last ping ends rather than a whole tick later.
                            update_interval(check_interval);
                            let last_ping = ai.last_ping_sent_nanos.load(Ordering::Relaxed);
                            let since = CandidatePair::now_nanos().saturating_sub(last_ping);
                            if last_ping != 0 && since < check_interval.as_nanos() as u64 {
                                update_interval(
                                    check_interval
                                        .saturating_sub(Duration::from_nanos(since))
                                        .max(Duration::from_millis(1)),
                                );
                            }
                        }
                        ConnectionState::Connected
                        | ConnectionState::Completed
                        | ConnectionState::Disconnected
                        | ConnectionState::Failed => {
                            update_interval(ai.ice_controller_check_interval().await);
                            update_interval(keepalive_interval);
                        }
                        _ => {}
                    };
                    // Ensure we run our task loop as quickly as the minimum of our various configured timeouts
                    update_interval(disconnected_timeout);
                    update_interval(failed_timeout);

                    if interval == ZERO_DURATION {
                        interval = DEFAULT_CHECK_INTERVAL;
                    }
                    let t = tokio::time::sleep(interval);
                    tokio::pin!(t);

                    tokio::select! {
                        _ = t.as_mut() => {
                            ai.contact(&mut last_connection_state).await;
                        },
                        _ = force_candidate_contact_rx.recv() => {
                            ai.contact(&mut last_connection_state).await;
                        },
                        _ = done_rx.recv() => {
                            return;
                        }
                    }
                }
            });
        }
    }

    pub(crate) async fn update_connection_state(&self, new_state: ConnectionState) {
        if self.agent_conn.done.load(Ordering::SeqCst) && new_state != ConnectionState::Closed {
            return;
        }
        if self.connection_state.load(Ordering::SeqCst) != new_state as u8 {
            log::info!(
                "[{}]: Setting new connection state: {}",
                self.get_name(),
                new_state
            );
            self.connection_state
                .store(new_state as u8, Ordering::SeqCst);

            // Call handler after finishing current task since we may be holding the agent lock
            // and the handler may also require it
            {
                let chan_state_tx = self.chan_state_tx.lock().await;
                if let Some(tx) = &*chan_state_tx {
                    let _ = tx.send(new_state);
                }
            }
        }
    }

    pub(crate) async fn set_selected_pair(&self, p: Option<Arc<CandidatePair>>) {
        log::trace!(
            "[{}]: Set selected candidate pair: {:?}",
            self.get_name(),
            p
        );

        if let Some(p) = p {
            self.ever_writable.store(true, Ordering::Relaxed);
            p.nominated.store(true, Ordering::SeqCst);
            p.note_selected();
            self.agent_conn.selected_pair.store(Some(p));

            self.update_connection_state(ConnectionState::Connected)
                .await;

            // Notify when the selected pair changes
            {
                let chan_candidate_pair_tx = self.chan_candidate_pair_tx.lock().await;
                if let Some(tx) = &*chan_candidate_pair_tx {
                    let _ = tx.send(()).await;
                }
            }

            // Signal connected
            {
                let mut on_connected_tx = self.on_connected_tx.lock().await;
                on_connected_tx.take();
            }
        } else {
            self.agent_conn.selected_pair.store(None);
        }
    }

    pub(crate) async fn add_pair(
        &self,
        local: Arc<dyn Candidate + Send + Sync>,
        remote: Arc<dyn Candidate + Send + Sync>,
    ) {
        if remote.network_type().is_tcp()
            && remote.tcp_type() == TcpType::Active
            && remote.candidate_type() != CandidateType::PeerReflexive
        {
            return;
        }
        let mut checklist = self.agent_conn.checklist.lock().await;
        if checklist
            .iter()
            .any(|pair| pair.local_port.equal(&*local) && pair.remote.equal(&*remote))
        {
            return;
        }
        // AddConnection has one Connection per peer address.
        // A new remote generation replaces that address entry, not a second
        // independent owner of the same Port/address pair.
        let replaced = checklist
            .iter()
            .filter(|pair| {
                Arc::ptr_eq(&pair.local_port, &local) && pair.remote.addr() == remote.addr()
            })
            .cloned()
            .collect::<Vec<_>>();
        if replaced
            .iter()
            .any(|old| old.remote.generation() >= remote.generation())
        {
            return;
        }
        // TurnPort attaches the new Connection to its cached entry before
        // replacing the old address owner.
        let tcp = if let Some(conn) = local.get_conn() {
            if let Some(port) = conn.as_any().downcast_ref::<super::tcp_port::TcpPort>() {
                match port.create_connection(remote.addr()) {
                    Ok(connection) => Some(connection),
                    Err(error) => {
                        log::warn!("TCP Connection creation failed: {error}");
                        return;
                    }
                }
            } else {
                if let Err(error) = conn.prepare_peer(remote.addr()).await {
                    log::warn!("ICE peer preparation failed for {}: {error}", remote.addr());
                    return;
                }
                None
            }
        } else {
            None
        };
        self.ever_had_pair.store(true, Ordering::Relaxed);
        checklist.retain(|pair| !replaced.iter().any(|old| Arc::ptr_eq(pair, old)));
        let local_port_candidate = local.clone();
        // The remote Candidate is copied into each Connection. Incoming
        // network-info must not mutate other Connections or the SDP registry.
        let remote: LocalCandidate = Arc::new(
            crate::candidate::candidate_base::remote_description_copy(&*remote),
        );
        let mut pair =
            CandidatePair::new(local, remote, self.is_controlling.load(Ordering::SeqCst));
        pair.tcp = tcp;
        checklist.push(Arc::new(pair));
        drop(checklist);
        self.note_connections_removed(&replaced).await;
        if let Some(port) = self.port_for_candidate(&local_port_candidate).await {
            port.connection_added();
        }
        if self
            .agent_conn
            .get_selected_pair()
            .is_some_and(|selected| replaced.iter().any(|old| Arc::ptr_eq(old, &selected)))
        {
            self.set_selected_pair(None).await;
        }
    }

    pub(crate) async fn find_pair(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) -> Option<Arc<CandidatePair>> {
        self.find_port_connection(local, remote.addr()).await
    }

    async fn find_port_connection(
        &self,
        local: &LocalCandidate,
        peer: SocketAddr,
    ) -> Option<Arc<CandidatePair>> {
        self.agent_conn
            .checklist
            .lock()
            .await
            .iter()
            .find(|pair| Arc::ptr_eq(&pair.local_port, local) && pair.remote.addr() == peer)
            .cloned()
    }

    pub(crate) fn request_connectivity_check(&self) {
        let _ = self.force_candidate_contact_tx.try_send(true);
    }

    /// Assumes you are holding the lock (must be execute using a.run).
    pub(crate) async fn add_remote_candidate(&self, c: &Arc<dyn Candidate + Send + Sync>) {
        let _serial = self.controller_serial.lock().await;
        if self.agent_conn.done.load(Ordering::SeqCst) {
            return;
        }
        if !self.prepare_remote_candidate(c).await {
            return;
        }
        // Per-Connection prflx descriptions are updated before inserting
        // the separately owned signaled Candidate value.
        for pair in self.agent_conn.checklist.lock().await.iter() {
            pair.remote.update_peer_reflexive(&**c);
        }
        self.add_remote_candidate_inner(c).await;
    }

    async fn prepare_remote_candidate(&self, candidate: &LocalCandidate) -> bool {
        let parameters = self.remote_credential_generations.lock().await;
        let mut credentials = candidate.credentials();
        let generation = if !credentials.username.is_empty() {
            parameters
                .iter()
                .rposition(|p| p.username == credentials.username)
                .map_or(parameters.len() as u32, |index| index as u32)
        } else if candidate.generation() != 0 {
            candidate.generation()
        } else {
            parameters.len().saturating_sub(1) as u32
        };
        if !parameters.is_empty() && generation < (parameters.len() - 1) as u32 {
            log::debug!("discarded stale remote candidate generation {generation}");
            return false;
        }
        if let Some(latest) = parameters.last() {
            if credentials.username.is_empty() {
                credentials.username.clone_from(&latest.username);
            }
            if credentials.username == latest.username && credentials.password.is_empty() {
                credentials.password.clone_from(&latest.password);
            }
        }
        candidate.set_credentials(credentials);
        candidate.set_generation(generation);
        true
    }

    async fn add_remote_candidate_inner(&self, c: &Arc<dyn Candidate + Send + Sync>) {
        let network_type = c.network_type();

        if self
            .remote_candidates
            .lock()
            .await
            .get(&network_type)
            .is_some_and(|candidates| candidates.iter().any(|candidate| candidate.equal(&**c)))
        {
            return;
        }
        // Newest active Ports are visited first. prflx Connections were
        // updated above; remembered signaled values are not mutated in place.
        let local_cands = self
            .local_ports
            .lock()
            .await
            .iter()
            .rev()
            .filter(|port| {
                port.can_create_outbound_connection()
                    && port.canonical.network_type() == network_type
            })
            .map(|port| port.canonical.clone())
            .collect::<Vec<_>>();
        for local in local_cands {
            self.add_pair(local, c.clone()).await;
        }
        // Old advertisement values are forgotten, never their Connections.
        let mut candidates = self.remote_candidates.lock().await;
        for values in candidates.values_mut() {
            values.retain(|candidate| candidate.generation() >= c.generation());
        }
        candidates.entry(network_type).or_default().push(c.clone());
        drop(candidates);

        self.request_connectivity_check();
    }

    pub(crate) async fn add_candidate(
        self: &Arc<Self>,
        c: &Arc<dyn Candidate + Send + Sync>,
    ) -> Result<()> {
        let port = self
            .register_local_port(c.clone(), true, CandidatePair::now_nanos())
            .await?;
        self.publish_port_candidate(&port, c.clone(), true).await?;
        port.complete(false);
        Ok(())
    }

    pub(super) async fn register_local_port(
        self: &Arc<Self>,
        canonical: LocalCandidate,
        read_transport: bool,
        created_at: u64,
    ) -> Result<Arc<LocalPort>> {
        let _serial = self.controller_serial.lock().await;
        if self.agent_conn.done.load(Ordering::SeqCst) {
            return Err(Error::ErrClosed);
        }
        canonical.mark_local();
        if let Some(credentials) = self
            .credentials_for_generation(canonical.generation())
            .await
        {
            canonical.set_credentials(credentials);
        }
        {
            let ports = self.local_ports.lock().await;
            if let Some(existing) = ports
                .iter()
                .find(|port| port.canonical.equal(&*canonical) && !port.closed.is_cancelled())
            {
                if !Arc::ptr_eq(&existing.canonical, &canonical) {
                    let _ = canonical.close().await;
                }
                return Ok(existing.clone());
            }
        }
        let port = LocalPort::new(canonical, created_at);
        if port.canonical.generation() < self.active_credential_generation.load(Ordering::Acquire) {
            port.prune();
        }
        self.local_ports.lock().await.push(port.clone());
        self.start_port_idle_owner(&port);
        if read_transport {
            let initialized = self
                .started_ch_tx
                .lock()
                .await
                .as_ref()
                .map(|tx| tx.subscribe());
            self.start_candidate(&port, initialized).await;
        } else {
            // A shared AllocationSequence owns the one physical reader.
            let (closed, _) = broadcast::channel(1);
            *port.canonical.get_closed_ch().lock().await = Some(closed);
        }
        Ok(port)
    }

    pub(super) async fn publish_port_candidate(
        &self,
        port: &Arc<LocalPort>,
        candidate: LocalCandidate,
        advertise: bool,
    ) -> Result<()> {
        let _serial = self.controller_serial.lock().await;
        if self.agent_conn.done.load(Ordering::SeqCst) {
            return Err(Error::ErrClosed);
        }
        if port.closed.is_cancelled() || port.gathering.load(Ordering::Acquire) != PORT_GATHERING {
            return Ok(()); // Late completion of a stopped/pruned Port.
        }
        candidate.mark_local();
        candidate.set_credentials(port.canonical.credentials());
        // Every Port address is checked, including privately learned
        // local prflx, before a later STUN success publishes an srflx alias.
        if candidate.candidate_type() == CandidateType::ServerReflexive
            && port.candidate_at(candidate.addr()).is_some()
        {
            return Ok(());
        }
        if !port.ready.swap(true, Ordering::AcqRel) {
            let remotes = self
                .remote_candidates
                .lock()
                .await
                .get(&port.canonical.network_type())
                .cloned()
                .unwrap_or_default();
            for remote in remotes {
                self.add_pair(port.canonical.clone(), remote).await;
            }
            // Configure/activate Port and sort before publishing SDP.
            if self.started_ch_tx.lock().await.is_none() {
                self.contact_candidates().await;
            } else {
                self.request_connectivity_check();
            }
            port.keep_alive_until_pruned(); // Normal port lifecycle, not the punch path.
        }
        if !advertise {
            return Ok(());
        }
        {
            let mut aliases = port.aliases.lock().expect("port aliases poisoned");
            if aliases.iter().any(|old| old.equal(&*candidate)) {
                return Ok(());
            }
            aliases.push(candidate.clone());
        }
        self.local_candidates
            .lock()
            .await
            .entry(candidate.network_type())
            .or_default()
            .push(candidate.clone());
        if let Some(tx) = &*self.chan_candidate_tx.lock().await {
            let _ = tx.send(Some(candidate)).await;
        }
        Ok(())
    }

    pub(super) async fn retire_older_ports(&self, generation: u32) {
        let _serial = self.controller_serial.lock().await;
        for port in self.local_ports.lock().await.iter() {
            if port.canonical.generation() < generation {
                port.prune();
            }
        }
        // Do not signal candidate removal or destroy old Connections.
    }

    async fn port_for_candidate(&self, candidate: &LocalCandidate) -> Option<Arc<LocalPort>> {
        self.local_ports
            .lock()
            .await
            .iter()
            .find(|port| Arc::ptr_eq(&port.canonical, candidate))
            .cloned()
    }

    pub(super) async fn candidate_is_currently_published(
        &self,
        candidate: &LocalCandidate,
    ) -> bool {
        self.local_ports.lock().await.iter().any(|port| {
            !port.closed.is_cancelled()
                && !port.pruned.load(Ordering::Acquire)
                && port.contains(candidate)
        })
    }

    pub(super) async fn note_connections_removed(&self, removed: &[Arc<CandidatePair>]) {
        for pair in removed {
            self.pending_binding_requests
                .lock()
                .await
                .retain(|request| {
                    request.local_candidate_id != pair.local_port.id()
                        || request.destination != pair.remote.addr()
                });
            if let Some(tcp) = &pair.tcp {
                tcp.release();
            } else if let Some(conn) = pair.local_port.get_conn() {
                conn.release_peer(pair.remote.addr()).await;
            }
            if let Some(port) = self.port_for_candidate(&pair.local_port).await {
                port.connection_removed();
            }
        }
    }

    fn start_port_idle_owner(self: &Arc<Self>, port: &Arc<LocalPort>) {
        let agent = Arc::downgrade(self);
        let port = Arc::downgrade(port);
        self.spawn_io(async move {
            loop {
                let Some(port) = port.upgrade() else {
                    return;
                };
                if port.closed.is_cancelled() {
                    return;
                }
                if port.idle_expired() {
                    let Some(agent) = agent.upgrade() else {
                        return;
                    };
                    let _serial = agent.controller_serial.lock().await;
                    if port.idle_expired() {
                        agent.destroy_port(&port).await;
                        return;
                    }
                }
                let wait = port.idle_wait();
                tokio::select! {
                    biased;
                    _ = port.closed.cancelled() => return,
                    _ = port.idle_changed.notified() => {},
                    _ = async {
                        if let Some(wait) = wait { tokio::time::sleep(wait).await; }
                        else { std::future::pending::<()>().await; }
                    } => {},
                }
            }
        });
    }

    async fn destroy_port(&self, port: &Arc<LocalPort>) {
        port.closed.cancel();
        self.local_ports
            .lock()
            .await
            .retain(|p| !Arc::ptr_eq(p, port));
        let id = port.canonical.id();
        self.pending_binding_requests
            .lock()
            .await
            .retain(|request| request.local_candidate_id != id);
        self.agent_conn
            .checklist
            .lock()
            .await
            .retain(|pair| pair.local_port.id() != id);
        for candidates in self.local_candidates.lock().await.values_mut() {
            candidates.retain(|candidate| !port.contains(candidate));
        }
        if self
            .agent_conn
            .get_selected_pair()
            .is_some_and(|pair| pair.local_port.id() == id)
        {
            self.set_selected_pair(None).await;
        }
        let _ = port.canonical.close().await;
        log::debug!("destroyed local ICE Port: {}", port.canonical);
    }

    pub(crate) async fn close(&self) -> Result<()> {
        self.punch.clear();
        {
            let mut done_tx = self.done_tx.lock().await;
            if done_tx.is_none() {
                return Err(Error::ErrClosed);
            }
            done_tx.take();
        };
        self.begin_close_io();
        self.io_tasks.wait().await;
        log::debug!("all ICE receive/check/resolve tasks joined");
        self.agent_conn.selected_pair.store(None);
        self.agent_conn.checklist.lock().await.clear();
        self.pending_binding_requests.lock().await.clear();
        self.delete_all_candidates().await;
        {
            let mut started_ch_tx = self.started_ch_tx.lock().await;
            started_ch_tx.take();
        }

        self.agent_conn.buffer.close().await;
        self.on_connected_tx.lock().await.take();

        self.update_connection_state(ConnectionState::Closed).await;

        {
            let mut chan_candidate_tx = self.chan_candidate_tx.lock().await;
            chan_candidate_tx.take();
        }
        {
            let mut chan_candidate_pair_tx = self.chan_candidate_pair_tx.lock().await;
            chan_candidate_pair_tx.take();
        }
        {
            let mut chan_state_tx = self.chan_state_tx.lock().await;
            chan_state_tx.take();
        }

        self.event_tasks.close();
        if IN_ICE_CALLBACK.try_with(|_| ()).is_err() {
            self.event_tasks.wait().await;
            log::debug!("all ICE event dispatchers joined");
        }
        self.on_candidate_hdlr.store(None);
        self.on_selected_candidate_pair_change_hdlr.store(None);
        self.on_connection_state_change_hdlr.store(None);
        self.credential_generations.lock().await.clear();
        self.remote_credential_generations.lock().await.clear();
        *self.ufrag_pwd.lock().await = UfragPwd::default();

        Ok(())
    }

    /// Final teardown: close locally owned transports and discard remote
    /// descriptions. ICE restarts retain the old connections instead.
    pub(crate) async fn delete_all_candidates(&self) {
        let ports = std::mem::take(&mut *self.local_ports.lock().await);
        for port in ports {
            port.closed.cancel();
            let _ = port.canonical.close().await;
        }
        self.local_candidates.lock().await.clear();

        // Clearing remote candidates only destroys remote Candidate
        // values. They do not own our listening ports (or another Agent's
        // socket when an in-process caller passes a candidate Arc directly).
        self.remote_candidates.lock().await.clear();
    }

    async fn process_transport_failures(&self) {
        let closed_tcp = {
            let mut pairs = self.agent_conn.checklist.lock().await;
            let closed = pairs
                .iter()
                .filter(|pair| pair.tcp.as_ref().is_some_and(|tcp| tcp.failed()))
                .cloned()
                .collect::<Vec<_>>();
            pairs.retain(|pair| !closed.iter().any(|old| Arc::ptr_eq(pair, old)));
            closed
        };
        self.note_connections_removed(&closed_tcp).await;
        if self
            .agent_conn
            .get_selected_pair()
            .is_some_and(|selected| closed_tcp.iter().any(|old| Arc::ptr_eq(old, &selected)))
        {
            self.set_selected_pair(None).await;
        }
        let local_candidates = self
            .local_ports
            .lock()
            .await
            .iter()
            .filter(|port| port.canonical.candidate_type() == CandidateType::Relay)
            .map(|port| port.canonical.clone())
            .collect::<Vec<_>>();
        let mut failed = Vec::new();
        for candidate in local_candidates {
            let Some(conn) = candidate.get_conn() else {
                continue;
            };
            for peer_addr in conn.take_failed_peer_addrs() {
                failed.push((candidate.id(), peer_addr));
            }
        }
        if failed.is_empty() {
            return;
        }

        let pairs = self.agent_conn.checklist.lock().await.clone();
        for (local_id, peer_addr) in failed {
            for pair in pairs
                .iter()
                .filter(|pair| pair.local_port.id() == local_id && pair.remote.addr() == peer_addr)
            {
                pair.write_state.store(3, Ordering::SeqCst);
                pair.pruned.store(true, Ordering::SeqCst);
                pair.state
                    .store(CandidatePairState::Failed as u8, Ordering::SeqCst);
                self.pending_binding_requests
                    .lock()
                    .await
                    .retain(|request| {
                        request.local_candidate_id != local_id || request.destination != peer_addr
                    });
                log::info!(
                    "TURN peer state failed; pruned ICE connection after permission/channel-bind failure: {pair}"
                );
            }
        }
        self.request_connectivity_check();
    }

    pub(crate) async fn failed_network_names_for_regather(
        &self,
        current_interfaces: Vec<crate::util::LocalInterface>,
    ) -> std::collections::HashSet<String> {
        // WebRTC defines a failed network as a current network interface with
        // no Connection object at all. Pair write state is intentionally not
        // consulted: a pruned, timed-out, or backup connection still proves
        // that the allocator has already gathered on that network.
        let networks_with_connection = self
            .agent_conn
            .checklist
            .lock()
            .await
            .iter()
            .map(|pair| pair.local_port.network_key())
            .collect::<std::collections::HashSet<_>>();
        current_interfaces
            .into_iter()
            .map(|interface| interface.network_key)
            .filter(|name| !networks_with_connection.contains(name))
            .collect()
    }

    pub(crate) async fn prune_local_candidates_on_networks(
        &self,
        failed_networks: &std::collections::HashSet<String>,
        generation: u32,
    ) {
        let _serial = self.controller_serial.lock().await;
        if generation != self.active_credential_generation.load(Ordering::Acquire) {
            return;
        }
        let ports = self
            .local_ports
            .lock()
            .await
            .iter()
            .filter(|port| {
                port.canonical.generation()
                    == self.active_credential_generation.load(Ordering::Acquire)
                    && failed_networks.contains(&port.canonical.network_key())
            })
            .cloned()
            .collect::<Vec<_>>();
        for port in &ports {
            port.prune();
        }
        for candidates in self.local_candidates.lock().await.values_mut() {
            candidates.retain(|candidate| !ports.iter().any(|port| port.contains(candidate)));
        }
        // Retire Ports and remove advertised candidates only.
        // The controller and last-Connection idle path still own live sockets.
    }

    pub(super) async fn update_network_costs(&self, changes: &[(String, u32)]) {
        if changes.is_empty() {
            return;
        }
        let _serial = self.controller_serial.lock().await;
        for port in self.local_ports.lock().await.iter() {
            if let Some((_, adapter_type)) = changes
                .iter()
                .find(|(key, _)| *key == port.canonical.network_key())
            {
                let cost = ice_network_cost(*adapter_type);
                port.canonical.set_network_cost(cost);
                for candidate in port.aliases.lock().expect("port aliases poisoned").iter() {
                    candidate.set_network_cost(cost);
                }
                for candidate in port
                    .learned
                    .lock()
                    .expect("learned local candidates poisoned")
                    .iter()
                {
                    candidate.set_network_cost(cost);
                }
            }
        }
        for pair in self.agent_conn.checklist.lock().await.iter() {
            if let Some((_, adapter_type)) = changes
                .iter()
                .find(|(key, _)| *key == pair.local_port.network_key())
            {
                pair.update_local_cost(ice_network_cost(*adapter_type));
            }
        }
        self.request_connectivity_check();
    }

    pub(super) async fn networks_failed(
        &self,
        adapter_types: &std::collections::HashSet<u32>,
        generation: u32,
    ) {
        if adapter_types.is_empty() {
            return;
        }
        let _serial = self.controller_serial.lock().await;
        if generation != self.active_credential_generation.load(Ordering::Acquire) {
            return;
        }
        let pairs = self
            .agent_conn
            .checklist
            .lock()
            .await
            .iter()
            .filter(|pair| {
                adapter_types.contains(&pair.local_candidate().adapter_type())
                    && !pair.pruned.load(Ordering::Relaxed)
            })
            .cloned()
            .collect::<Vec<_>>();
        self.pending_binding_requests
            .lock()
            .await
            .retain(|request| {
                !pairs.iter().any(|pair| {
                    request.local_candidate_id == pair.local_port.id()
                        && request.destination == pair.remote.addr()
                })
            });
        for pair in pairs {
            // Clearing pending requests/pings also writes receiving=false,
            // WRITE_INIT. It does not reset the smoothed RTT/sample count.
            pair.receiving_state.store(false, Ordering::Relaxed);
            pair.write_state.store(2, Ordering::Relaxed);
            pair.outstanding_ping_nanos
                .lock()
                .expect("ping history poisoned")
                .clear();
            pair.unanswered_since_nanos.store(0, Ordering::Relaxed);
            pair.consecutive_unanswered_checks
                .store(0, Ordering::Relaxed);
        }
        if self.started_ch_tx.lock().await.is_none() {
            self.contact_candidates().await; // Immediate reason 10 sort.
        }
    }

    pub(crate) async fn find_remote_candidate(
        &self,
        network_type: NetworkType,
        addr: SocketAddr,
    ) -> Option<Arc<dyn Candidate + Send + Sync>> {
        let (ip, port) = (addr.ip(), addr.port());

        let remote_candidates = self.remote_candidates.lock().await;
        let cands = remote_candidates.get(&network_type)?;
        let address = ip.to_string();
        cands
            .iter()
            .filter(|candidate| candidate.address() == address && candidate.port() == port)
            .max_by(|left, right| {
                left.generation().cmp(&right.generation()).then_with(|| {
                    let left_signaled = left.candidate_type() != CandidateType::PeerReflexive;
                    let right_signaled = right.candidate_type() != CandidateType::PeerReflexive;
                    left_signaled.cmp(&right_signaled)
                })
            })
            .cloned()
    }

    pub(crate) async fn send_binding_request(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        self.ping_send_failed.store(false, Ordering::Relaxed);
        log::trace!(
            "[{}]: ping STUN from {} to {}",
            self.get_name(),
            local,
            remote
        );

        self.invalidate_pending_binding_requests(Instant::now())
            .await;
        if let Some(pair) = self.find_pair(local, remote).await {
            pair.requests_sent.fetch_add(1, Ordering::Relaxed);
        }
        {
            let mut pending_binding_requests = self.pending_binding_requests.lock().await;
            pending_binding_requests.push(BindingRequest {
                timestamp: Instant::now(),
                transaction_id: m.transaction_id,
                destination: remote.addr(),
                is_use_candidate: m.contains(ATTR_USE_CANDIDATE),
                credential_generation: local.generation(),
                local_candidate_id: local.id(),
                priority: {
                    let mut priority = PriorityAttr::default();
                    priority.get_from(m).ok().map(|_| priority.0)
                },
            });
        }

        if !self.send_stun(m, local, remote).await {
            self.ping_send_failed.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) async fn send_binding_success(
        &self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        let addr = remote.addr();
        let (ip, port) = (addr.ip(), addr.port());
        let Some(credentials) = self.credentials_for_generation(local.generation()).await else {
            return;
        };
        let local_pwd = credentials.password;

        let (out, result) = {
            let mut out = Message::new();
            let mut setters: Vec<Box<dyn Setter>> =
                vec![Box::new(m.clone()), Box::new(BINDING_SUCCESS)];
            // The retransmit count is echoed before the mapped address.
            if let Ok(value) = m.get(AttrType(0xFF00)) {
                setters.push(Box::new(RawAttribute {
                    typ: AttrType(0xFF00),
                    length: value.len() as u16,
                    value,
                }));
            }
            setters.push(Box::new(XorMappedAddress { ip, port }));
            // The SDK announces support in replies, but leaves active GOOG
            // Ping transmission disabled (field trials +40=true, +41=false).
            if m.get(AttrType(0xC059))
                .ok()
                .is_some_and(|value| value.get(..2).is_some_and(|first| first != [0, 0]))
            {
                setters.push(Box::new(RawAttribute {
                    typ: AttrType(0xC059),
                    length: 2,
                    value: vec![0, 1],
                }));
            }
            setters.push(Box::new(MessageIntegrity::new_short_term_integrity(
                local_pwd,
            )));
            setters.push(Box::new(FINGERPRINT));
            let result = out.build(&setters);
            (out, result)
        };

        if let Err(err) = result {
            log::warn!(
                "[{}]: Failed to handle inbound ICE from: {} to: {} error: {}",
                self.get_name(),
                local,
                remote,
                err
            );
        } else {
            self.send_stun(&out, local, remote).await;
        }
    }

    /// Removes pending binding requests that are over `maxBindingRequestTimeout` old Let HTO be the
    /// transaction timeout, which SHOULD be 2*RTT if RTT is known or 500 ms otherwise.
    ///
    /// reference: (IETF ref-8445)[https://tools.ietf.org/html/rfc8445#appendix-B.1].
    pub(crate) async fn invalidate_pending_binding_requests(&self, filter_time: Instant) {
        let mut pending_binding_requests = self.pending_binding_requests.lock().await;
        let initial_size = pending_binding_requests.len();

        let mut temp = vec![];
        for binding_request in pending_binding_requests.drain(..) {
            if filter_time
                .checked_duration_since(binding_request.timestamp)
                .map(|duration| duration < MAX_BINDING_REQUEST_TIMEOUT)
                .unwrap_or(true)
            {
                temp.push(binding_request);
            }
        }

        *pending_binding_requests = temp;
        let bind_requests_removed = initial_size - pending_binding_requests.len();
        if bind_requests_removed > 0 {
            log::trace!(
                "[{}]: Discarded {} binding requests because they expired",
                self.get_name(),
                bind_requests_removed
            );
        }
    }

    /// Assert that the passed `TransactionID` is in our `pendingBindingRequests` and returns the
    /// destination, If the bindingRequest was valid remove it from our pending cache.
    pub(crate) async fn handle_inbound_binding_success(
        &self,
        id: TransactionId,
        local_id: &str,
        source: SocketAddr,
    ) -> Option<BindingRequest> {
        self.invalidate_pending_binding_requests(Instant::now())
            .await;

        let mut pending_binding_requests = self.pending_binding_requests.lock().await;
        for i in 0..pending_binding_requests.len() {
            if pending_binding_requests[i].transaction_id == id
                && pending_binding_requests[i].local_candidate_id == local_id
                && pending_binding_requests[i].destination == source
            {
                let valid_binding_request = pending_binding_requests.remove(i);
                return Some(valid_binding_request);
            }
        }
        None
    }

    pub(crate) async fn update_local_candidate_from_response(
        &self,
        pair: &Arc<CandidatePair>,
        response: &Message,
        request_priority: Option<u32>,
    ) {
        // XOR-MAPPED-ADDRESS is preferred over MAPPED-ADDRESS. This
        // changes Connection.local_candidate, never its actual Port owner.
        let mapped = if response.contains(ATTR_XORMAPPED_ADDRESS) {
            let mut address = XorMappedAddress::default();
            address
                .get_from(response)
                .ok()
                .map(|_| SocketAddr::new(address.ip, address.port))
        } else {
            let mut address = stun::addr::MappedAddress::default();
            address
                .get_from(response)
                .ok()
                .map(|_| SocketAddr::new(address.ip, address.port))
        };
        let Some(mapped) = mapped else {
            log::debug!("ICE success omitted mapped address; local description unchanged");
            return;
        };
        let Some(port) = self.port_for_candidate(&pair.local_port).await else {
            return;
        };
        let known = port.candidate_at(mapped);
        let current = pair.local_candidate();
        if let Some(known) = known {
            // The comparison includes id/priority and descriptive fields, but not cost.
            let same = current.id() == known.id()
                && current.equal(&*known)
                && current.priority() == known.priority()
                && current.relay_protocol() == known.relay_protocol()
                && current.url() == known.url()
                && current.tcp_type() == known.tcp_type()
                && current.adapter_type() == known.adapter_type()
                && current.network_key() == known.network_key();
            if !same {
                pair.set_local_candidate(Arc::new(
                    crate::candidate::candidate_base::local_description_copy(&*known),
                ));
                log::debug!("ICE Connection local candidate matched Port address: {mapped}");
            }
            return;
        }
        let Some(priority) = request_priority else {
            log::debug!("ICE request omitted priority; local prflx not created");
            return;
        };
        let mut candidate = crate::candidate::candidate_base::local_description_copy(&*current);
        candidate.id = crate::rand::generate_cand_id();
        candidate.candidate_type = CandidateType::PeerReflexive;
        candidate.related_address = Some(CandidateRelatedAddress {
            address: current.address(),
            port: current.port(),
        });
        // IEEE CRC32 of type + previous base IP + protocol +
        // relay protocol + decimal Port tie breaker; no packet/file hashes.
        let foundation = format!(
            "prflx{}{}{}{}",
            current.addr().ip(),
            current.network_type().network_short(),
            current.relay_protocol(),
            self.tie_breaker.load(Ordering::Relaxed)
        );
        candidate.foundation_override = crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC)
            .checksum(foundation.as_bytes())
            .to_string();
        candidate.priority_override = priority;
        candidate.address = mapped.ip().to_string();
        candidate.port = mapped.port();
        if candidate.set_ip(&mapped.ip()).is_err() {
            return;
        }
        let candidate = Arc::new(candidate);
        port.learned
            .lock()
            .expect("learned local candidates poisoned")
            .push(candidate.clone()); // No SDP publication.
        pair.set_local_candidate(candidate);
        log::info!(
            "ICE Connection learned local prflx: {} -> {mapped}; bound Port remains {}",
            current.addr(),
            port.canonical.addr()
        );
    }

    /// Processes STUN traffic from a remote candidate.
    pub(crate) async fn handle_inbound(
        &self,
        m: &mut Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: SocketAddr,
    ) {
        let goog_request = m.typ.value() == 0x0200;
        if !goog_request
            && (m.typ.method != METHOD_BINDING
                || !(m.typ.class == CLASS_SUCCESS_RESPONSE
                    || m.typ.class == CLASS_REQUEST
                    || m.typ.class == CLASS_ERROR_RESPONSE
                    || m.typ.class == CLASS_INDICATION))
        {
            log::trace!(
                "[{}]: unhandled STUN from {} to {} class({}) method({})",
                self.get_name(),
                remote,
                local,
                m.typ.class,
                m.typ.method
            );
            return;
        }

        let _serial = self.controller_serial.lock().await;

        if self.agent_conn.done.load(Ordering::SeqCst) {
            return;
        }

        let connection = self.find_port_connection(local, remote).await;
        if goog_request {
            let Some(credentials) = self.credentials_for_generation(local.generation()).await
            else {
                return;
            };
            // These are authenticated with the Port password, without
            // USERNAME, FP or a check of unknown comprehension-required attrs.
            if assert_inbound_message_integrity(m, credentials.password.as_bytes()).is_err() {
                self.send_binding_error(m, local, remote, 401, "Unauthorized", &[])
                    .await;
                return;
            }
            let Some(connection) = connection else {
                // A compressed probe cannot create a new Connection.
                self.send_binding_error(m, local, remote, 400, "Bad Request", &[])
                    .await;
                return;
            };
            connection.note_check_received(super::agent_config::ICE_RECEIVING_TIMEOUT);
            connection.remote.seen(false);
            let mut response_type = MessageType::default();
            response_type.read_value(0x0300);
            let mut response = Message::new();
            if response
                .build(&[Box::new(m.clone()), Box::new(response_type)])
                .is_ok()
            {
                turn::client::integrity::append_goog_integrity(
                    &mut response.raw,
                    credentials.password.as_bytes(),
                );
                self.send_stun(&response, local, &connection.remote).await;
            }
            self.finish_inbound_request(m, &connection);
            return;
        }
        let mut remote_candidate = connection
            .as_ref()
            .map(|connection| connection.remote.clone());
        let unknown = m
            .attributes
            .0
            .iter()
            .filter(|attribute| attribute.typ.0 & 0xc000 == 0x4000)
            .map(|attribute| attribute.typ.0)
            .collect::<Vec<_>>();
        if m.typ.class == CLASS_SUCCESS_RESPONSE || m.typ.class == CLASS_ERROR_RESPONSE {
            let Some(candidate) = &remote_candidate else {
                return;
            };
            let credentials = candidate.credentials();
            if credentials.password.is_empty()
                || !unknown.is_empty()
                || assert_inbound_message_integrity(m, credentials.password.as_bytes()).is_err()
            {
                return;
            }
            if m.typ.class == CLASS_SUCCESS_RESPONSE {
                self.handle_success_response(m, local, candidate, remote)
                    .await;
            } else {
                let mut error = stun::error_code::ErrorCodeAttribute::default();
                if error.get_from(m).is_err() {
                    return;
                }
                if self
                    .handle_inbound_binding_success(m.transaction_id, &local.id(), remote)
                    .await
                    .is_none()
                {
                    return;
                }
                match error.code.0 {
                    401 | 420 | 500 => {}
                    487 => {
                        self.change_ice_role(!self.is_controlling.load(Ordering::Relaxed))
                            .await
                    }
                    _ => {
                        if let Some(connection) = connection {
                            // Only this Connection is destroyed, not its Port or
                            // unrelated peer entries on the same TURN allocation.
                            self.agent_conn
                                .checklist
                                .lock()
                                .await
                                .retain(|pair| !Arc::ptr_eq(pair, &connection));
                            self.note_connections_removed(&[connection.clone()]).await;
                            if self
                                .agent_conn
                                .get_selected_pair()
                                .is_some_and(|selected| Arc::ptr_eq(&selected, &connection))
                            {
                                self.set_selected_pair(None).await;
                            }
                            self.request_connectivity_check();
                        }
                    }
                }
            }
        } else if m.typ.class == CLASS_REQUEST {
            if !m.contains(ATTR_USERNAME) || !m.contains(ATTR_MESSAGE_INTEGRITY) {
                self.send_binding_error(m, local, remote, 400, "Bad Request", &[])
                    .await;
                return;
            }
            let Some(local_credentials) = self.credentials_for_generation(local.generation()).await
            else {
                return;
            };
            let mut username = Username::new(ATTR_USERNAME, String::new());
            let split = username
                .get_from(m)
                .ok()
                .and_then(|_| username.text.split_once(':'));
            let Some((destination, remote_username)) = split else {
                self.send_binding_error(m, local, remote, 401, "Unauthorized", &[])
                    .await;
                return;
            };
            let remote_username = remote_username.to_owned();
            if destination != local_credentials.username
                || !matches!(m.get(ATTR_MESSAGE_INTEGRITY), Ok(value) if value.len() == 20)
                || assert_inbound_message_integrity(m, local_credentials.password.as_bytes())
                    .is_err()
            {
                self.send_binding_error(m, local, remote, 401, "Unauthorized", &[])
                    .await;
                return;
            }
            if !unknown.is_empty() {
                self.send_binding_error(m, local, remote, 420, "Unknown Attribute", &unknown)
                    .await;
                return;
            }
            if let Some(candidate) = &remote_candidate {
                if candidate.credentials().username != remote_username {
                    self.send_binding_error(m, local, remote, 401, "Unauthorized", &[])
                        .await;
                    return;
                }
            } else {
                remote_candidate = self
                    .remote_candidates
                    .lock()
                    .await
                    .get(&local.network_type())
                    .and_then(|candidates| {
                        candidates.iter().find(|candidate| {
                            candidate.addr() == remote
                                && candidate.credentials().username == remote_username
                        })
                    })
                    .cloned();
            }
            if remote_candidate.is_none() {
                let mut priority = PriorityAttr::default();
                if priority.get_from(m).is_err() {
                    self.send_binding_error(m, local, remote, 400, "Bad Request", &[])
                        .await;
                    return;
                }
                let (mut remote_generation, credentials) = self
                    .remote_parameters_for_username(&remote_username)
                    .await
                    .unwrap_or((
                        0,
                        IceCredentials {
                            username: remote_username,
                            password: String::new(),
                        },
                    ));
                if remote_generation == 0 {
                    if let Ok(value) = m.get(AttrType(0xC070)) {
                        if let Ok(value) = <[u8; 4]>::try_from(value.as_slice()) {
                            remote_generation = u32::from_be_bytes(value);
                        }
                    }
                }
                let (network_id, network_cost) =
                    super::agent_selector::remote_network_info(m).unwrap_or((0, 0));
                let config = CandidatePeerReflexiveConfig {
                    base_config: CandidateBaseConfig {
                        network: local.network_type().network_short(),
                        address: remote.ip().to_string(),
                        port: remote.port(),
                        component: local.component(),
                        priority: priority.0,
                        network_id,
                        network_cost: Some(network_cost),
                        generation: remote_generation,
                        credentials,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let Ok(mut candidate) = config.new_candidate_peer_reflexive() else {
                    return;
                };
                if local.network_type().is_tcp() {
                    candidate.tcp_type = TcpType::Active;
                }
                remote_candidate = Some(Arc::new(candidate));
                // This prflx Connection is created only on its arrival Port.
                // It does not remember/fan out a signaled Candidate to every Port.
            }
            if let Some(candidate) = &remote_candidate {
                if connection.is_none() {
                    self.add_pair(local.clone(), candidate.clone()).await;
                }
                let Some(connection) = self.find_port_connection(local, remote).await else {
                    self.send_binding_error(m, local, remote, 500, "Server Error", &[])
                        .await;
                    return;
                };
                // Receiving is updated even if the role check rejects the
                // request. Mutate the per-Connection copy, not the SDP entry.
                let candidate = &connection.remote;
                remote_candidate = Some(candidate.clone());
                connection.note_check_received(super::agent_config::ICE_RECEIVING_TIMEOUT);
                candidate.seen(false);
                self.request_connectivity_check();
                if !self
                    .check_role_conflict(
                        m,
                        local,
                        remote,
                        &local_credentials.username,
                        &candidate.credentials().username,
                    )
                    .await
                {
                    return;
                }
                self.send_binding_success(m, local, candidate).await;
                self.finish_inbound_request(m, &connection);
            }
        } else if m.typ.class == CLASS_INDICATION && unknown.is_empty() {
            if let Some(connection) = connection {
                // Receiving is updated from a Binding indication without
                // claiming that a connectivity check received a response.
                connection.note_check_received(super::agent_config::ICE_RECEIVING_TIMEOUT);
                self.request_connectivity_check();
            }
        }
        if let Some(candidate) = remote_candidate {
            candidate.seen(false);
        }
    }

    fn finish_inbound_request(&self, request: &Message, connection: &CandidatePair) {
        // Common Binding / GOOG Ping state transitions happen here, after
        // sending the reply. A received request alone does not prove writable.
        if !connection.pruned.load(Ordering::Relaxed)
            && connection.write_state.load(Ordering::Relaxed) == 3
        {
            connection.write_state.store(2, Ordering::Relaxed);
        }
        if !self.is_controlling.load(Ordering::Relaxed) {
            super::agent_selector::apply_remote_nomination(request, connection);
        }
        super::agent_selector::apply_remote_network_info(request, &connection.remote);
        self.request_connectivity_check();
    }

    async fn send_binding_error(
        &self,
        request: &Message,
        local: &LocalCandidate,
        destination: SocketAddr,
        code: u16,
        reason: &str,
        unknown: &[u16],
    ) {
        let Some(credentials) = self.credentials_for_generation(local.generation()).await else {
            return;
        };
        let response = {
            let binding = request.typ.method == METHOD_BINDING;
            let mut response_type = MessageType::default();
            response_type.read_value(if binding { 0x0111 } else { 0x0310 });
            let mut setters: Vec<Box<dyn Setter>> = vec![
                Box::new(request.clone()),
                Box::new(response_type),
                Box::new(stun::error_code::ErrorCodeAttribute {
                    code: stun::error_code::ErrorCode(code),
                    reason: reason.as_bytes().to_vec(),
                }),
            ];
            if !unknown.is_empty() {
                let value = unknown
                    .iter()
                    .flat_map(|typ| typ.to_be_bytes())
                    .collect::<Vec<_>>();
                setters.push(Box::new(RawAttribute {
                    typ: ATTR_UNKNOWN_ATTRIBUTES,
                    length: value.len() as u16,
                    value,
                }));
            }
            // GOOG request errors carry neither integrity nor FP.
            if binding && code != 400 && code != 401 {
                setters.push(Box::new(MessageIntegrity::new_short_term_integrity(
                    credentials.password,
                )));
            }
            if binding {
                setters.push(Box::new(FINGERPRINT));
            }
            let mut response = Message::new();
            if response.build(&setters).is_err() {
                return;
            }
            response
        };
        if let Some(conn) = local.get_conn() {
            let _ = conn.send_control_to(&response.raw, destination).await;
        }
    }

    async fn change_ice_role(&self, controlling: bool) {
        self.is_controlling.store(controlling, Ordering::Release);
        for pair in self.agent_conn.checklist.lock().await.iter() {
            pair.ice_role_controlling
                .store(controlling, Ordering::Release);
        }
        log::info!("ICE role conflict resolved: controlling={controlling}");
        self.request_connectivity_check();
    }

    async fn check_role_conflict(
        &self,
        request: &Message,
        local: &LocalCandidate,
        source: SocketAddr,
        local_username: &str,
        remote_username: &str,
    ) -> bool {
        let tie = |typ| {
            request
                .get(typ)
                .ok()
                .and_then(|bytes| <[u8; 8]>::try_from(bytes.as_slice()).ok())
                .map(u64::from_be_bytes)
        };
        let local_tie = self.tie_breaker.load(Ordering::Relaxed);
        let peer_controlling = tie(ATTR_ICE_CONTROLLING);
        if peer_controlling == Some(local_tie) && remote_username == local_username {
            return true;
        }
        let remote_role = tie(ATTR_ICE_CONTROLLED)
            .map(|value| (false, value))
            .or_else(|| peer_controlling.map(|value| (true, value)));
        let Some((peer_controlling, peer_tie)) = remote_role else {
            return true;
        };
        let controlling = self.is_controlling.load(Ordering::Relaxed);
        if controlling != peer_controlling {
            return true;
        }
        // This comparison is deliberately asymmetric at equality.
        let change = if controlling {
            peer_tie >= local_tie
        } else {
            peer_tie < local_tie
        };
        if change {
            self.change_ice_role(!controlling).await;
            true
        } else {
            self.send_binding_error(request, local, source, 487, "Role Conflict", &[])
                .await;
            false
        }
    }

    /// Connection/P2P data callbacks, serialized with connection retirement.
    /// Ordinary selected media only refreshes receiving and dispatches bytes;
    /// it must not run a whole checklist sort/ping/prune for every packet.
    async fn handle_non_stun_traffic(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: SocketAddr,
        buf: &[u8],
        received_at: std::time::Instant,
    ) -> bool {
        let _serial = self.controller_serial.lock().await;
        if self.agent_conn.done.load(Ordering::SeqCst) {
            return false;
        }
        // UDPPort dispatches to its own address-keyed Connection.
        // A globally known candidate is not proof that this local Port has a
        // Connection, and choosing the newest description loses old-generation
        // receiving state while the previous path is still carrying media.
        let Some(pair) = self.find_port_connection(local, remote).await else {
            return false;
        };
        pair.remote.seen(false);
        // This signals only an actual transition.
        if pair.note_payload_received(super::agent_config::ICE_RECEIVING_TIMEOUT) {
            self.request_connectivity_check();
        }
        if let Err(err) = self
            .agent_conn
            .buffer
            .write_with_timestamp(buf, Some(received_at))
            .await
        {
            log::warn!("[{}]: failed to write packet: {}", self.get_name(), err);
        }
        // On the controlled side, data on another live Connection
        // proposes it directly, without an extra ping.
        if !self.is_controlling.load(Ordering::Relaxed)
            && self
                .agent_conn
                .get_selected_pair()
                .as_ref()
                .is_none_or(|selected| !Arc::ptr_eq(selected, &pair))
        {
            self.ice_on_nonselected_payload(&pair).await;
        }
        // Timed-out write state is reset only after the data callbacks.
        // This does not acknowledge a ping or prove the return path writable.
        if !pair.pruned.load(Ordering::Relaxed) && pair.write_state.load(Ordering::Relaxed) == 3 {
            pair.write_state.store(2, Ordering::Relaxed);
            self.request_connectivity_check();
        }
        true
    }

    /// Sets the credentials of the remote agent.
    pub(crate) async fn set_remote_credentials(
        &self,
        remote_ufrag: String,
        remote_pwd: String,
    ) -> Result<()> {
        if remote_ufrag.is_empty() {
            return Err(Error::ErrRemoteUfragEmpty);
        } else if remote_pwd.is_empty() {
            return Err(Error::ErrRemotePwdEmpty);
        }

        let _serial = self.controller_serial.lock().await;
        let parameters = IceCredentials {
            username: remote_ufrag.clone(),
            password: remote_pwd.clone(),
        };
        let generation = {
            let mut history = self.remote_credential_generations.lock().await;
            if history.last() != Some(&parameters) {
                history.push(parameters.clone());
            }
            (history.len() - 1) as u32
        };
        {
            let mut current = self.ufrag_pwd.lock().await;
            current.remote_ufrag = remote_ufrag;
            current.remote_pwd = remote_pwd;
        }
        for candidate in self.remote_candidates.lock().await.values().flatten() {
            candidate.fill_remote_credentials(&parameters, None);
        }
        for connection in self.agent_conn.checklist.lock().await.iter() {
            connection
                .remote
                .fill_remote_credentials(&parameters, Some(generation));
        }
        self.request_connectivity_check();
        Ok(())
    }

    pub(crate) async fn credentials_for_generation(
        &self,
        generation: u32,
    ) -> Option<IceCredentials> {
        self.credential_generations
            .lock()
            .await
            .get(&generation)
            .cloned()
    }

    pub(crate) async fn connection_credentials(
        &self,
        local: &LocalCandidate,
        remote: &LocalCandidate,
    ) -> Option<UfragPwd> {
        let local = self.credentials_for_generation(local.generation()).await?;
        let remote = remote.credentials();
        if remote.username.is_empty() || remote.password.is_empty() {
            return None;
        }
        Some(UfragPwd {
            local_ufrag: local.username,
            local_pwd: local.password,
            remote_ufrag: remote.username,
            remote_pwd: remote.password,
        })
    }

    async fn remote_parameters_for_username(
        &self,
        username: &str,
    ) -> Option<(u32, IceCredentials)> {
        self.remote_credential_generations
            .lock()
            .await
            .iter()
            .enumerate()
            .rev()
            .find(|(_, parameters)| parameters.username == username)
            .map(|(generation, parameters)| (generation as u32, parameters.clone()))
    }

    /// Whether the message left the socket.
    pub(crate) async fn send_stun(
        &self,
        msg: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) -> bool {
        if let Err(err) = local.write_to(&msg.raw, &**remote, false).await {
            log::trace!(
                "[{}]: failed to send STUN message: {}",
                self.get_name(),
                err
            );
            return false;
        }
        true
    }

    /// Runs the candidate using the provided connection.
    async fn start_candidate(
        self: &Arc<Self>,
        port: &Arc<LocalPort>,
        initialized_ch: Option<broadcast::Receiver<()>>,
    ) {
        let candidate = &port.canonical;
        let (closed_ch_tx, closed_ch_rx) = broadcast::channel(1);
        {
            let closed_ch = candidate.get_closed_ch();
            let mut closed = closed_ch.lock().await;
            *closed = Some(closed_ch_tx);
        }

        let cand = Arc::clone(candidate);
        let port = port.clone();
        let port_closed = port.closed.clone();
        if let Some(conn) = candidate.get_conn() {
            let conn = Arc::clone(conn);
            let ai = Arc::clone(self);
            self.spawn_io(async move {
                let result = ai.recv_loop(port, closed_ch_rx, initialized_ch, conn).await;
                if let Err(error) = result {
                    if !ai.agent_conn.done.load(Ordering::SeqCst) && !port_closed.is_cancelled() {
                        log::debug!("local ICE transport failed: candidate={cand}, reason={error}");
                        ai.remove_failed_local_candidate(&cand).await;
                    }
                }
            });
        } else {
            log::error!("[{}]: Can't start due to conn is_none", self.get_name(),);
        }
    }

    async fn remove_failed_local_candidate(&self, candidate: &Arc<dyn Candidate + Send + Sync>) {
        let _serial = self.controller_serial.lock().await;
        if let Some(port) = self.port_for_candidate(candidate).await {
            self.destroy_port(&port).await;
        }
        self.contact_candidates().await;
        log::info!("removed local ICE candidate after terminal transport failure: {candidate}");
    }

    pub(super) fn start_on_connection_state_change_routine(
        self: &Arc<Self>,
        mut chan_state_rx: mpsc::UnboundedReceiver<ConnectionState>,
        mut chan_candidate_rx: mpsc::Receiver<Option<Arc<dyn Candidate + Send + Sync>>>,
        mut chan_candidate_pair_rx: mpsc::Receiver<()>,
    ) {
        let ai = Arc::clone(self);
        self.spawn_event(async move {
            // CandidatePair and ConnectionState are usually changed at once.
            // Blocking one by the other one causes deadlock.
            while chan_candidate_pair_rx.recv().await.is_some() {
                if let (Some(cb), Some(p)) = (
                    &*ai.on_selected_candidate_pair_change_hdlr.load(),
                    &*ai.agent_conn.selected_pair.load(),
                ) {
                    let mut f = cb.lock().await;
                    let local = p.local_candidate();
                    IN_ICE_CALLBACK.scope((), f(&local, &p.remote)).await;
                }
            }
        });

        let ai = Arc::clone(self);
        self.spawn_event(async move {
            loop {
                tokio::select! {
                    opt_state = chan_state_rx.recv() => {
                        if let Some(s) = opt_state {
                            if let Some(handler) = &*ai.on_connection_state_change_hdlr.load() {
                                let mut f = handler.lock().await;
                                IN_ICE_CALLBACK.scope((), f(s)).await;
                            }
                        } else {
                            while let Some(c) = chan_candidate_rx.recv().await {
                                if let Some(handler) = &*ai.on_candidate_hdlr.load() {
                                    let mut f = handler.lock().await;
                                    IN_ICE_CALLBACK.scope((), f(c)).await;
                                }
                            }
                            break;
                        }
                    },
                    opt_cand = chan_candidate_rx.recv() => {
                        if let Some(c) = opt_cand {
                            if let Some(handler) = &*ai.on_candidate_hdlr.load() {
                                let mut f = handler.lock().await;
                                IN_ICE_CALLBACK.scope((), f(c)).await;
                            }
                        } else {
                            while let Some(s) = chan_state_rx.recv().await {
                                if let Some(handler) = &*ai.on_connection_state_change_hdlr.load() {
                                    let mut f = handler.lock().await;
                                    IN_ICE_CALLBACK.scope((), f(s)).await;
                                }
                            }
                            break;
                        }
                    }
                }
            }
        });
    }

    async fn recv_loop(
        self: &Arc<Self>,
        port: Arc<LocalPort>,
        mut closed_ch_rx: broadcast::Receiver<()>,
        initialized_ch: Option<broadcast::Receiver<()>>,
        conn: Arc<dyn util::Conn + Send + Sync>,
    ) -> Result<()> {
        if let Some(mut initialized_ch) = initialized_ch {
            tokio::select! {
                _ = initialized_ch.recv() => {}
                _ = closed_ch_rx.recv() => return Err(Error::ErrClosed),
            }
        }

        let mut buffer = vec![0_u8; RECEIVE_MTU];
        let mut n;
        let mut src_addr;
        let mut received_at;
        loop {
            tokio::select! {
               result = conn.recv_from_with_timestamp(&mut buffer) => {
                   match result {
                       Ok((num, src, arrival)) => {
                            n = num;
                            src_addr = src;
                            received_at = arrival;
                       }
                       Err(err) => return Err(Error::Other(err.to_string())),
                   }
               },
                _  = closed_ch_rx.recv() => return Err(Error::ErrClosed),
            }

            if port.ready.load(Ordering::Acquire) && !port.closed.is_cancelled() {
                self.handle_inbound_candidate_msg(
                    &port.canonical,
                    &buffer[..n],
                    src_addr,
                    received_at,
                )
                .await;
            }
        }
    }

    /// The session's probes when `packet` is this session's probe request.
    pub(super) async fn punch_request(&self, packet: &[u8]) -> Option<super::punch::Probe> {
        if !super::punch::Probe::looks_like_request(packet) {
            return None;
        }
        let credentials = self.ufrag_pwd.lock().await;
        super::punch::Probe::derive(
            &credentials.local_ufrag,
            &credentials.local_pwd,
            &credentials.remote_ufrag,
            &credentials.remote_pwd,
        )
        .filter(|probe| packet == probe.request)
    }

    pub(super) async fn handle_inbound_candidate_msg(
        self: &Arc<Self>,
        c: &Arc<dyn Candidate + Send + Sync>,
        buf: &[u8],
        src_addr: SocketAddr,
        received_at: std::time::Instant,
    ) {
        if let Some(probe) = self.punch_request(buf).await {
            let _serial = self.controller_serial.lock().await;
            if self.agent_conn.done.load(Ordering::Acquire) {
                return;
            }
            let unknown = self.find_port_connection(c, src_addr).await.is_none();
            let udp_port = c.network_type().is_udp() && c.candidate_type() == CandidateType::Host;
            // UDPPort handles known-peer probes itself; common
            // Port handles unknown-peer probes. A known TCP/relay
            // Connection instead receives ordinary data through the normal data path.
            if unknown || udp_port {
                if let Some(conn) = c.get_conn() {
                    if udp_port {
                        let _ = conn.try_send_to(&probe.response, src_addr);
                    } else {
                        let _ = conn.send_control_to(&probe.response, src_addr).await;
                    }
                }
                if unknown {
                    self.punch.on_request(self, src_addr).await;
                }
                return;
            }
        }
        if turn::client::message::is_ice_stun(buf) {
            if let Ok(mut message) =
                turn::client::message::decode(buf, turn::client::message::Dialect::Ice)
            {
                self.handle_inbound(&mut message, c, src_addr).await;
                return;
            }
        }
        if !self
            .handle_non_stun_traffic(c, src_addr, buf, received_at)
            .await
        {
            log::warn!(
                "[{}]: Discarded message, not a valid remote candidate",
                self.get_name(),
                //c.addr().await //from {}
            );
        }
    }

    pub(crate) fn get_name(&self) -> &str {
        if self.is_controlling.load(Ordering::SeqCst) {
            "controlling"
        } else {
            "controlled"
        }
    }
}
