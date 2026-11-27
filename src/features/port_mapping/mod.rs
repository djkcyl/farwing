//! Port forwarding v2 (`docs/protocol/v2-design.md` §9): TCP streams on the
//! tunnel channel with per-stream credit, UDP datagrams on the unreliable
//! dgram channel. Rules (local listener → target) exist only on the
//! controller; the host resolves and connects to whatever target an
//! authorized controller asks for.
use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{Notify, mpsc},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::{RTCDataChannel, data_channel_state::RTCDataChannelState};

#[cfg(windows)]
pub(crate) mod host;
pub(crate) mod service;
pub(crate) mod store;
mod udp;
pub(crate) mod ui;
pub(crate) mod wire;

use crate::transport::transfer::{Credit, INITIAL_WINDOW, Window};
use crate::i18n::t;
use wire::Body;

const TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes the dgram channel may have queued before datagrams are dropped.
const DATAGRAM_QUEUE: usize = 1024 * 1024;
/// Datagrams waiting for one association before new ones are dropped.
const DATAGRAM_BACKLOG: usize = 256;
const MAX_STREAMS: usize = 128;

/// What arrives for one stream.
pub(crate) enum Inbound {
    Opened(wire::OpenResult),
    Data(Bytes),
    ShutdownWrite,
    Reset(wire::Reset),
    Datagram(Bytes),
}

/// One forwarded connection or UDP association.
pub(crate) struct Stream {
    id: u32,
    inbound: mpsc::UnboundedSender<Inbound>,
    credit: Mutex<Credit>,
    credit_changed: Notify,
    window: Mutex<Window>,
    datagrams: AtomicUsize,
    stop: CancellationToken,
}

impl Stream {
    pub(crate) fn id(&self) -> u32 {
        self.id
    }
}

/// Removes the stream from the transport when its owner ends.
pub(crate) struct Registration {
    stream: Arc<Stream>,
    transport: Arc<Transport>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.stream.stop.cancel();
        lock(&self.transport.streams).remove(&self.stream.id);
    }
}

impl std::ops::Deref for Registration {
    type Target = Arc<Stream>;
    fn deref(&self) -> &Arc<Stream> {
        &self.stream
    }
}

