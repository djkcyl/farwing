//! LAN discovery (`docs/protocol/v2-design.md` §12.4): hosts announce
//! themselves on an IPv4 multicast group and answer queries; controllers
//! query by multicast and broadcast and collect signed announcements.

use super::LocalDevice;
use crate::session::host_client::HostClient;
use anyhow::{Context, Result};
use farwing_protocol::direct::{self as wire, lan};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

/// How often a host announces itself unasked.
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(30);

fn group() -> Ipv4Addr {
    let [a, b, c, d] = wire::DISCOVERY_GROUP;
    Ipv4Addr::new(a, b, c, d)
}

/// What a host announces, if it currently accepts connections.
pub(crate) trait Announcer: Send + Sync {
    fn announcement(&self) -> Option<(LocalDevice, u16, Vec<String>)>;
}

fn announce(local: &LocalDevice, port: u16, methods: Vec<String>, nonce: &str) -> lan::Announce {
    let mut announce = lan::Announce {
        t: lan::ANNOUNCE.into(),
        v: farwing_protocol::PROTOCOL_VERSION,
        device_id: local.identity.device_id.clone(),
        name: local.name.clone(),
        platform: farwing_protocol::Platform::Windows,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        public_key: local.identity.public_key(),
        port,
        udp_port: port,
        methods,
        ts: chrono::Utc::now().timestamp(),
        nonce: nonce.to_owned(),
        sig: String::new(),
    };
    announce.sig = local.identity.sign(&announce.signed_bytes());
    announce
}

/// A host's discovery socket and its announcements.
pub(crate) struct Responder {
    cancel: CancellationToken,
}

impl Drop for Responder {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Responder {
    pub(crate) fn start(
        _client: HostClient,
        source: Arc<dyn Announcer>,
        cancel: CancellationToken,
    ) -> Option<Self> {
        let socket = match responder_socket() {
            Ok(socket) => socket,
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "LAN discovery unavailable");
                return None;
            }
        };
        let task_cancel = cancel.clone();
        tokio::spawn(async move {
            let destination = SocketAddr::V4(SocketAddrV4::new(group(), wire::DISCOVERY_PORT));
            let mut tick = tokio::time::interval(ANNOUNCE_INTERVAL);
            let mut buffer = vec![0u8; 2048];
            loop {
                tokio::select! {
                    _ = task_cancel.cancelled() => break,
                    _ = tick.tick() => {
                        if let Some((local, port, methods)) = source.announcement() {
                            send(&socket, &announce(&local, port, methods, ""), destination).await;
                        }
                    }
                    received = socket.recv_from(&mut buffer) => {
                        let Ok((length, from)) = received else { continue };
                        let Ok(query) = serde_json::from_slice::<lan::Query>(&buffer[..length]) else { continue };
                        if query.t != lan::QUERY || query.nonce.len() > 64 {
                            continue;
                        }
                        if let Some((local, port, methods)) = source.announcement() {
                            send(&socket, &announce(&local, port, methods, &query.nonce), from).await;
                        }
                    }
                }
            }
        });
        tracing::info!(port = wire::DISCOVERY_PORT, "LAN discovery responder started");
        Some(Self { cancel })
    }
}

async fn send(socket: &UdpSocket, message: &impl serde::Serialize, to: SocketAddr) {
    let Ok(bytes) = serde_json::to_vec(message) else { return };
    if bytes.len() > wire::MAX_DISCOVERY_BYTES {
        tracing::warn!(bytes = bytes.len(), "LAN discovery message too large");
        return;
    }
    if let Err(error) = socket.send_to(&bytes, to).await {
        tracing::debug!(%error, %to, "LAN discovery send failed");
    }
}

fn responder_socket() -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // Several profiles on one machine share the discovery port.
    socket.set_reuse_address(true)?;
    socket.bind(&SockAddr::from(SocketAddrV4::new(
        Ipv4Addr::UNSPECIFIED,
        wire::DISCOVERY_PORT,
    )))
    .with_context(|| format!("无法监听局域网发现端口 {}", wire::DISCOVERY_PORT))?;
    socket.join_multicast_v4(&group(), &Ipv4Addr::UNSPECIFIED)?;
    socket.set_multicast_loop_v4(true)?;
    socket.set_multicast_ttl_v4(1)?;
    socket.set_nonblocking(true)?;
    Ok(UdpSocket::from_std(socket.into())?)
}

