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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
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

/// Validates time-limited TURN REST usernames.
pub struct RestAuthHandler {
    secret: String,
}

impl RestAuthHandler {
    pub fn new(secret: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
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
        let expiry_text = username.split_once(':').map_or(username, |(e, _)| e);
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

pub struct BuiltinTurn {
    udp: Server,
    udp_addr: SocketAddr,
    tcp_addr: Option<SocketAddr>,
    tcp_task: Option<JoinHandle<()>>,
    tcp_stop: Arc<Notify>,
}

impl BuiltinTurn {
    pub fn udp_addr(&self) -> SocketAddr {
        self.udp_addr
    }

    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        self.tcp_addr
    }

    pub async fn close(self) {
        self.tcp_stop.notify_waiters();
        if let Some(t) = self.tcp_task {
            t.abort();
        }
        let _ = self.udp.close().await;
    }
}

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

pub async fn start(cfg: &TurnConfig) -> Result<BuiltinTurn> {
    let auth = Arc::new(RestAuthHandler::new(cfg.shared_secret.clone()));
    let listen_udp = cfg.listen_udp.context("[turn] listen_udp missing")?;
    let udp = UdpSocket::bind(listen_udp)
        .await
        .with_context(|| format!("binding TURN UDP {listen_udp}"))?;
    let udp_addr = udp.local_addr()?;
    let server = new_server(cfg, Arc::new(udp), Arc::clone(&auth)).await?;
    tracing::info!(%udp_addr, "builtin TURN listening (UDP)");

    let tcp_stop = Arc::new(Notify::new());
    let (tcp_addr, tcp_task) = match cfg.listen_tcp {
        Some(addr) => {
            let listener = TcpListener::bind(addr)
                .await
                .with_context(|| format!("binding TURN TCP {addr}"))?;
            let local = listener.local_addr()?;
            tracing::info!(tcp_addr = %local, "builtin TURN listening (TCP)");
            let cfg = cfg.clone();
            let stop = Arc::clone(&tcp_stop);
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
                    let cfg = cfg.clone();
                    let auth = Arc::clone(&auth);
                    tokio::spawn(async move {
                        if let Err(e) = serve_tcp(stream, peer, &cfg, auth).await {
                            tracing::debug!(%peer, error = %e, "TURN TCP connection ended");
                        }
                    });
                }
            });
            (Some(local), Some(task))
        }
        None => (None, None),
    };
    Ok(BuiltinTurn {
        udp: server,
        udp_addr,
        tcp_addr,
        tcp_task,
        tcp_stop,
    })
}

/// One TURN server instance per TCP client connection (the webrtc-rs server is
/// packet oriented; [`TcpTurnConn`] frames STUN/ChannelData over the stream).
async fn serve_tcp(
    stream: TcpStream,
    peer: SocketAddr,
    cfg: &TurnConfig,
    auth: Arc<RestAuthHandler>,
) -> Result<()> {
    let conn = TcpTurnConn::from_stream(stream)?;
    debug_assert_eq!(conn.remote, peer);
    let server = new_server(cfg, Arc::clone(&conn) as Arc<dyn Conn + Send + Sync>, auth).await?;
    conn.wait_closed().await;
    let _ = server.close().await;
    Ok(())
}

/// Packet view of a TURN-over-TCP stream (RFC 5766 §2.1 / §11.5 framing).
pub struct TcpTurnConn {
    reader: Mutex<OwnedReadHalf>,
    writer: Mutex<OwnedWriteHalf>,
    local: SocketAddr,
    remote: SocketAddr,
    closed: AtomicBool,
    closed_notify: Notify,
}

impl TcpTurnConn {
    /// Wrap a connected TCP stream (server or client side).
    pub fn from_stream(stream: TcpStream) -> std::io::Result<Arc<Self>> {
        stream.set_nodelay(true).ok();
        let local = stream.local_addr()?;
        let remote = stream.peer_addr()?;
        let (r, w) = stream.into_split();
        Ok(Arc::new(TcpTurnConn {
            reader: Mutex::new(r),
            writer: Mutex::new(w),
            local,
            remote,
            closed: AtomicBool::new(false),
            closed_notify: Notify::new(),
        }))
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
impl Conn for TcpTurnConn {
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
    }
}