pub(crate) struct Transport {
    tunnel: Mutex<Weak<RTCDataChannel>>,
    datagrams: Mutex<Weak<RTCDataChannel>>,
    streams: Mutex<HashMap<u32, Arc<Stream>>>,
    changed: Notify,
    serial: AtomicU32,
}
impl Default for Transport {
    fn default() -> Self {
        Self {
            tunnel: Mutex::new(Weak::new()),
            datagrams: Mutex::new(Weak::new()),
            streams: Mutex::default(),
            changed: Notify::new(),
            serial: AtomicU32::new(0),
        }
    }
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Transport {
    /// Attach the tunnel or dgram channel, by label.
    pub(crate) async fn bind(self: &Arc<Self>, channel: &Arc<RTCDataChannel>) {
        match channel.label() {
            crate::protocol::peer::channel::TUNNEL => {
                *lock(&self.tunnel) = Arc::downgrade(channel);
            }
            crate::protocol::peer::channel::DGRAM => {
                *lock(&self.datagrams) = Arc::downgrade(channel);
            }
            _ => return,
        }
        self.changed.notify_waiters();
    }
    /// Whether `channel` is one this transport uses.
    pub(crate) fn owns(&self, channel: &Arc<RTCDataChannel>) -> bool {
        let channel = Arc::downgrade(channel);
        lock(&self.tunnel).ptr_eq(&channel) || lock(&self.datagrams).ptr_eq(&channel)
    }
    pub(crate) fn wake(&self) {
        self.changed.notify_waiters();
        for stream in lock(&self.streams).values() {
            stream.credit_changed.notify_waiters();
        }
    }
    /// End every stream (the connection or its permission is gone).
    pub(crate) fn reset_all(&self) {
        for (_, stream) in lock(&self.streams).drain() {
            stream.stop.cancel();
            let _ = stream.inbound.send(Inbound::Reset(wire::Reset {
                stream: stream.id,
                code: wire::code::CLOSED,
            }));
        }
        self.wake();
    }
    pub(crate) fn close(&self) {
        *lock(&self.tunnel) = Weak::new();
        *lock(&self.datagrams) = Weak::new();
        self.reset_all();
    }
    pub(crate) fn ready(&self) -> bool {
        lock(&self.tunnel)
            .upgrade()
            .is_some_and(|c| c.ready_state() == RTCDataChannelState::Open)
    }
    pub(crate) async fn wait_ready(&self) -> Result<()> {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let wake = self.changed.notified();
                tokio::pin!(wake);
                wake.as_mut().enable();
                if self.ready() {
                    return;
                }
                wake.await;
            }
        })
        .await
        .context(t().features.mapping_channel_not_ready)
    }
    /// A new stream number for an `Open` (controller side).
    fn next_stream(&self) -> u32 {
        loop {
            let id = self.serial.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            if id != 0 && !lock(&self.streams).contains_key(&id) {
                return id;
            }
        }
    }
    /// Register stream `id`; its frames arrive on the returned receiver.
    pub(crate) fn register(
        self: &Arc<Self>,
        id: u32,
        stop: CancellationToken,
    ) -> Result<(Registration, mpsc::UnboundedReceiver<Inbound>)> {
        let (inbound, receiver) = mpsc::unbounded_channel();
        let stream = Arc::new(Stream {
            id,
            inbound,
            credit: Mutex::new(Credit::new(INITIAL_WINDOW)),
            credit_changed: Notify::new(),
            window: Mutex::new(Window::new()),
            datagrams: AtomicUsize::new(0),
            stop,
        });
        {
            let mut streams = lock(&self.streams);
            ensure!(streams.len() < MAX_STREAMS, "{}", t().features.mapping_connection_limit);
            ensure!(id != 0 && !streams.contains_key(&id), "{}", t().features.mapping_duplicate_stream);
            streams.insert(id, stream.clone());
        }
        Ok((
            Registration {
                stream,
                transport: Arc::clone(self),
            },
            receiver,
        ))
    }
    pub(crate) fn contains(&self, id: u32) -> bool {
        lock(&self.streams).contains_key(&id)
    }
    /// Handle a tunnel frame (`PORT` or `FLOW`). An `Open` is returned for
    /// the host to act on; everything else goes to its stream.
    pub(crate) fn receive(&self, bytes: &[u8]) -> Result<Option<wire::Open>> {
        if let Some(flow) = wire::decode_flow(bytes)? {
            if let Some(stream) = lock(&self.streams).get(&flow.stream)
                && lock(&stream.credit).grant(flow.credit)
            {
                stream.credit_changed.notify_waiters();
            }
            return Ok(None);
        }
        let Some(body) = wire::decode(bytes)? else {
            return Ok(None);
        };
        if let Body::Open(open) = body {
            return Ok(Some(open));
        }
        let Some(stream) = lock(&self.streams).get(&body.stream()).cloned() else {
            return Ok(None);
        };
        let inbound = match body {
            Body::Open(_) => unreachable!(),
            Body::OpenResult(result) => Inbound::Opened(result),
            Body::Data(data) => {
                let mut window = lock(&stream.window);
                let received = window.received();
                if let Err(error) = window.receive(received, data.data.len()) {
                    drop(window);
                    stream.stop.cancel();
                    tracing::debug!(stream = stream.id, error = %format!("{error:#}"), "port flow window violated");
                    let _ = stream.inbound.send(Inbound::Reset(wire::Reset {
                        stream: stream.id,
                        code: wire::code::PROTOCOL,
                    }));
                    return Ok(None);
                }
                Inbound::Data(data.data)
            }
            Body::ShutdownWrite(_) => Inbound::ShutdownWrite,
            Body::Reset(reset) => {
                stream.stop.cancel();
                Inbound::Reset(reset)
            }
        };
        let _ = stream.inbound.send(inbound);
        Ok(None)
    }
    /// Handle a `PORT_DGRAM` frame; an overloaded association drops it.
    pub(crate) fn receive_datagram(&self, bytes: &[u8]) -> Result<bool> {
        let Some(datagram) = wire::decode_datagram(bytes)? else {
            return Ok(false);
        };
        if let Some(stream) = lock(&self.streams).get(&datagram.stream)
            && stream.datagrams.fetch_add(1, Ordering::AcqRel) < DATAGRAM_BACKLOG
        {
            let _ = stream.inbound.send(Inbound::Datagram(datagram.data));
        } else if let Some(stream) = lock(&self.streams).get(&datagram.stream) {
            stream.datagrams.fetch_sub(1, Ordering::AcqRel);
        }
        Ok(true)
    }
    /// A datagram taken from the stream's queue.
    pub(crate) fn datagram_taken(stream: &Stream) {
        stream.datagrams.fetch_sub(1, Ordering::AcqRel);
    }
    /// Send a frame through the tunnel's outbox (at most
    /// [`crate::transport::outbox::BULK_QUEUE`] bytes queued in SCTP, so
    /// session frames stay ahead of forwarded data).
    async fn send_frame(&self, bytes: Vec<u8>) -> Result<()> {
        let channel = lock(&self.tunnel).upgrade().context(t().features.mapping_connection_closed)?;
        crate::transport::outbox::send(&channel, Bytes::from(bytes)).await
    }
    pub(crate) async fn send(&self, body: Body) -> Result<()> {
        self.send_frame(wire::encode(body)).await
    }
    /// End every stream on both sides: the peer is told before local state
    /// goes, so its listeners close their connections instead of stalling.
    pub(crate) async fn reset_all_remote(&self, code: i32) {
        let streams: Vec<u32> = lock(&self.streams).values().map(|s| s.id).collect();
        for stream in streams {
            self.reset(stream, code).await;
        }
        self.reset_all();
    }
    pub(crate) async fn reset(&self, stream: u32, code: i32) {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            self.send(Body::Reset(wire::Reset { stream, code })),
        )
        .await;
    }
    /// Send TCP data within the stream's credit.
    pub(crate) async fn send_data(&self, stream: &Stream, mut data: Bytes) -> Result<()> {
        while !data.is_empty() {
            let allowed = loop {
                let changed = stream.credit_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                ensure!(!stream.stop.is_cancelled(), "{}", t().features.mapping_closed);
                let available = lock(&stream.credit).available();
                if available > 0 {
                    break available;
                }
                tokio::select! {
                    _ = stream.stop.cancelled() => anyhow::bail!("{}", t().features.mapping_closed),
                    _ = changed => {},
                }
            };
            let size = data
                .len()
                .min(wire::DATA_LIMIT)
                .min(usize::try_from(allowed).unwrap_or(usize::MAX));
            let chunk = data.split_to(size);
            lock(&stream.credit).spend(chunk.len())?;
            self.send(Body::Data(wire::Data {
                stream: stream.id,
                data: chunk,
            }))
            .await?;
        }
        Ok(())
    }
    /// `len` received bytes were written out locally; grant more credit.
    pub(crate) async fn delivered(&self, stream: &Stream, len: usize) -> Result<()> {
        let credit = lock(&stream.window).deliver(len);
        if let Some(credit) = credit {
            self.send_frame(wire::encode_flow(stream.id, credit)).await?;
        }
        Ok(())
    }
    /// Send one UDP datagram, or drop it when the dgram channel is backed up.
    pub(crate) async fn send_datagram(&self, stream: u32, data: Bytes) -> Result<bool> {
        let channel = lock(&self.datagrams).upgrade().context(t().features.mapping_udp_not_connected)?;
        ensure!(
            channel.ready_state() == RTCDataChannelState::Open,
            "{}",
            t().features.mapping_udp_channel_closed
        );
        if channel.buffered_amount().await + data.len() > DATAGRAM_QUEUE {
            return Ok(false);
        }
        channel
            .send(&Bytes::from(wire::encode_datagram(stream, data)))
            .await?;
        Ok(true)
    }
    /// Open a stream to `host:port` and wait for the host's answer.
    async fn open(
        self: &Arc<Self>,
        proto: i32,
        host: &str,
        port: u16,
        stop: CancellationToken,
    ) -> Result<(Registration, mpsc::UnboundedReceiver<Inbound>)> {
        let id = self.next_stream();
        let (stream, mut inbound) = self.register(id, stop)?;
        self.send(Body::Open(wire::Open {
            stream: id,
            proto,
            host: host.to_owned(),
            port: u32::from(port),
        }))
        .await?;
        let answer = tokio::time::timeout(TIMEOUT, inbound.recv())
            .await
            .context(t().features.mapping_handshake_timeout)?
            .context(t().features.mapping_ended)?;
        match answer {
            Inbound::Opened(result) if result.code == wire::code::OK => Ok((stream, inbound)),
            Inbound::Opened(result) => anyhow::bail!(wire::describe(result.code)),
            Inbound::Reset(reset) => anyhow::bail!(wire::describe(reset.code)),
            _ => anyhow::bail!("{}", t().features.mapping_handshake_out_of_order),
        }
    }

    /// TCP-connect probe; `http` additionally checks for an HTTP server.
    async fn probe(self: &Arc<Self>, rule: &Rule, http: bool, parent: &CancellationToken) -> ProbeStatus {
        let cancel = parent.child_token();
        let started = std::time::Instant::now();
        let opened = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(anyhow::anyhow!("{}", t().features.mapping_probe_cancelled)),
            result = tokio::time::timeout(
                Duration::from_secs(5),
                self.open(wire::proto::TCP, &rule.target, rule.remote_port, cancel.clone()),
            ) => result.context(t().features.mapping_probe_timeout).and_then(|r| r),
        };
        let (stream, mut inbound) = match opened {
            Ok(value) => value,
            Err(error) => return ProbeStatus::Unreachable(error.to_string()),
        };
        let millis = started.elapsed().as_secs_f64() * 1000.0;
        let http = http
            && tokio::select! {
                biased;
                _ = cancel.cancelled() => false,
                result = tokio::time::timeout(Duration::from_secs(3), async {
                    let request = Bytes::from(format!(
                        "HEAD / HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
                        rule.target, rule.remote_port
                    ));
                    self.send_data(&stream, request).await?;
                    let mut prefix = Vec::new();
                    while let Some(message) = inbound.recv().await {
                        let Inbound::Data(data) = message else { break };
                        let remaining = 1024 - prefix.len();
                        prefix.extend_from_slice(&data[..remaining.min(data.len())]);
                        if prefix.contains(&b'\n') || prefix.len() == 1024 {
                            break;
                        }
                    }
                    Ok::<bool, anyhow::Error>(
                        prefix.len() >= 12
                            && (prefix.starts_with(b"HTTP/1.0 ") || prefix.starts_with(b"HTTP/1.1 "))
                            && prefix[9..12].iter().all(u8::is_ascii_digit),
                    )
                }) => matches!(result, Ok(Ok(true))),
            };
        self.reset(stream.id, wire::code::CLOSED).await;
        ProbeStatus::Reachable { millis, http }
    }

    /// Forward one accepted local TCP connection.
    async fn stream(
        self: Arc<Self>,
        rule: Rule,
        socket: TcpStream,
        parent: CancellationToken,
        stats: Arc<Mutex<RuleStatus>>,
    ) {
        let stop = parent.child_token();
        lock(&stats).connections += 1;
        let mut opened = None;
        let result = tokio::select! {
            biased;
            _ = stop.cancelled() => Ok(()),
            result = async {
                socket.set_nodelay(true)?;
                let (stream, inbound) = self
                    .open(wire::proto::TCP, &rule.target, rule.remote_port, stop.clone())
                    .await?;
                let id = stream.id;
                opened = Some(id);
                lock(&stats).error = None;
                pump(&self, &stream, inbound, socket, &stats).await
            } => result,
        };
        if let Some(id) = opened
            && result.is_err()
        {
            self.reset(id, wire::code::CLOSED).await;
        }
        let mut state = lock(&stats);
        state.connections = state.connections.saturating_sub(1);
        if !parent.is_cancelled()
            && let Err(error) = result
        {
            state.error = Some(error.to_string());
        }
    }
}

