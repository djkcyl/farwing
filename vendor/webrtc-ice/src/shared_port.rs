//! One UDP port per local address shared by many ICE agents (Farwing direct
//! sessions listen on a single UDP port). Datagrams are routed to the agent
//! that owns them:
//! - STUN responses by transaction ID (recorded when the agent sent the
//!   request, so a STUN server shared by agents is fine);
//! - a remote's first Binding request by the local ufrag in its USERNAME;
//! - everything else by the remote address the agent last exchanged with.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use util::Conn;

use crate::agent::agent_internal::AgentInternal;

type Socket = dyn Conn + Send + Sync;
type Datagram = (Vec<u8>, SocketAddr, Instant);

const QUEUE: usize = 2048;
const TRANSACTION_LIFETIME: Duration = Duration::from_secs(30);
const STUN_COOKIE: [u8; 4] = [0x21, 0x12, 0xa4, 0x42];

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// Who a lease belongs to, for routing a remote's first Binding request.
#[async_trait]
pub(crate) trait Owner: Send + Sync {
    async fn local_ufrag(&self) -> Option<String>;
}

#[async_trait]
impl Owner for Weak<AgentInternal> {
    async fn local_ufrag(&self) -> Option<String> {
        let agent = self.upgrade()?;
        let ufrag = agent.ufrag_pwd.lock().await.local_ufrag.clone();
        Some(ufrag)
    }
}

struct Route {
    queue: mpsc::Sender<Datagram>,
    owner: Box<dyn Owner>,
    closed: CancellationToken,
}

#[derive(Default)]
struct Routes {
    leases: Vec<Weak<Route>>,
    remotes: HashMap<SocketAddr, Weak<Route>>,
    transactions: HashMap<[u8; 12], (Weak<Route>, Instant)>,
}

struct Mux {
    socket: Arc<Socket>,
    routes: Mutex<Routes>,
    closed: CancellationToken,
}

static MUXES: Mutex<Vec<(SocketAddr, Weak<Mux>)>> = Mutex::new(Vec::new());

/// A STUN message: (class/method type, transaction ID).
fn stun(packet: &[u8]) -> Option<(u16, [u8; 12])> {
    if packet.len() < 20 || packet[0] & 0xc0 != 0 || packet[4..8] != STUN_COOKIE {
        return None;
    }
    let mut id = [0; 12];
    id.copy_from_slice(&packet[8..20]);
    Some((u16::from_be_bytes([packet[0], packet[1]]), id))
}

/// A request (class bits 00), as opposed to a response or indication.
fn is_request(kind: u16) -> bool {
    kind & 0x0110 == 0
}

/// The local ufrag of a Binding request's USERNAME ("local:remote").
fn username_ufrag(packet: &[u8]) -> Option<String> {
    let length = usize::from(u16::from_be_bytes([packet[2], packet[3]]));
    let end = (20 + length).min(packet.len());
    let mut at = 20;
    while at + 4 <= end {
        let kind = u16::from_be_bytes([packet[at], packet[at + 1]]);
        let size = usize::from(u16::from_be_bytes([packet[at + 2], packet[at + 3]]));
        let value = packet.get(at + 4..at + 4 + size)?;
        if kind == 0x0006 {
            let username = std::str::from_utf8(value).ok()?;
            return username.split(':').next().map(str::to_owned);
        }
        at += 4 + size.div_ceil(4) * 4;
    }
    None
}

impl Mux {
    fn deliver(route: &Route, datagram: Datagram) -> bool {
        if route.closed.is_cancelled() {
            return false;
        }
        // A full queue drops like a congested network would.
        let _ = route.queue.try_send(datagram);
        true
    }

    async fn route(&self, packet: &[u8], source: SocketAddr, at: Instant) {
        if let Some((kind, id)) = stun(packet) {
            if !is_request(kind) {
                let owner = lock(&self.routes).transactions.remove(&id);
                if let Some(route) = owner.and_then(|(route, _)| route.upgrade()) {
                    if Self::deliver(&route, (packet.to_vec(), source, at)) {
                        return;
                    }
                }
            } else if kind == 0x0001 && !lock(&self.routes).remotes.contains_key(&source) {
                if let Some(ufrag) = username_ufrag(packet) {
                    // A remote's first check: find the agent by its ufrag.
                    let leases: Vec<Arc<Route>> =
                        lock(&self.routes).leases.iter().filter_map(Weak::upgrade).collect();
                    for route in leases {
                        if route.owner.local_ufrag().await.as_deref() == Some(ufrag.as_str()) {
                            lock(&self.routes)
                                .remotes
                                .insert(source, Arc::downgrade(&route));
                            Self::deliver(&route, (packet.to_vec(), source, at));
                            return;
                        }
                    }
                }
            }
        }
        let owner = lock(&self.routes).remotes.get(&source).and_then(Weak::upgrade);
        match owner {
            Some(route) => {
                Self::deliver(&route, (packet.to_vec(), source, at));
            }
            None => log::debug!("shared UDP port: no session for {source}"),
        }
    }

