//! Builtin STUN/TURN server (webrtc-rs `turn`), authenticated with TURN REST
//! credentials: `username = "<expiry>:<anything>"`,
//! `password = base64(HMAC-SHA1(shared_secret, username))` (coturn
//! `use-auth-secret` compatible).

use crate::config::TurnConfig;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::any::Any;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::Duration;
use turn::auth::{AuthHandler, generate_auth_key};
use turn::relay::RelayAddressGenerator;
use turn::relay::relay_range::RelayAddressGeneratorRanges;
use turn::relay::relay_static::RelayAddressGeneratorStatic;
use turn::server::Server;
use turn::server::config::{ConnConfig, ServerConfig};
use webrtc_util::Conn;
use webrtc_util::vnet::net::Net;

/// Validates time-limited TURN REST usernames: `<expiry>:<session>` on the
/// control node, `<expiry>:<session>:<node>` on edge node `node`.
pub struct RestAuthHandler {
    secret: String,
    node: Option<String>,
}

impl RestAuthHandler {
    pub fn new(secret: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
            node: None,
        }
    }

    /// An edge node's handler: only credentials naming `node`.
    pub fn for_node(secret: impl Into<String>, node: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
            node: Some(node.into()),
        }
    }
}

impl AuthHandler for RestAuthHandler {
    fn auth_handle(
        &self,
        username: &str,
        realm: &str,
        _src_addr: SocketAddr,
    ) -> Result<Vec<u8>, turn::Error> {
        let mut fields = username.split(':');
        let expiry_text = fields.next().unwrap_or_default();
        let named = fields.nth(1);
        if named != self.node.as_deref() {
            return Err(turn::Error::Other("credential for another relay".into()));
        }
        let expiry: i64 = expiry_text
            .parse()
            .map_err(|_| turn::Error::Other("malformed TURN username".into()))?;
        if expiry < crate::db::now_s() {
            return Err(turn::Error::Other("expired TURN credential".into()));
        }
        let password = crate::crypto::turn_password(&self.secret, username);
        Ok(generate_auth_key(username, realm, &password))
    }
}

/// What a relay carries now.
#[derive(Clone, Debug, Default)]
pub struct RelayStats {
    pub allocations: u32,
    /// Sessions with an allocation (from the credentials' usernames).
    pub sessions: Vec<String>,
    /// Bytes relayed so far by the current allocations.
    pub relayed_bytes: u64,
}

/// The servers of the TCP and TLS connections, for their statistics.
type StreamServers = Arc<parking_lot::Mutex<std::collections::HashMap<u64, Arc<Server>>>>;

pub struct BuiltinTurn {
    udp: Server,
    streams: StreamServers,
    udp_addr: SocketAddr,
    tcp_addr: Option<SocketAddr>,
    tls_addr: Option<SocketAddr>,
    stream_tasks: Vec<JoinHandle<()>>,
    stream_stop: Arc<Notify>,
}

impl BuiltinTurn {
    pub fn udp_addr(&self) -> SocketAddr {
        self.udp_addr
    }

    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        self.tcp_addr
    }

    pub fn tls_addr(&self) -> Option<SocketAddr> {
        self.tls_addr
    }

    pub async fn stats(&self) -> RelayStats {
        let streams: Vec<Arc<Server>> = self.streams.lock().values().cloned().collect();
        let mut infos = Vec::new();
        if let Ok(udp) = self.udp.get_allocations_info(None).await {
            infos.extend(udp.into_values());
        }
        for server in streams {
            if let Ok(stream) = server.get_allocations_info(None).await {
                infos.extend(stream.into_values());
            }
        }
        let mut sessions: Vec<String> = infos
            .iter()
            .filter_map(|i| i.username.split(':').nth(1).map(str::to_owned))
            .collect();
        sessions.sort();
        sessions.dedup();
        RelayStats {
            allocations: infos.len() as u32,
            sessions,
            relayed_bytes: infos.iter().map(|i| i.relayed_bytes as u64).sum(),
        }
    }

    /// Stop serving and release the ports (a restart binds them again).
    pub async fn close(self) {
        self.stream_stop.notify_waiters();
        for t in self.stream_tasks {
            t.abort();
            let _ = t.await;
        }
        // The TCP and TLS clients' connections end too: their credentials
        // and allocations belong to the relay that stops.
        let streams: Vec<Arc<Server>> = self.streams.lock().values().cloned().collect();
        for server in streams {
            let _ = server.close().await;
        }
        let _ = self.udp.close().await;
    }
}