/// Relay one opened TCP stream in both directions; each direction ends on
/// its own (half-close), a reset ends both.
pub(crate) async fn pump(
    transport: &Transport,
    stream: &Stream,
    mut inbound: mpsc::UnboundedReceiver<Inbound>,
    socket: TcpStream,
    stats: &Mutex<RuleStatus>,
) -> Result<()> {
    let (mut reader, mut writer) = socket.into_split();
    let sending = async {
        let mut buffer = vec![0; wire::DATA_LIMIT];
        loop {
            let count = reader.read(&mut buffer).await?;
            if count == 0 {
                transport.send(Body::ShutdownWrite(stream.id)).await?;
                return Ok::<_, anyhow::Error>(());
            }
            transport
                .send_data(stream, Bytes::copy_from_slice(&buffer[..count]))
                .await?;
            lock(stats).sent += count as u64;
        }
    };
    let receiving = async {
        while let Some(message) = inbound.recv().await {
            match message {
                Inbound::Data(data) => {
                    writer.write_all(&data).await?;
                    lock(stats).received += data.len() as u64;
                    transport.delivered(stream, data.len()).await?;
                }
                Inbound::ShutdownWrite => {
                    writer.shutdown().await?;
                    return Ok(());
                }
                Inbound::Reset(reset) if reset.code == wire::code::CLOSED => {
                    anyhow::bail!("{}", t().features.mapping_peer_closed)
                }
                Inbound::Reset(reset) => anyhow::bail!(wire::describe(reset.code)),
                Inbound::Opened(_) | Inbound::Datagram(_) => anyhow::bail!("{}", t().features.mapping_duplicate_handshake),
            }
        }
        anyhow::bail!("{}", t().features.mapping_ended)
    };
    let (sent, received) = tokio::join!(
        async {
            tokio::select! {
                result = sending => {
                    if result.is_err() { stream.stop.cancel(); }
                    result
                },
                _ = stream.stop.cancelled() => Ok(()),
            }
        },
        async {
            tokio::select! {
                result = receiving => {
                    if result.is_err() { stream.stop.cancel(); }
                    result
                },
                _ = stream.stop.cancelled() => Ok(()),
            }
        }
    );
    sent.and(received)
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) enum Protocol {
    #[default]
    Tcp,
    Udp,
}
impl Protocol {
    pub(crate) fn label(self) -> &'static str {
        if self == Self::Tcp { "TCP" } else { "UDP" }
    }
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct Rule {
    pub id: u64,
    pub name: String,
    pub local_addr: IpAddr,
    pub local_port: u16,
    /// Host name or address, resolved on the host.
    pub target: String,
    pub remote_port: u16,
    pub enabled: bool,
    #[serde(default)]
    pub protocol: Protocol,
}
impl Rule {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(self.id != 0, "{}", t().features.mapping_invalid_rule_id);
        ensure!(
            !self.name.trim().is_empty() && self.name.encode_utf16().count() <= 32,
            "{}",
            t().features.mapping_name_length
        );
        ensure!(
            self.local_port != 0 && self.remote_port != 0,
            "{}",
            t().features.mapping_port_range
        );
        wire::Open {
            stream: 1,
            proto: wire::proto::TCP,
            host: self.target.clone(),
            port: u32::from(self.remote_port),
        }
        .validate()
        .context(t().features.mapping_target_format)?;
        Ok(())
    }
}
#[derive(Clone, Default)]
pub(crate) enum ProbeStatus {
    #[default]
    Unknown,
    Reachable {
        millis: f64,
        http: bool,
    },
    Unreachable(String),
}
#[derive(Clone, Default)]
pub(crate) struct RuleStatus {
    pub listening: bool,
    pub connections: usize,
    pub sent: u64,
    pub received: u64,
    pub error: Option<String>,
    pub probing: bool,
    pub probe: ProbeStatus,
    pub total_sent: u64,
    pub total_received: u64,
    pub send_rate: f64,
    pub receive_rate: f64,
}

