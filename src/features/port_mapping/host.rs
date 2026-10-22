//! Inbound forwarding belongs to an authenticated peer, never to a listening
//! server. The host resolves the requested target itself and connects only
//! while this participant may use port forwarding.
use super::*;
use crate::features::host::Lease;
use crate::i18n::t;
use std::sync::atomic::AtomicBool;

const MAX_STREAMS: usize = 64;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Status {
    pub connections: usize,
    pub sent: u64,
    pub received: u64,
    pub error: Option<String>,
}

pub(crate) struct Request {
    generation: u64,
    policy: u64,
    open: wire::Open,
}
#[derive(Clone)]
pub(crate) struct Receiver {
    transport: Arc<Transport>,
    generation: Arc<AtomicU64>,
    requests: mpsc::Sender<Request>,
    lease: Lease,
    connected: Arc<AtomicBool>,
    changed: Arc<Notify>,
}
impl Receiver {
    pub(crate) fn new(lease: Lease, connected: Arc<AtomicBool>) -> (Self, mpsc::Receiver<Request>) {
        let (requests, input) = mpsc::channel(64);
        (
            Self {
                transport: Arc::default(),
                generation: Arc::default(),
                requests,
                lease,
                connected,
                changed: Arc::default(),
            },
            input,
        )
    }
    /// Attach the tunnel or dgram channel.
    pub(crate) async fn bind(&self, channel: &Arc<RTCDataChannel>) {
        if channel.label() == crate::protocol::peer::channel::TUNNEL {
            self.invalidate();
        }
        self.transport.bind(channel).await;
    }
    pub(crate) fn close(&self, channel: &Arc<RTCDataChannel>) {
        if self.transport.owns(channel) {
            self.invalidate();
            self.transport.close();
        }
    }
    pub(crate) fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.changed.notify_one();
        self.transport.reset_all();
    }
    /// A tunnel or dgram frame. Returns false for frames of other features.
    pub(crate) async fn receive(
        &self,
        channel: &Arc<RTCDataChannel>,
        bytes: &[u8],
    ) -> Result<bool> {
        if !crate::protocol::peer::is_type(
            bytes,
            &[
                crate::protocol::peer::FrameType::Port,
                crate::protocol::peer::FrameType::PortDatagram,
                crate::protocol::peer::FrameType::Flow,
            ],
        ) {
            return Ok(false);
        }
        if !self.transport.owns(channel) {
            return Ok(true);
        }
        if self.transport.receive_datagram(bytes)? {
            return Ok(true);
        }
        if let Some(open) = self.transport.receive(bytes)? {
            self.requests
                .send(Request {
                    generation: self.generation.load(Ordering::Acquire),
                    policy: self.lease.port_policy().1,
                    open,
                })
                .await
                .context(t().features.mapping_session_closed)?;
        }
        Ok(true)
    }
    fn permitted(&self, generation: u64, policy: u64) -> bool {
        self.generation.load(Ordering::Acquire) == generation
            && self.connected.load(Ordering::Acquire)
            && self.lease.port_policy() == (true, policy)
    }
}