/// How long a TURN-over-TLS client has to finish its handshake.
const TLS_HANDSHAKE: Duration = Duration::from_secs(10);

fn relay_generator(cfg: &TurnConfig) -> Box<dyn RelayAddressGenerator + Send + Sync> {
    let relay_ip: IpAddr = cfg
        .relay_ip
        .or(cfg.public_ip)
        .unwrap_or(IpAddr::from([127, 0, 0, 1]));
    let net = Arc::new(Net::new(None));
    match (cfg.relay_port_min, cfg.relay_port_max) {
        (Some(min_port), Some(max_port)) => Box::new(RelayAddressGeneratorRanges {
            relay_address: relay_ip,
            min_port,
            max_port,
            max_retries: 0,
            address: cfg.relay_bind.to_string(),
            net,
        }),
        _ => Box::new(RelayAddressGeneratorStatic {
            relay_address: relay_ip,
            address: cfg.relay_bind.to_string(),
            net,
        }),
    }
}

async fn new_server(
    cfg: &TurnConfig,
    conn: Arc<dyn Conn + Send + Sync>,
    auth: Arc<RestAuthHandler>,
) -> Result<Server> {
    Server::new(ServerConfig {
        conn_configs: vec![ConnConfig {
            conn,
            relay_addr_generator: relay_generator(cfg),
        }],
        realm: cfg.realm.clone(),
        auth_handler: auth,
        channel_bind_timeout: Duration::from_secs(0),
        alloc_close_notify: None,
    })
    .await
    .map_err(|e| anyhow::anyhow!("starting TURN server: {e}"))
}