pub(crate) async fn run_rule(
    rule: Rule,
    transport: Arc<Transport>,
    stop: CancellationToken,
    status: Arc<Mutex<RuleStatus>>,
    probe: Arc<Notify>,
) {
    if rule.protocol == Protocol::Udp {
        udp::run_rule(rule, transport, stop, status).await;
        return;
    }
    let result=async {
        rule.validate()?;
        let endpoint=SocketAddr::new(rule.local_addr,rule.local_port);
        let socket=if rule.local_addr.is_ipv4() {tokio::net::TcpSocket::new_v4()?} else {tokio::net::TcpSocket::new_v6()?};
        socket.bind(endpoint).with_context(||(t().features.mapping_listen_failed)(&endpoint.to_string()))?;
        let listener=socket.listen(128)?;
        lock(&status).listening=true;
        let mut streams=JoinSet::new();
        streams.spawn(probe_loop(rule.clone(),Arc::clone(&transport),stop.clone(),Arc::clone(&status),probe));
        let result=loop {
            tokio::select! {
                biased;
                _=stop.cancelled()=>break Ok(()),
                Some(result)=streams.join_next(), if !streams.is_empty()=>{if let Err(error)=result {tracing::warn!(%error,"TCP forwarding task failed");}},
                accepted=listener.accept()=>{
                    let (socket,_)=match accepted{Ok(value)=>value,Err(error)=>break Err(anyhow::Error::from(error))};
                    if streams.len() >= 65 { drop(socket); continue; }
                    streams.spawn(Arc::clone(&transport).stream(rule.clone(),socket,stop.clone(),Arc::clone(&status)));
                }
            }
        };
        drop(listener);
        stop.cancel();
        while streams.join_next().await.is_some() {}
        result
    }.await;
    let mut state = lock(&status);
    state.listening = false;
    if let Err(error) = result {
        state.error = Some(format!("{error:#}"));
    }
}

