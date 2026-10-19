//! TCPPort/TCPConnection and its two-byte
//! packet framing. This is not TURN/TCP framing.
use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use async_trait::async_trait;
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use util::Conn;

use super::{
    agent_internal::AgentInternal, local_port::LocalPort, network_catalog::NetworkCatalog,
};
use crate::util::LocalInterface;

#[derive(Default)]
struct Registry {
    current: HashMap<SocketAddr, Weak<TcpConnection>>,
    incoming: Vec<Arc<TcpConnection>>,
    logical: Weak<LocalPort>,
}

pub(crate) struct TcpPort {
    this: Weak<Self>,
    interface: LocalInterface,
    catalog: Arc<Mutex<NetworkCatalog>>,
    address: SocketAddr,
    listener: StdMutex<Option<Arc<TcpListener>>>,
    agent: Weak<AgentInternal>,
    wake: mpsc::Sender<bool>,
    registry: StdMutex<Registry>,
    stop: CancellationToken,
    tasks: TaskTracker,
    close_serial: Mutex<bool>,
}

impl TcpPort {
    pub(super) fn new(
        interface: LocalInterface,
        catalog: Arc<Mutex<NetworkCatalog>>,
        agent: &Arc<AgentInternal>,
        min_port: u16,
        max_port: u16,
    ) -> Arc<Self> {
        let listen = || -> io::Result<TcpListener> {
            let socket = if interface.ip.is_ipv4() {
                TcpSocket::new_v4()?
            } else {
                TcpSocket::new_v6()?
            };
            crate::socket_options::configure_media_buffers(&socket2::SockRef::from(&socket));
            if min_port == 0 && max_port == 0 {
                socket.bind(SocketAddr::new(interface.ip, 0))?;
            } else {
                let mut bound = false;
                let mut last_error = None;
                for port in min_port..=max_port {
                    match socket.bind(SocketAddr::new(interface.ip, port)) {
                        Ok(()) => {
                            bound = true;
                            break;
                        }
                        Err(error) => last_error = Some(error),
                    }
                }
                if !bound {
                    return Err(
                        last_error.unwrap_or_else(|| io::Error::other("empty TCP port range"))
                    );
                }
            }
            socket.listen(5)
        };
        let listener = match listen() {
            Ok(listener) => Some(Arc::new(listener)),
            Err(error) => {
                // Explicitly publishes active:9 if no listener exists.
                log::warn!(
                    "TCP listener unavailable on {}: {error}; active candidate only",
                    interface.ip
                );
                None
            }
        };
        let address = listener
            .as_ref()
            .and_then(|listener| listener.local_addr().ok())
            .unwrap_or(SocketAddr::new(interface.ip, 9));
        Arc::new_cyclic(|this| Self {
            this: this.clone(),
            interface,
            catalog,
            address,
            listener: StdMutex::new(listener),
            agent: Arc::downgrade(agent),
            wake: agent.force_candidate_contact_tx.clone(),
            registry: StdMutex::new(Registry::default()),
            stop: CancellationToken::new(),
            tasks: TaskTracker::new(),
            close_serial: Mutex::new(false),
        })
    }