/// Start the relay for `cfg`; TURN over TLS (`listen_tls`) only with `tls`,
/// the web certificate's configuration.
pub async fn start(
    cfg: &TurnConfig,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> Result<BuiltinTurn> {
    start_with(cfg, tls, RestAuthHandler::new(cfg.shared_secret.clone())).await
}

/// [`start`] with another credential check (an edge node's).
pub async fn start_with(
    cfg: &TurnConfig,
    tls: Option<Arc<rustls::ServerConfig>>,
    auth: RestAuthHandler,
) -> Result<BuiltinTurn> {
    let auth = Arc::new(auth);
    let listen_udp = cfg.listen_udp.context("turn.listen_udp missing")?;
    let udp = UdpSocket::bind(listen_udp)
        .await
        .with_context(|| format!("binding TURN UDP {listen_udp}"))?;
    let udp_addr = udp.local_addr()?;
    let server = new_server(cfg, Arc::new(udp), Arc::clone(&auth)).await?;
    tracing::info!(%udp_addr, "builtin TURN listening (UDP)");

    let stream_stop = Arc::new(Notify::new());
    let streams = StreamServers::default();
    let mut stream_tasks = Vec::new();
    let listen = |addr: SocketAddr, tls: Option<Arc<rustls::ServerConfig>>| {
        let cfg = cfg.clone();
        let auth = Arc::clone(&auth);
        let stop = Arc::clone(&stream_stop);
        let streams = Arc::clone(&streams);
        async move {
            let what = if tls.is_some() { "TLS" } else { "TCP" };
            let listener = TcpListener::bind(addr)
                .await
                .with_context(|| format!("binding TURN {what} {addr}"))?;
            let local = listener.local_addr()?;
            tracing::info!(addr = %local, "builtin TURN listening ({what})");
            let acceptor = tls.map(tokio_rustls::TlsAcceptor::from);
            let task = tokio::spawn(async move {
                loop {
                    let accepted = tokio::select! {
                        a = listener.accept() => a,
                        _ = stop.notified() => break,
                    };
                    let Ok((stream, peer)) = accepted else {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    };
                    let (cfg, auth, acceptor) = (cfg.clone(), Arc::clone(&auth), acceptor.clone());
                    let streams = Arc::clone(&streams);
                    tokio::spawn(async move {
                        if let Err(e) = serve_stream(stream, peer, acceptor, &cfg, auth, &streams).await {
                            tracing::debug!(%peer, error = %format!("{e:#}"), "TURN {what} connection ended");
                        }
                    });
                }
            });
            anyhow::Ok((local, task))
        }
    };
    let tcp_addr = match cfg.listen_tcp {
        Some(addr) => {
            let (local, task) = listen(addr, None).await?;
            stream_tasks.push(task);
            Some(local)
        }
        None => None,
    };
    let tls_addr = match (cfg.listen_tls, tls) {
        (Some(addr), Some(tls)) => match listen(addr, Some(tls)).await {
            Ok((local, task)) => {
                stream_tasks.push(task);
                Some(local)
            }
            Err(e) => {
                stream_stop.notify_waiters();
                for t in stream_tasks {
                    t.abort();
                }
                let _ = server.close().await;
                return Err(e);
            }
        },
        _ => None,
    };
    Ok(BuiltinTurn {
        udp: server,
        streams,
        udp_addr,
        tcp_addr,
        tls_addr,
        stream_tasks,
        stream_stop,
    })
}

/// One TURN server instance per TCP or TLS client connection (the webrtc-rs
/// server is packet oriented; [`StreamTurnConn`] frames STUN/ChannelData
/// over the stream).
async fn serve_stream(
    stream: TcpStream,
    peer: SocketAddr,
    tls: Option<tokio_rustls::TlsAcceptor>,
    cfg: &TurnConfig,
    auth: Arc<RestAuthHandler>,
    streams: &StreamServers,
) -> Result<()> {
    stream.set_nodelay(true).ok();
    let local = stream.local_addr()?;
    let conn = match tls {
        Some(acceptor) => {
            let stream = tokio::time::timeout(TLS_HANDSHAKE, acceptor.accept(stream))
                .await
                .context("TLS handshake timed out")?
                .context("TLS handshake")?;
            StreamTurnConn::new(stream, local, peer)
        }
        None => StreamTurnConn::new(stream, local, peer),
    };
    let server = Arc::new(
        new_server(cfg, Arc::clone(&conn) as Arc<dyn Conn + Send + Sync>, auth).await?,
    );
    let key = rand::random::<u64>();
    streams.lock().insert(key, Arc::clone(&server));
    conn.wait_closed().await;
    streams.lock().remove(&key);
    let _ = server.close().await;
    Ok(())
}

type BoxedStream = Box<dyn TurnStream>;
trait TurnStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> TurnStream for T {}

/// Packet view of a TURN stream over TCP or TLS (RFC 5766 §2.1 / §11.5
/// framing).
pub struct StreamTurnConn {
    reader: Mutex<ReadHalf<BoxedStream>>,
    writer: Mutex<WriteHalf<BoxedStream>>,
    local: SocketAddr,
    remote: SocketAddr,
    closed: AtomicBool,
    closed_notify: Notify,
}

impl StreamTurnConn {
    /// Wrap a connected stream (server or client side).
    pub fn new(
        stream: impl AsyncRead + AsyncWrite + Send + Unpin + 'static,
        local: SocketAddr,
        remote: SocketAddr,
    ) -> Arc<Self> {
        let (r, w) = tokio::io::split(Box::new(stream) as BoxedStream);
        Arc::new(StreamTurnConn {
            reader: Mutex::new(r),
            writer: Mutex::new(w),
            local,
            remote,
            closed: AtomicBool::new(false),
            closed_notify: Notify::new(),
        })
    }

    /// Wrap a connected TCP stream.
    pub fn from_tcp(stream: TcpStream) -> std::io::Result<Arc<Self>> {
        stream.set_nodelay(true).ok();
        let local = stream.local_addr()?;
        let remote = stream.peer_addr()?;
        Ok(Self::new(stream, local, remote))
    }

    fn mark_closed(&self) {
        if !self.closed.swap(true, Ordering::SeqCst) {
            self.closed_notify.notify_waiters();
        }
    }

    async fn wait_closed(&self) {
        loop {
            let notified = self.closed_notify.notified();
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    async fn read_frame(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut r = self.reader.lock().await;
        loop {
            let mut hdr = [0u8; 4];
            r.read_exact(&mut hdr).await?;
            let len = u16::from_be_bytes([hdr[2], hdr[3]]) as usize;
            let total = match hdr[0] >> 6 {
                0 => 20 + len,
                1 => 4 + len.div_ceil(4) * 4,
                _ => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "not a STUN or ChannelData frame",
                    ));
                }
            };
            let mut frame = vec![0u8; total];
            frame[..4].copy_from_slice(&hdr);
            r.read_exact(&mut frame[4..]).await?;
            if total <= buf.len() {
                buf[..total].copy_from_slice(&frame);
                return Ok(total);
            }
            // Oversized frame: drop it and keep reading.
        }
    }
}