/// Periodic probes only test a TCP connection; a requested probe also
/// checks for an HTTP server.
async fn probe_loop(
    rule: Rule,
    transport: Arc<Transport>,
    stop: CancellationToken,
    status: Arc<Mutex<RuleStatus>>,
    requested: Arc<Notify>,
) {
    static LIMIT: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    let limit = LIMIT.get_or_init(|| tokio::sync::Semaphore::new(4));
    let mut http = false;
    loop {
        let permit = tokio::select! {biased;_=stop.cancelled()=>break,p=limit.acquire()=>p.expect("probe semaphore remains open")};
        lock(&status).probing = true;
        let outcome = transport.probe(&rule, http, &stop).await;
        {
            let mut status = lock(&status);
            status.probing = false;
            if !stop.is_cancelled() {
                status.probe = outcome;
            }
        }
        drop(permit);
        http = tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            _ = requested.notified() => true,
            _ = tokio::time::sleep(Duration::from_secs(30)) => false,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_targets_may_be_names_or_addresses() {
        let mut rule = Rule {
            id: 1,
            name: "nas".into(),
            local_addr: IpAddr::from([127, 0, 0, 1]),
            local_port: 8080,
            target: "nas.local".into(),
            remote_port: 80,
            enabled: true,
            protocol: Protocol::Tcp,
        };
        rule.validate().unwrap();
        rule.target = "192.168.1.10".into();
        rule.validate().unwrap();
        rule.target = "::1".into();
        rule.validate().unwrap();
        rule.target = "bad host".into();
        assert!(rule.validate().is_err());
    }

    #[test]
    fn stored_rules_with_ip_targets_still_load() {
        let json = r#"{"id":1,"name":"a","local_addr":"127.0.0.1","local_port":1,"target":"10.0.0.1","remote_port":2,"enabled":true}"#;
        let rule: Rule = serde_json::from_str(json).unwrap();
        assert_eq!(rule.target, "10.0.0.1");
    }

    #[tokio::test]
    async fn frames_route_to_their_stream_and_credit_is_enforced() {
        let transport = Arc::new(Transport::default());
        let (stream, mut inbound) = transport.register(5, CancellationToken::new()).unwrap();
        assert!(transport.register(5, CancellationToken::new()).is_err());
        let open = transport
            .receive(&wire::encode(Body::Open(wire::Open {
                stream: 9,
                proto: wire::proto::TCP,
                host: "localhost".into(),
                port: 22,
            })))
            .unwrap();
        assert_eq!(open.map(|o| o.stream), Some(9));
        transport
            .receive(&wire::encode(Body::Data(wire::Data {
                stream: 5,
                data: Bytes::from_static(b"abc"),
            })))
            .unwrap();
        assert!(matches!(inbound.recv().await, Some(Inbound::Data(d)) if &d[..] == b"abc"));
        transport
            .receive(&wire::encode_flow(5, INITIAL_WINDOW * 2))
            .unwrap();
        assert_eq!(lock(&stream.credit).available(), INITIAL_WINDOW * 2);
        drop(stream);
        assert!(!transport.contains(5));
    }
}