/// A host found on the LAN.
#[derive(Clone, Debug)]
pub(crate) struct Found {
    pub announce: lan::Announce,
    /// The address the announcement came from, with the endpoint's port.
    pub address: SocketAddr,
}

impl Found {
    pub(crate) fn target(&self) -> super::Target {
        super::Target {
            host: self.address.ip().to_string(),
            port: self.address.port(),
        }
    }
}

/// Query the LAN and collect answers for `wait`.
pub(crate) async fn discover(wait: Duration) -> Result<Vec<Found>> {
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_broadcast(true)?;
    socket.set_multicast_ttl_v4(1)?;
    socket.set_multicast_loop_v4(true)?;
    socket.bind(&SockAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)))?;
    socket.set_nonblocking(true)?;
    let socket = UdpSocket::from_std(socket.into())?;
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let query = lan::Query {
        t: lan::QUERY.into(),
        v: farwing_protocol::PROTOCOL_VERSION,
        nonce: nonce.clone(),
    };
    for destination in [group(), Ipv4Addr::BROADCAST] {
        send(
            &socket,
            &query,
            SocketAddr::V4(SocketAddrV4::new(destination, wire::DISCOVERY_PORT)),
        )
        .await;
    }
    let mut found = HashMap::<String, Found>::new();
    let deadline = tokio::time::Instant::now() + wait;
    let mut buffer = vec![0u8; 2048];
    let mut resent = false;
    loop {
        let received = tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            // A lost query is sent once more halfway.
            _ = tokio::time::sleep(wait / 2), if !resent => {
                resent = true;
                for destination in [group(), Ipv4Addr::BROADCAST] {
                    send(&socket, &query, SocketAddr::V4(SocketAddrV4::new(destination, wire::DISCOVERY_PORT))).await;
                }
                continue;
            }
            received = socket.recv_from(&mut buffer) => received,
        };
        let Ok((length, from)) = received else { continue };
        let Ok(announce) = serde_json::from_slice::<lan::Announce>(&buffer[..length]) else {
            continue;
        };
        if announce.t != lan::ANNOUNCE
            || announce.nonce != nonce
            || !farwing_protocol::ids::is_device_id(&announce.device_id)
            // The ID is the key's (`docs/access-model.md` §2.1).
            || farwing_protocol::crypto::local_device_id(&announce.public_key).as_deref()
                != Some(announce.device_id.as_str())
            || announce.port == 0
            || farwing_protocol::crypto::verify(&announce.public_key, &announce.signed_bytes(), &announce.sig)
                .is_err()
        {
            continue;
        }
        let mut announce = announce;
        announce.name = super::sanitize_name(&announce.name);
        let address = SocketAddr::new(from.ip(), announce.port);
        found
            .entry(announce.public_key.clone())
            .or_insert(Found { announce, address });
    }
    let mut found: Vec<_> = found.into_values().collect();
    found.sort_by(|a, b| a.announce.name.cmp(&b.announce.name));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announcements_are_signed() {
        let key = farwing_protocol::crypto::DeviceKey::generate();
        let local = LocalDevice::from_identity(
            crate::account::api::ClientIdentity::new("c", "", &key.seed()).unwrap(),
            "PC".into(),
        )
        .unwrap();
        let message = announce(&local, 28890, vec!["paired".into()], "n");
        assert!(serde_json::to_vec(&message).unwrap().len() <= wire::MAX_DISCOVERY_BYTES);
        farwing_protocol::crypto::verify(&message.public_key, &message.signed_bytes(), &message.sig)
            .unwrap();
        let mut forged = message.clone();
        forged.port = 1;
        assert!(
            farwing_protocol::crypto::verify(&forged.public_key, &forged.signed_bytes(), &forged.sig)
                .is_err()
        );
    }
}