pub(crate) async fn run(
    receiver: Receiver,
    mut input: mpsc::Receiver<Request>,
    stop: CancellationToken,
) {
    let mut jobs = JoinSet::new();
    let stats = Arc::new(Mutex::new(Status::default()));
    let mut generation = receiver.generation.load(Ordering::Acquire);
    let mut policy = receiver.lease.port_policy();
    let mut changes = receiver.lease.port_changes();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let restart = |jobs: &mut JoinSet<()>| {
        receiver.transport.reset_all();
        jobs.abort_all();
    };
    loop {
        if stop.is_cancelled() || !receiver.lease.requested() {
            break;
        }
        let next_generation = receiver.generation.load(Ordering::Acquire);
        let next_policy = receiver.lease.port_policy();
        if next_generation != generation
            || next_policy != policy
            || !receiver.connected.load(Ordering::Acquire)
        {
            if next_generation == generation
                && next_policy != policy
                && receiver.connected.load(Ordering::Acquire)
            {
                // The owner changed the permission: tell the controller.
                receiver
                    .transport
                    .reset_all_remote(wire::code::TURNED_OFF)
                    .await;
            }
            restart(&mut jobs);
            while jobs.join_next().await.is_some() {}
            generation = next_generation;
            policy = next_policy;
        }
        receiver.lease.port_status(lock(&stats).clone());
        tokio::select! {
            biased;
            _=stop.cancelled()=>break,
            Some(_)=jobs.join_next(), if !jobs.is_empty()=>{},
            _=tick.tick()=>{},
            _=changes.changed()=>{},
            _=receiver.changed.notified()=>{},
            request=input.recv()=>{
                let Some(request)=request else{break};
                // Binding or permission may have changed while recv was asleep.
                let actual_generation=receiver.generation.load(Ordering::Acquire);
                let actual_policy=receiver.lease.port_policy();
                if actual_generation!=generation || actual_policy!=policy {
                    restart(&mut jobs);
                    while jobs.join_next().await.is_some() {}
                    generation=actual_generation;policy=actual_policy;
                }
                if request.generation!=generation || request.policy!=policy.1 {continue;}
                let open=request.open;
                // The controller is told the code; this host keeps its own words.
                let denied=if !receiver.permitted(generation,policy.1) {Some((wire::code::FORBIDDEN,t().features.mapping_not_allowed_here.to_owned()))}
                    else if let Err(error)=open.validate() {Some((wire::code::PROTOCOL,error.to_string()))}
                    else if receiver.transport.contains(open.stream) {Some((wire::code::PROTOCOL,t().features.mapping_duplicate_stream.to_owned()))}
                    else if jobs.len()>=MAX_STREAMS {Some((wire::code::LIMIT,wire::describe(wire::code::LIMIT)))}
                    else {None};
                if let Some((code,local))=denied {
                    lock(&stats).error=Some(local);
                    // Bounded rejection in this actor; never spawn one task per invalid request.
                    let _=tokio::time::timeout(Duration::from_secs(2),receiver.transport.send(Body::OpenResult(wire::OpenResult{stream:open.stream,code}))).await;
                    continue;
                }
                let registered=receiver.transport.register(open.stream,stop.child_token());
                let Ok((stream,inbound))=registered else {continue};
                let owner=receiver.clone();
                let stats=stats.clone();
                jobs.spawn(async move {
                    serve(owner,request.generation,request.policy,open,stream,inbound,stats).await;
                });
            }
        }
    }
    input.close();
    receiver.invalidate();
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    receiver.transport.close();
    receiver.lease.port_status(Status::default());
}

/// Resolve the requested target on this host, in resolver order. Unspecified,
/// multicast and broadcast addresses are never connected to.
async fn resolve(open: &wire::Open) -> std::result::Result<Vec<SocketAddr>, (i32, String)> {
    // `(code, this host's own words)`.
    let host = open.host.trim().trim_start_matches('[').trim_end_matches(']');
    let port = u16::try_from(open.port).map_err(|_| (wire::code::PROTOCOL, String::new()))?;
    let candidates: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::time::timeout(RESOLVE_TIMEOUT, tokio::net::lookup_host((host, port)))
            .await
            .map_err(|_| (wire::code::DNS_FAILED, t().features.mapping_resolve_timeout.to_owned()))?
            .map_err(|error| (wire::code::DNS_FAILED, error.to_string()))?
            .collect(),
    };
    let targets: Vec<SocketAddr> = candidates
        .into_iter()
        .filter(|address| {
            let ip = address.ip();
            !ip.is_unspecified()
                && !ip.is_multicast()
                && !matches!(ip, IpAddr::V4(v4) if v4.is_broadcast())
        })
        .collect();
    if targets.is_empty() {
        return Err((wire::code::NOT_UNICAST, t().features.mapping_target_not_unicast.to_owned()));
    }
    Ok(targets)
}

fn connect_code(error: &std::io::Error) -> i32 {
    match error.kind() {
        std::io::ErrorKind::ConnectionRefused => wire::code::REFUSED,
        std::io::ErrorKind::TimedOut => wire::code::TIMEOUT,
        _ => wire::code::UNREACHABLE,
    }
}