#[async_trait]
impl Conn for StreamTurnConn {
    async fn connect(&self, _addr: SocketAddr) -> webrtc_util::Result<()> {
        Err(webrtc_util::Error::Other("connect not supported".into()))
    }

    async fn recv(&self, buf: &mut [u8]) -> webrtc_util::Result<usize> {
        self.recv_from(buf).await.map(|(n, _)| n)
    }

    async fn recv_from(&self, buf: &mut [u8]) -> webrtc_util::Result<(usize, SocketAddr)> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(webrtc_util::Error::Other("closed".into()));
        }
        let res = tokio::select! {
            r = self.read_frame(buf) => r,
            _ = self.wait_closed() => Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "closed")),
        };
        match res {
            Ok(n) => Ok((n, self.remote)),
            Err(e) => {
                self.mark_closed();
                Err(e.into())
            }
        }
    }

    async fn send(&self, buf: &[u8]) -> webrtc_util::Result<usize> {
        let mut w = self.writer.lock().await;
        match w.write_all(buf).await {
            Ok(()) => Ok(buf.len()),
            Err(e) => {
                self.mark_closed();
                Err(e.into())
            }
        }
    }

    async fn send_to(&self, buf: &[u8], _target: SocketAddr) -> webrtc_util::Result<usize> {
        self.send(buf).await
    }

    fn local_addr(&self) -> webrtc_util::Result<SocketAddr> {
        Ok(self.local)
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        Some(self.remote)
    }

    async fn close(&self) -> webrtc_util::Result<()> {
        self.mark_closed();
        let _ = self.writer.lock().await.shutdown().await;
        Ok(())
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_auth() {
        let h = RestAuthHandler::new("secret");
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let future = crate::db::now_s() + 60;
        let user = format!("{future}:session");
        let key = h.auth_handle(&user, "farwing", addr).unwrap();
        let expected = generate_auth_key(
            &user,
            "farwing",
            &crate::crypto::turn_password("secret", &user),
        );
        assert_eq!(key, expected);
        let past = format!("{}:session", crate::db::now_s() - 1);
        assert!(h.auth_handle(&past, "farwing", addr).is_err());
        assert!(h.auth_handle("nonsense", "farwing", addr).is_err());
        // Credentials naming a node are an edge node's, and only that one's.
        let node_user = format!("{future}:session:n1");
        assert!(h.auth_handle(&node_user, "farwing", addr).is_err());
        let node = RestAuthHandler::for_node("k1", "n1");
        assert!(node.auth_handle(&node_user, "farwing", addr).is_ok());
        assert!(node.auth_handle(&user, "farwing", addr).is_err());
        assert!(node.auth_handle(&format!("{future}:session:n2"), "farwing", addr).is_err());
    }
}
