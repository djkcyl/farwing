//! AllocationSequence's UDP socket. One physical reader, inline host dispatch,
//! and source-specific TURN ingress.
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex as StdMutex, RwLock, Weak};

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use turn::client::{transaction::TransactionManager, ClientIngress};
use util::Conn;

use super::agent_internal::AgentInternal;
use super::local_port::LocalPort;

type Socket = dyn Conn + Send + Sync;

pub(super) struct SharedUdpSocket {
    socket: StdMutex<Option<Arc<Socket>>>,
    local: SocketAddr,
    closed: CancellationToken,
    reader: StdMutex<Option<JoinHandle<()>>>,
    close_serial: Mutex<()>,
    turns: Arc<RwLock<HashMap<SocketAddr, Vec<Weak<ClientIngress>>>>>,
    stun_servers: Arc<RwLock<HashSet<SocketAddr>>>,
    stun: StdMutex<Option<Arc<TransactionManager>>>,
}

impl SharedUdpSocket {
    pub fn new(socket: Arc<Socket>) -> Result<Arc<Self>, util::Error> {
        crate::socket_options::configure_udp_media_buffers(socket.as_ref());
        Ok(Arc::new(Self {
            local: socket.local_addr()?,
            socket: StdMutex::new(Some(socket)),
            closed: CancellationToken::new(),
            reader: StdMutex::new(None),
            close_serial: Mutex::new(()),
            turns: Arc::new(RwLock::new(HashMap::new())),
            stun_servers: Arc::new(RwLock::new(HashSet::new())),
            stun: StdMutex::new(None),
        }))
    }

    pub fn lease(self: &Arc<Self>) -> Arc<Socket> {
        Arc::new(UdpLease {
            socket: self.clone(),
            closed: self.closed.child_token(),
        })
    }

    pub fn add_turn(&self, ingress: &Arc<ClientIngress>) {
        let Some(remote) = ingress.remote_addr() else {
            return;
        };
        let mut turns = self.turns.write().expect("shared TURN routes poisoned");
        let routes = turns.entry(remote).or_default();
        routes.retain(|route| route.strong_count() != 0);
        routes.push(Arc::downgrade(ingress));
    }

    pub fn add_stun_server(&self, server: SocketAddr) {
        self.stun_servers
            .write()
            .expect("shared STUN servers poisoned")
            .insert(server);
    }

    pub fn start_reader(
        &self,
        agent: &Arc<AgentInternal>,
        port: &Arc<LocalPort>,
        stun: Arc<TransactionManager>,
    ) -> Result<(), util::Error> {
        let mut reader = self.reader.lock().expect("shared UDP reader poisoned");
        if reader.is_some() {
            return Err(util::Error::Other("shared UDP already reading".into()));
        }
        let socket = self
            .socket
            .lock()
            .expect("shared UDP socket poisoned")
            .clone()
            .ok_or(util::Error::ErrClosedListener)?;
        let closed = self.closed.clone();
        let turns = self.turns.clone();
        let stun_servers = self.stun_servers.clone();
        *self.stun.lock().expect("shared STUN manager poisoned") = Some(stun.clone());
        let agent = Arc::downgrade(agent);
        let port = Arc::downgrade(port);
        *reader = Some(tokio::spawn(async move {
            let mut data = vec![0; 65_556];
            let receive_packets = async {
                loop {
                    tokio::select! {
                        biased;
                        _ = closed.cancelled() => break,
                        result = socket.recv_from_with_timestamp(&mut data) => {
                            let (length, source, received_at) = match result {
                                Ok(packet) => packet,
                                Err(error) => {
                                    // UDP read errors are per-event, not
                                    // TurnPort's stream-close/allocation-failed path.
                                    log::debug!("shared UDP receive error: {error}");
                                    tokio::task::yield_now().await;
                                    continue;
                                }
                            };
                            let packet = &data[..length];
                        let routes = turns.read().expect("shared TURN routes poisoned")
                            .get(&source).into_iter().flatten().filter_map(Weak::upgrade).collect::<Vec<_>>();
                            let mut matched_turn = false;
                            let mut consumed = false;
                            for route in routes {
                                if !route.matches_source(source) { continue; }
                                matched_turn = true;
                                match route.handle_shared_packet(packet, source, received_at).await {
                                    Ok(false) => {},
                                    Ok(true) => { consumed = true; break; },
                                    Err(error) => {
                                        log::warn!("discarded malformed shared TURN packet: {error}");
                                        consumed = true; break;
                                    }
                                }
                            }
                            let from_stun = stun_servers.read().expect("shared STUN servers poisoned").contains(&source);
                            if consumed || (matched_turn && !from_stun) { continue; }
                            let Some(port) = port.upgrade().filter(|port| !port.closed.is_cancelled()) else { continue; };
                            if from_stun {
                            if let Ok(message) = turn::client::message::decode(packet, turn::client::message::Dialect::Stun) {
                                stun.handle_response(message, source);
                            }
                                continue;
                            }
                            if !port.ready.load(Ordering::Acquire) && packet == super::punch::REQUEST {
                                // The unactivated UDPPort can reply, but has
                                // not yet bound the P2P unknown-address callback.
                                let _ = socket.try_send_to(&super::punch::RESPONSE, source);
                                continue;
                            }
                            if port.ready.load(Ordering::Acquire) {
                                let Some(agent) = agent.upgrade() else { break; };
                                agent.handle_inbound_candidate_msg(
                                &port.canonical, packet, source, received_at,
                                ).await;
                            }
                        }
                    }
                }
            };
            tokio::select! {
                biased;
                _ = closed.cancelled() => {},
                _ = receive_packets => {},
            }
        }));
        Ok(())
    }
}