async fn serve(
    receiver: Receiver,
    generation: u64,
    policy: u64,
    open: wire::Open,
    stream: Registration,
    mut inbound: mpsc::UnboundedReceiver<Inbound>,
    stats: Arc<Mutex<Status>>,
) {
    let transport = receiver.transport.clone();
    let id = open.stream;
    lock(&stats).connections += 1;
    let answer = |code: i32| {
        let transport = transport.clone();
        async move { transport.send(Body::OpenResult(wire::OpenResult { stream: id, code })).await }
    };
    // A failed open is answered once (`OpenResult`); only an opened stream
    // is reset when it ends, or the controller sees the reset first.
    let mut opened = false;
    let result = async {
        let targets = match resolve(&open).await {
            Ok(targets) => targets,
            Err((code, local)) => {
                answer(code).await?;
                anyhow::bail!(local);
            }
        };
        ensure!(receiver.permitted(generation, policy), "{}", t().features.mapping_permission_expired);
        if open.proto == wire::proto::UDP {
            // Datagrams have no handshake to race: the first address is used.
            let target = targets[0];
            let socket = match async {
                let socket = tokio::net::UdpSocket::bind(if target.is_ipv4() {
                    "0.0.0.0:0"
                } else {
                    "[::]:0"
                })
                .await?;
                socket.connect(target).await?;
                Ok::<_, std::io::Error>(socket)
            }
            .await
            {
                Ok(socket) => socket,
                Err(error) => {
                    answer(connect_code(&error)).await?;
                    return Err(error.into());
                }
            };
            answer(wire::code::OK).await?;
            opened = true;
            tracing::debug!(stream = id, %target, "host UDP forwarding opened");
            let mut buffer = vec![0; 65_536];
            loop {
                ensure!(receiver.permitted(generation, policy), "{}", t().features.mapping_permission_expired);
                tokio::select! {
                    _ = tokio::time::sleep(super::udp::IDLE) => return Ok(()),
                    _ = stream.stop.cancelled() => return Ok(()),
                    packet = socket.recv(&mut buffer) => {
                        let count = match packet {
                            Ok(count) => count,
                            // An ICMP port-unreachable for an earlier datagram.
                            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => continue,
                            Err(error) => return Err(error.into()),
                        };
                        if count <= wire::DATAGRAM_LIMIT
                            && transport.send_datagram(id, Bytes::copy_from_slice(&buffer[..count])).await?
                        {
                            lock(&stats).sent += count as u64;
                        }
                    },
                    message = inbound.recv() => match message.context(t().features.mapping_udp_closed)? {
                        Inbound::Datagram(data) => {
                            Transport::datagram_taken(&stream);
                            let _ = socket.send(&data).await;
                            lock(&stats).received += data.len() as u64;
                        }
                        Inbound::Reset(_) => return Ok(()),
                        _ => anyhow::bail!("{}", t().features.mapping_udp_invalid_message),
                    },
                }
            }
        }
        let socket = tokio::select! {
            _ = stream.stop.cancelled() => return Ok(()),
            // Every address of a name is tried (`localhost` → `::1`, then
            // `127.0.0.1` for a service listening on IPv4 only).
            socket = tokio::time::timeout(CONNECT_TIMEOUT, crate::transport::happy_eyeballs::connect(&targets)) => match socket {
                Err(_) => {
                    answer(wire::code::TIMEOUT).await?;
                    anyhow::bail!("{}", t().features.mapping_target_timeout);
                }
                Ok(Err(error)) => {
                    answer(connect_code(&error)).await?;
                    return Err(error.into());
                }
                Ok(Ok(socket)) => socket,
            },
        };
        socket.set_nodelay(true)?;
        ensure!(receiver.permitted(generation, policy), "{}", t().features.mapping_permission_expired);
        answer(wire::code::OK).await?;
        opened = true;
        tracing::debug!(stream = id, target = ?socket.peer_addr().ok(), "host TCP forwarding opened");
        let relay = Mutex::new(RuleStatus::default());
        let result = pump(&transport, &stream, inbound, socket, &relay).await;
        let relay = lock(&relay);
        let mut stats = lock(&stats);
        stats.sent += relay.sent;
        stats.received += relay.received;
        result
    }
    .await;
    if let Err(error) = &result {
        if opened && receiver.generation.load(Ordering::Acquire) == generation {
            transport.reset(id, wire::code::CLOSED).await;
        }
        lock(&stats).error = Some(error.to_string());
    }
    tracing::debug!(stream = id, error = ?result.err().map(|e| e.to_string()), "host port forwarding closed");
    let mut stats = lock(&stats);
    stats.connections = stats.connections.saturating_sub(1);
}