    pub(super) fn passive(&self) -> bool {
        self.listener
            .lock()
            .expect("TCP listener poisoned")
            .is_some()
    }

    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        let _registry = self.registry.lock().expect("TCP registry poisoned");
        if self.stop.is_cancelled() {
            return;
        }
        let stop = self.stop.clone();
        self.tasks.spawn(async move {
            tokio::select! { biased; _ = stop.cancelled() => {}, _ = future => {} }
        });
    }

    pub(super) fn start(&self, logical: &Arc<LocalPort>) {
        self.registry.lock().expect("TCP registry poisoned").logical = Arc::downgrade(logical);
        let Some(listener) = self.listener.lock().expect("TCP listener poisoned").clone() else {
            return;
        };
        let weak = self.this.clone();
        self.spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((socket, remote)) => {
                        let Some(port) = weak.upgrade() else {
                            return;
                        };
                        if let Err(error) = socket.set_nodelay(true) {
                            log::warn!("TCP accepted socket NODELAY: {error}");
                        }
                        let connection = TcpConnection::new(&port, remote, false);
                        port.registry
                            .lock()
                            .expect("TCP registry poisoned")
                            .incoming
                            .push(connection.clone());
                        connection.install(socket);
                    }
                    Err(error) => {
                        log::warn!("TCP accept failed: {error}");
                        tokio::task::yield_now().await;
                    }
                }
            }
        });
    }

    pub(crate) fn create_connection(&self, remote: SocketAddr) -> io::Result<Arc<TcpConnection>> {
        if self.stop.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "TCP Port closed",
            ));
        }
        let port = self
            .this
            .upgrade()
            .ok_or_else(|| io::Error::other("TCP Port released"))?;
        let mut registry = self.registry.lock().expect("TCP registry poisoned");
        let connection = if let Some(index) = registry
            .incoming
            .iter()
            .position(|p| p.remote == remote && !p.stop.is_cancelled())
        {
            registry.incoming.remove(index)
        } else {
            TcpConnection::new(&port, remote, true)
        };
        connection.claimed.store(true, Ordering::Release);
        registry.current.insert(remote, Arc::downgrade(&connection));
        drop(registry);
        if connection.outgoing {
            connection.connect();
        }
        Ok(connection)
    }

    fn find(&self, remote: SocketAddr) -> Option<Arc<TcpConnection>> {
        let registry = self.registry.lock().expect("TCP registry poisoned");
        registry
            .current
            .get(&remote)
            .and_then(Weak::upgrade)
            .or_else(|| {
                registry
                    .incoming
                    .iter()
                    .find(|p| p.remote == remote)
                    .cloned()
            })
    }

    fn remove(&self, connection: &TcpConnection) {
        let mut registry = self.registry.lock().expect("TCP registry poisoned");
        registry
            .incoming
            .retain(|p| !std::ptr::eq(&**p, connection));
        if registry
            .current
            .get(&connection.remote)
            .is_some_and(|p| std::ptr::eq(p.as_ptr(), connection))
        {
            registry.current.remove(&connection.remote);
        }
    }

    async fn dispatch(
        &self,
        connection: &TcpConnection,
        bytes: &[u8],
        received_at: std::time::Instant,
    ) {
        let logical = {
            let registry = self.registry.lock().expect("TCP registry poisoned");
            if connection.claimed.load(Ordering::Acquire)
                && !registry
                    .current
                    .get(&connection.remote)
                    .is_some_and(|p| std::ptr::eq(p.as_ptr(), connection))
            {
                return;
            }
            registry.logical.upgrade()
        };
        let Some(logical) = logical else {
            return;
        };
        if logical.closed.is_cancelled() || !logical.ready.load(Ordering::Acquire) {
            return;
        }
        if let Some(agent) = self.agent.upgrade() {
            agent
                .handle_inbound_candidate_msg(
                    &logical.canonical,
                    bytes,
                    connection.remote,
                    received_at,
                )
                .await;
        }
    }
}

impl Drop for TcpPort {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[derive(Default)]
struct Output {
    frame: Vec<u8>,
    offset: usize,
    writable_interest: bool,
}

impl Output {
    fn flush(&mut self, socket: &TcpStream) -> io::Result<usize> {
        let start = self.offset;
        self.writable_interest = false;
        while self.offset < self.frame.len() {
            match socket.try_write(&self.frame[self.offset..]) {
                Ok(0) => return Ok(self.offset - start),
                Ok(n) => {
                    self.offset += n;
                    if self.offset < self.frame.len() {
                        self.writable_interest = true;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.writable_interest = true;
                    return Ok(self.offset - start);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error), // PhysicalSocket::Send does not synthesize close.
            }
        }
        let written = self.offset - start;
        self.frame.clear();
        self.offset = 0;
        Ok(written)
    }
}

pub(crate) struct TcpConnection {
    port: Weak<TcpPort>,
    remote: SocketAddr,
    outgoing: bool,
    claimed: AtomicBool,
    connected: AtomicBool,
    connecting: AtomicBool,
    grace: AtomicBool,
    failed: AtomicBool,
    stop: CancellationToken,
    socket: StdMutex<Option<Arc<TcpStream>>>,
    output: StdMutex<Output>,
    write_changed: Notify,
}

impl TcpConnection {
    fn new(port: &Arc<TcpPort>, remote: SocketAddr, outgoing: bool) -> Arc<Self> {
        Arc::new(Self {
            port: Arc::downgrade(port),
            remote,
            outgoing,
            claimed: AtomicBool::new(false),
            connected: AtomicBool::new(!outgoing),
            connecting: AtomicBool::new(false),
            grace: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            stop: port.stop.child_token(),
            socket: StdMutex::new(None),
            output: StdMutex::new(Output::default()),
            write_changed: Notify::new(),
        })
    }