    /// The lease `route` is sending `packet` to `target`.
    fn sent(&self, route: &Arc<Route>, packet: &[u8], target: SocketAddr) {
        let mut routes = lock(&self.routes);
        let request = stun(packet).filter(|(kind, _)| is_request(*kind));
        if let Some((kind, id)) = request {
            let now = Instant::now();
            if routes.transactions.len() > 4096 {
                routes
                    .transactions
                    .retain(|_, (route, at)| route.strong_count() > 0 && now - *at < TRANSACTION_LIFETIME);
            }
            routes.transactions.insert(id, (Arc::downgrade(route), now));
            // A STUN server answers by transaction; its address is not ours
            // alone. Connectivity checks go to remotes we also hear from.
            if kind != 0x0001 {
                return;
            }
        }
        routes.remotes.insert(target, Arc::downgrade(route));
    }

    fn release(&self, route: &Arc<Route>) {
        let mut routes = lock(&self.routes);
        routes
            .leases
            .retain(|lease| lease.upgrade().is_some_and(|lease| !Arc::ptr_eq(&lease, route)));
        routes
            .remotes
            .retain(|_, lease| lease.upgrade().is_some_and(|lease| !Arc::ptr_eq(&lease, route)));
        routes
            .transactions
            .retain(|_, (lease, _)| lease.upgrade().is_some_and(|lease| !Arc::ptr_eq(&lease, route)));
    }
}

/// One agent's view of the shared port.
pub(crate) struct Lease {
    mux: Arc<Mux>,
    route: Arc<Route>,
    queue: tokio::sync::Mutex<mpsc::Receiver<Datagram>>,
    local: SocketAddr,
}

/// Bind (or join) the shared UDP port at `address` for an agent.
pub(crate) async fn lease(
    net: &Arc<util::vnet::net::Net>,
    address: SocketAddr,
    owner: Box<dyn Owner>,
) -> Result<Arc<Socket>, util::Error> {
    let mux = {
        let existing = {
            let mut muxes = lock(&MUXES);
            muxes.retain(|(_, mux)| mux.strong_count() > 0);
            muxes
                .iter()
                .find(|(bound, _)| *bound == address)
                .and_then(|(_, mux)| mux.upgrade())
        };
        match existing {
            Some(mux) => mux,
            None => {
                let socket = net.bind(address).await?;
                crate::socket_options::configure_udp_media_buffers(socket.as_ref());
                let mux = Arc::new(Mux {
                    socket,
                    routes: Mutex::default(),
                    closed: CancellationToken::new(),
                });
                let reader = Arc::downgrade(&mux);
                let socket = mux.socket.clone();
                let closed = mux.closed.clone();
                tokio::spawn(async move {
                    let mut data = vec![0; 65_536];
                    loop {
                        let received = tokio::select! {
                            biased;
                            _ = closed.cancelled() => break,
                            result = socket.recv_from_with_timestamp(&mut data) => result,
                        };
                        let Some(mux) = reader.upgrade() else { break };
                        match received {
                            Ok((length, source, at)) => mux.route(&data[..length], source, at).await,
                            Err(error) => {
                                log::debug!("shared UDP port receive error: {error}");
                                tokio::task::yield_now().await;
                            }
                        }
                    }
                });
                lock(&MUXES).push((address, Arc::downgrade(&mux)));
                mux
            }
        }
    };
    let (sender, receiver) = mpsc::channel(QUEUE);
    let route = Arc::new(Route {
        queue: sender,
        owner,
        closed: CancellationToken::new(),
    });
    lock(&mux.routes).leases.push(Arc::downgrade(&route));
    let local = mux.socket.local_addr()?;
    Ok(Arc::new(Lease {
        mux,
        route,
        queue: tokio::sync::Mutex::new(receiver),
        local,
    }))
}

impl Drop for Mux {
    fn drop(&mut self) {
        self.closed.cancel();
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.route.closed.cancel();
        self.mux.release(&self.route);
    }
}