#[async_trait]
impl Conn for SharedUdpSocket {
    async fn connect(&self, _: SocketAddr) -> Result<(), util::Error> {
        Err(util::Error::Other("shared UDP is connectionless".into()))
    }
    async fn recv(&self, _: &mut [u8]) -> Result<usize, util::Error> {
        Err(util::Error::Other(
            "AllocationSequence owns shared UDP ingress".into(),
        ))
    }
    async fn recv_from(&self, _: &mut [u8]) -> Result<(usize, SocketAddr), util::Error> {
        Err(util::Error::Other(
            "AllocationSequence owns shared UDP ingress".into(),
        ))
    }
    async fn send(&self, _: &[u8]) -> Result<usize, util::Error> {
        Err(util::Error::ErrNoRemAddr)
    }
    async fn send_to(&self, packet: &[u8], target: SocketAddr) -> Result<usize, util::Error> {
        if self.closed.is_cancelled() {
            return Err(util::Error::ErrClosedListener);
        }
        let socket = self
            .socket
            .lock()
            .expect("shared UDP socket poisoned")
            .clone()
            .ok_or(util::Error::ErrClosedListener)?;
        tokio::select! {
            biased;
            _ = self.closed.cancelled() => Err(util::Error::ErrClosedListener),
            result = socket.send_to(packet, target) => result,
        }
    }
    fn try_send_to(&self, packet: &[u8], target: SocketAddr) -> Result<usize, util::Error> {
        if self.closed.is_cancelled() {
            return Err(util::Error::ErrClosedListener);
        }
        self.socket
            .lock()
            .expect("shared UDP socket poisoned")
            .as_ref()
            .ok_or(util::Error::ErrClosedListener)?
            .try_send_to(packet, target)
    }
    fn local_addr(&self) -> Result<SocketAddr, util::Error> {
        Ok(self.local)
    }
    fn remote_addr(&self) -> Option<SocketAddr> {
        None
    }
    async fn close(&self) -> Result<(), util::Error> {
        let _serial = self.close_serial.lock().await;
        self.closed.cancel();
        let task = self
            .reader
            .lock()
            .expect("shared UDP reader poisoned")
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
        let stun = self
            .stun
            .lock()
            .expect("shared STUN manager poisoned")
            .take();
        if let Some(stun) = stun {
            stun.close().await;
        }
        let socket = self
            .socket
            .lock()
            .expect("shared UDP socket poisoned")
            .take();
        if let Some(socket) = socket {
            socket.close().await?;
        }
        self.turns
            .write()
            .expect("shared TURN routes poisoned")
            .clear();
        Ok(())
    }
    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
}

impl Drop for SharedUdpSocket {
    fn drop(&mut self) {
        self.closed.cancel();
        if let Some(reader) = self
            .reader
            .get_mut()
            .expect("shared UDP reader poisoned")
            .take()
        {
            reader.abort();
        }
    }
}

struct UdpLease {
    socket: Arc<SharedUdpSocket>,
    closed: CancellationToken,
}

#[async_trait]
impl Conn for UdpLease {
    async fn connect(&self, address: SocketAddr) -> Result<(), util::Error> {
        self.socket.connect(address).await
    }
    async fn recv(&self, buf: &mut [u8]) -> Result<usize, util::Error> {
        self.socket.recv(buf).await
    }
    async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr), util::Error> {
        self.socket.recv_from(buf).await
    }
    async fn send(&self, buf: &[u8]) -> Result<usize, util::Error> {
        self.socket.send(buf).await
    }
    async fn send_to(&self, buf: &[u8], address: SocketAddr) -> Result<usize, util::Error> {
        if self.closed.is_cancelled() {
            return Err(util::Error::ErrClosedListener);
        }
        self.socket.send_to(buf, address).await
    }
    fn try_send_to(&self, buf: &[u8], address: SocketAddr) -> Result<usize, util::Error> {
        if self.closed.is_cancelled() {
            return Err(util::Error::ErrClosedListener);
        }
        self.socket.try_send_to(buf, address)
    }
    fn local_addr(&self) -> Result<SocketAddr, util::Error> {
        self.socket.local_addr()
    }
    fn remote_addr(&self) -> Option<SocketAddr> {
        None
    }
    async fn close(&self) -> Result<(), util::Error> {
        // Closing one TURN/UDP Port cannot shut down the Sequence's other Ports.
        self.closed.cancel();
        Ok(())
    }
    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
}