    pub(crate) fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire) && !self.stop.is_cancelled()
    }
    pub(crate) fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
    pub(crate) fn media_ready(&self) -> bool {
        self.connected() && !self.grace.load(Ordering::Acquire)
    }
    pub(crate) fn note_binding_success(&self) {
        if self.grace.swap(false, Ordering::AcqRel) {
            self.wake();
        }
    }
    fn wake(&self) {
        if let Some(port) = self.port.upgrade() {
            let _ = port.wake.try_send(true);
        }
    }

    fn connect(self: &Arc<Self>) {
        if !self.outgoing
            || self.stop.is_cancelled()
            || self.connecting.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let Some(port) = self.port.upgrade() else {
            return;
        };
        let peer = self.clone();
        let ip = port.interface.ip;
        let catalog = port.catalog.clone();
        let key = port.interface.network_key.clone();
        port.spawn(async move {
            let result = tokio::select! {
                biased;
                _ = peer.stop.cancelled() => return,
                result = async {
                    let socket = if ip.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
                    crate::socket_options::configure_media_buffers(&socket2::SockRef::from(&socket));
                    if let Err(error) = socket.bind(SocketAddr::new(ip, 0)) {
                        if !ip.is_unspecified() { return Err(error); }
                        log::warn!("TCP any-address bind failed: {error}; allowing OS implicit bind");
                    }
                    let socket = socket.connect(peer.remote).await?;
                    if let Err(error) = socket.set_nodelay(true) { log::warn!("TCP NODELAY failed: {error}"); }
                    let actual = socket.local_addr()?.ip();
                    if !ip.is_unspecified() && !actual.is_loopback() && !catalog.lock().await.contains_address(&key, actual) {
                        return Err(io::Error::new(io::ErrorKind::AddrNotAvailable, "TCP socket bound outside its Network"));
                    }
                    Ok::<_, io::Error>(socket)
                } => result,
            };
            peer.connecting.store(false, Ordering::Release);
            match result {
                Ok(socket) => { peer.install(socket); peer.wake(); }
                Err(error) => {
                    log::debug!("TCP connection to {} failed: {error}", peer.remote);
                    peer.on_closed();
                }
            }
        });
    }

    fn install(self: &Arc<Self>, socket: TcpStream) {
        let Some(port) = self.port.upgrade() else {
            return;
        };
        // Includes accepted sockets and all reconnects, before their reader starts.
        crate::socket_options::configure_media_buffers(&socket2::SockRef::from(&socket));
        let socket = Arc::new(socket);
        *self.socket.lock().expect("TCP socket poisoned") = Some(socket.clone());
        *self.output.lock().expect("TCP output poisoned") = Output::default();
        self.connected.store(true, Ordering::Release);
        let peer = self.clone();
        port.spawn(async move {
            let result = tokio::select! {
                biased;
                _ = peer.stop.cancelled() => return,
                result = peer.read_write(&socket) => result,
            };
            if let Err(error) = result {
                log::debug!("TCP socket {} ended: {error}", peer.remote);
            }
            peer.on_closed();
        });
    }