#[async_trait]
impl Conn for Lease {
    async fn connect(&self, _: SocketAddr) -> Result<(), util::Error> {
        Err(util::Error::Other("shared UDP port is connectionless".into()))
    }
    async fn recv(&self, buf: &mut [u8]) -> Result<usize, util::Error> {
        Ok(self.recv_from(buf).await?.0)
    }
    async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr), util::Error> {
        let (length, source, _) = self.recv_from_with_timestamp(buf).await?;
        Ok((length, source))
    }
    async fn recv_from_with_timestamp(
        &self,
        buf: &mut [u8],
    ) -> Result<(usize, SocketAddr, Instant), util::Error> {
        let mut queue = self.queue.lock().await;
        let received = tokio::select! {
            biased;
            _ = self.route.closed.cancelled() => None,
            datagram = queue.recv() => datagram,
        };
        let (packet, source, at) = received.ok_or(util::Error::ErrClosedListener)?;
        let length = packet.len().min(buf.len());
        buf[..length].copy_from_slice(&packet[..length]);
        Ok((length, source, at))
    }
    async fn send(&self, _: &[u8]) -> Result<usize, util::Error> {
        Err(util::Error::ErrNoRemAddr)
    }
    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> Result<usize, util::Error> {
        if self.route.closed.is_cancelled() {
            return Err(util::Error::ErrClosedListener);
        }
        self.mux.sent(&self.route, buf, target);
        self.mux.socket.send_to(buf, target).await
    }
    fn try_send_to(&self, buf: &[u8], target: SocketAddr) -> Result<usize, util::Error> {
        if self.route.closed.is_cancelled() {
            return Err(util::Error::ErrClosedListener);
        }
        self.mux.sent(&self.route, buf, target);
        self.mux.socket.try_send_to(buf, target)
    }
    fn local_addr(&self) -> Result<SocketAddr, util::Error> {
        Ok(self.local)
    }
    fn remote_addr(&self) -> Option<SocketAddr> {
        None
    }
    async fn close(&self) -> Result<(), util::Error> {
        self.route.closed.cancel();
        self.mux.release(&self.route);
        Ok(())
    }
    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Named(&'static str);
    #[async_trait]
    impl Owner for Named {
        async fn local_ufrag(&self) -> Option<String> {
            Some(self.0.to_owned())
        }
    }

    fn binding_request(id: u8, username: &str) -> Vec<u8> {
        let mut attribute = vec![0x00, 0x06];
        attribute.extend((username.len() as u16).to_be_bytes());
        attribute.extend(username.as_bytes());
        while attribute.len() % 4 != 0 {
            attribute.push(0);
        }
        let mut packet = vec![0x00, 0x01];
        packet.extend((attribute.len() as u16).to_be_bytes());
        packet.extend(STUN_COOKIE);
        packet.extend([id; 12]);
        packet.extend(attribute);
        packet
    }

    fn binding_response(id: u8) -> Vec<u8> {
        let mut packet = vec![0x01, 0x01, 0x00, 0x00];
        packet.extend(STUN_COOKIE);
        packet.extend([id; 12]);
        packet
    }

    async fn receive(lease: &Arc<Socket>) -> Option<(Vec<u8>, SocketAddr)> {
        let mut buf = vec![0; 1500];
        tokio::time::timeout(Duration::from_millis(300), lease.recv_from(&mut buf))
            .await
            .ok()?
            .ok()
            .map(|(length, source)| (buf[..length].to_vec(), source))
    }

    #[tokio::test]
    async fn sessions_share_one_port() {
        let net = Arc::new(util::vnet::net::Net::new(None));
        // Find a free port, then share it.
        let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = probe.local_addr().unwrap();
        drop(probe);
        let a = lease(&net, address, Box::new(Named("aaaa"))).await.unwrap();
        let b = lease(&net, address, Box::new(Named("bbbb"))).await.unwrap();
        assert_eq!(a.local_addr().unwrap(), b.local_addr().unwrap());

        let remote_a = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let remote_b = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // First contact from each remote: routed by the ufrag in USERNAME.
        remote_a.send_to(&binding_request(1, "aaaa:xa"), address).await.unwrap();
        remote_b.send_to(&binding_request(2, "bbbb:xb"), address).await.unwrap();
        let got_a = receive(&a).await.unwrap();
        let got_b = receive(&b).await.unwrap();
        assert_eq!(got_a.1, remote_a.local_addr().unwrap());
        assert_eq!(got_b.1, remote_b.local_addr().unwrap());
        assert!(receive(&a).await.is_none());
        // Afterwards by remote address (DTLS/SRTP carry no ufrag).
        remote_b.send_to(b"media for b", address).await.unwrap();
        assert_eq!(receive(&b).await.unwrap().0, b"media for b");
        // A STUN server shared by both: responses by transaction ID.
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_address = server.local_addr().unwrap();
        a.send_to(&binding_request(7, ""), server_address).await.unwrap();
        b.send_to(&binding_request(8, ""), server_address).await.unwrap();
        server.send_to(&binding_response(8), address).await.unwrap();
        server.send_to(&binding_response(7), address).await.unwrap();
        assert_eq!(receive(&b).await.unwrap().0, binding_response(8));
        assert_eq!(receive(&a).await.unwrap().0, binding_response(7));
        // Unknown senders are dropped; a closed lease no longer receives.
        let stranger = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        stranger.send_to(b"who", address).await.unwrap();
        assert!(receive(&a).await.is_none() && receive(&b).await.is_none());
        drop(b);
        remote_b.send_to(b"late", address).await.unwrap();
        assert!(receive(&a).await.is_none());
        // The port stays usable for a new session.
        let c = lease(&net, address, Box::new(Named("cccc"))).await.unwrap();
        remote_b.send_to(&binding_request(3, "cccc:xb"), address).await.unwrap();
        assert_eq!(receive(&c).await.unwrap().1, remote_b.local_addr().unwrap());
    }
}