    async fn read_write(&self, socket: &TcpStream) -> io::Result<()> {
        let mut input = vec![0; 65_538];
        let mut used = 0;
        loop {
            let wants_write = self
                .output
                .lock()
                .expect("TCP output poisoned")
                .writable_interest;
            tokio::select! {
                _ = self.write_changed.notified() => {},
                ready = socket.writable(), if wants_write => {
                    ready?;
                    let mut output = self.output.lock().expect("TCP output poisoned");
                    if let Err(error) = output.flush(socket) { log::debug!("TCP pending write failed: {error}"); }
                    if output.frame.is_empty() { self.wake(); }
                },
                ready = socket.readable() => {
                    ready?;
                    match socket.try_read(&mut input[used..]) {
                        Ok(0) => return Ok(()),
                        Ok(n) => used += n,
                        Err(error) if matches!(error.kind(),io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => continue,
                        Err(error) => return Err(error),
                    }
                    let mut consumed = 0;
                    while used - consumed >= 2 {
                        let size = usize::from(u16::from_be_bytes([input[consumed],input[consumed+1]]));
                        if used - consumed < size + 2 { break; }
                        // Timestamps each complete framed packet,
                        // before Connection/ICE callbacks and their queues.
                        let received_at = std::time::Instant::now();
                        if let Some(port) = self.port.upgrade() { port.dispatch(self, &input[consumed+2..consumed+2+size], received_at).await; }
                        consumed += size + 2;
                    }
                    input.copy_within(consumed..used,0); used -= consumed;
                },
            }
        }
    }

    fn on_closed(self: &Arc<Self>) {
        if self.stop.is_cancelled() {
            return;
        }
        if !self.claimed.load(Ordering::Acquire) {
            self.release();
            return;
        }
        if self.connected.swap(false, Ordering::AcqRel) {
            self.grace.store(true, Ordering::Release);
            if let Some(port) = self.port.upgrade() {
                let peer = self.clone();
                port.spawn(async move {
                    tokio::select! { biased; _ = peer.stop.cancelled() => return, _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {} }
                    if peer.grace.load(Ordering::Acquire) { peer.failed.store(true, Ordering::Release); peer.wake(); }
                });
            }
        } else if !self.grace.load(Ordering::Acquire) {
            self.failed.store(true, Ordering::Release);
        }
        self.wake();
    }

    pub(crate) fn send(self: &Arc<Self>, payload: &[u8]) -> io::Result<usize> {
        if !self.connected() {
            self.connect(); // Send-triggered reconnect, not a timer dial loop.
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "TCP connection not ready",
            ));
        }
        let size = u16::try_from(payload.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "TCP packet exceeds 16-bit length",
            )
        })?;
        let socket = self
            .socket
            .lock()
            .expect("TCP socket poisoned")
            .clone()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "TCP socket absent"))?;
        let mut output = self.output.lock().expect("TCP output poisoned");
        // Drops the new packet while the previous partial frame remains,
        // reporting its size. Never append an unbounded queue of media packets.
        if !output.frame.is_empty() {
            return Ok(payload.len());
        }
        output.frame.extend_from_slice(&size.to_be_bytes());
        output.frame.extend_from_slice(payload);
        output.offset = 0;
        match output.flush(&socket) {
            Ok(0) => {
                output.frame.clear();
                output.offset = 0;
                self.write_changed.notify_one();
                Ok(0)
            }
            Ok(_) => {
                self.write_changed.notify_one();
                Ok(payload.len())
            }
            Err(error) => {
                output.frame.clear();
                output.offset = 0;
                Err(error)
            }
        }
    }

    pub(crate) fn release(&self) {
        self.stop.cancel();
        self.socket.lock().expect("TCP socket poisoned").take();
        if let Some(port) = self.port.upgrade() {
            port.remove(self);
        }
        // A Connection may be removed from its own read callback. Port owns
        // and joins all tasks; do not join this reader from inside itself.
    }
}

impl Drop for TcpConnection {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[async_trait]
impl Conn for TcpPort {
    async fn connect(&self, _: SocketAddr) -> util::Result<()> {
        Err(io::Error::other("TCP Port owns per-peer connections").into())
    }
    async fn recv(&self, _: &mut [u8]) -> util::Result<usize> {
        Err(io::Error::other("TCP Port dispatches inline").into())
    }
    async fn recv_from(&self, _: &mut [u8]) -> util::Result<(usize, SocketAddr)> {
        Err(io::Error::other("TCP Port dispatches inline").into())
    }
    async fn send(&self, _: &[u8]) -> util::Result<usize> {
        Err(io::Error::other("TCP Port requires peer address").into())
    }
    async fn send_to(&self, data: &[u8], target: SocketAddr) -> util::Result<usize> {
        self.find(target)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "TCP peer absent"))?
            .send(data)
            .map_err(Into::into)
    }
    fn local_addr(&self) -> util::Result<SocketAddr> {
        Ok(self.address)
    }
    fn remote_addr(&self) -> Option<SocketAddr> {
        None
    }
    async fn close(&self) -> util::Result<()> {
        let mut joined = self.close_serial.lock().await;
        if *joined {
            return Ok(());
        }
        let peers = {
            let registry = self.registry.lock().expect("TCP registry poisoned");
            self.stop.cancel();
            self.tasks.close();
            registry
                .current
                .values()
                .filter_map(Weak::upgrade)
                .chain(registry.incoming.iter().cloned())
                .collect::<Vec<_>>()
        };
        for peer in peers {
            peer.release();
        }
        self.tasks.wait().await;
        self.listener.lock().expect("TCP listener poisoned").take();
        let mut registry = self.registry.lock().expect("TCP registry poisoned");
        registry.current.clear();
        registry.incoming.clear();
        *joined = true;
        log::debug!(
            "TCP Port listener/connection tasks joined: {}",
            self.address
        );
        Ok(())
    }
    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
}
