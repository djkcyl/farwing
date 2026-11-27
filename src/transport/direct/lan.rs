//! LAN discovery (`docs/protocol/v2-design.md` §12.4): hosts announce
//! themselves on an IPv4 multicast group and answer queries; controllers
//! query by multicast and broadcast and collect signed announcements.

#[cfg(windows)]
use super::LocalDevice;
#[cfg(windows)]
use crate::session::host_client::HostClient;
#[cfg(windows)]
use anyhow::Context;
use anyhow::Result;
use farwing_protocol::direct::{self as wire, lan};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
#[cfg(windows)]
use tokio_util::sync::CancellationToken;

/// How often the responder joins adapters that came up. It never announces
/// itself unasked: it only answers queries from local networks.
#[cfg(windows)]
const ADAPTER_INTERVAL: Duration = Duration::from_secs(30);

fn group() -> Ipv4Addr {
    let [a, b, c, d] = wire::DISCOVERY_GROUP;
    Ipv4Addr::new(a, b, c, d)
}

/// What a host announces, if it currently accepts connections.
#[cfg(windows)]
pub(crate) trait Announcer: Send + Sync {
    fn announcement(&self) -> Option<(LocalDevice, u16, Vec<String>)>;
}

/// Queries are answered only from private, link-local and loopback
/// addresses: discovery is for the local network.
#[cfg(windows)]
fn local_network(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

#[cfg(windows)]
fn announce(local: &LocalDevice, port: u16, methods: Vec<String>, nonce: &str) -> lan::Announce {
    let mut announce = lan::Announce {
        t: lan::ANNOUNCE.into(),
        v: farwing_protocol::PROTOCOL_VERSION,
        device_id: local.identity.device_id.clone(),
        name: local.name.clone(),
        platform: farwing_protocol::Platform::Windows,
        version: String::new(),
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
#[cfg(windows)]
pub(crate) struct Responder {
    cancel: CancellationToken,
}

#[cfg(windows)]
impl Drop for Responder {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(windows)]
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
            let mut tick = tokio::time::interval(ADAPTER_INTERVAL);
            let mut buffer = vec![0u8; 2048];
            let mut joined = Vec::new();
            loop {
                tokio::select! {
                    _ = task_cancel.cancelled() => break,
                    _ = tick.tick() => join_lan_adapters(&socket, &mut joined),
                    received = socket.recv_from(&mut buffer) => {
                        let Ok((length, from)) = received else { continue };
                        if !local_network(from.ip()) {
                            continue;
                        }
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

/// The group joined only on the default interface is not received on the
/// LAN when a VPN or proxy adapter holds the default route. Adapters that
/// come up later are joined on the next announcement.
#[cfg(windows)]
fn join_lan_adapters(socket: &UdpSocket, joined: &mut Vec<Ipv4Addr>) {
    let socket = socket2::SockRef::from(socket);
    for address in crate::platform::windows::device_profile::lan_ipv4_addresses() {
        if joined.contains(&address) {
            continue;
        }
        match socket.join_multicast_v4(&group(), &address) {
            Ok(()) => joined.push(address),
            Err(error) => tracing::debug!(%error, %address, "LAN discovery group not joined"),
        }
    }
}

#[cfg(windows)]
fn responder_socket() -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    // Several profiles on one machine share the discovery port.
    socket.set_reuse_address(true)?;
    socket.bind(&SockAddr::from(SocketAddrV4::new(
        Ipv4Addr::UNSPECIFIED,
        wire::DISCOVERY_PORT,
    )))
    .with_context(|| (crate::i18n::t().session.lan_discovery_port_failed)(wire::DISCOVERY_PORT))?;
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

fn query_socket(local: Ipv4Addr) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, SockAddr, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_broadcast(true)?;
    socket.set_multicast_ttl_v4(1)?;
    socket.set_multicast_loop_v4(true)?;
    if !local.is_unspecified() {
        socket.set_multicast_if_v4(&local)?;
    }
    socket.bind(&SockAddr::from(SocketAddrV4::new(local, 0)))?;
    socket.set_nonblocking(true)?;
    Ok(UdpSocket::from_std(socket.into())?)
}

/// Query the LAN on the default route and collect signed answers for `wait`.
/// On Windows, also send via each LAN adapter so the default route going
/// through a VPN does not hide the local route.
pub(crate) async fn discover(wait: Duration) -> Result<Vec<Found>> {
    let sockets = vec![(Arc::new(query_socket(Ipv4Addr::UNSPECIFIED)?), false)];
    #[cfg(windows)]
    let sockets = {
        let mut sockets = sockets;
        for address in crate::platform::windows::device_profile::lan_ipv4_addresses() {
            match query_socket(address) {
                Ok(socket) => sockets.push((Arc::new(socket), true)),
                Err(error) => tracing::debug!(%error, %address, "LAN discovery adapter unavailable"),
            }
        }
        sockets
    };
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let query = lan::Query {
        t: lan::QUERY.into(),
        v: farwing_protocol::PROTOCOL_VERSION,
        nonce: nonce.clone(),
    };
    let send_query = async |sockets: &[(Arc<UdpSocket>, bool)]| {
        for (socket, _) in sockets {
            for destination in [group(), Ipv4Addr::BROADCAST] {
                send(
                    socket,
                    &query,
                    SocketAddr::V4(SocketAddrV4::new(destination, wire::DISCOVERY_PORT)),
                )
                .await;
            }
        }
    };
    send_query(&sockets).await;
    let (answers, mut received) = tokio::sync::mpsc::channel::<(Vec<u8>, SocketAddr, bool)>(64);
    let mut readers = tokio::task::JoinSet::new();
    for (socket, lan) in &sockets {
        let (socket, lan, answers) = (socket.clone(), *lan, answers.clone());
        readers.spawn(async move {
            let mut buffer = vec![0u8; 2048];
            loop {
                let Ok((length, from)) = socket.recv_from(&mut buffer).await else { continue };
                if answers.send((buffer[..length].to_vec(), from, lan)).await.is_err() {
                    break;
                }
            }
        });
    }
    drop(answers);
    // Per host: what it answered, and whether that came over a LAN adapter.
    let mut found = HashMap::<String, (Found, bool)>::new();
    let deadline = tokio::time::Instant::now() + wait;
    let mut resent = false;
    loop {
        let (bytes, from, lan) = tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            // A lost query is sent once more halfway.
            _ = tokio::time::sleep(wait / 2), if !resent => {
                resent = true;
                send_query(&sockets).await;
                continue;
            }
            answer = received.recv() => match answer {
                Some(answer) => answer,
                None => break,
            },
        };
        let Ok(announce) = serde_json::from_slice::<lan::Announce>(&bytes) else {
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
        let key = announce.public_key.clone();
        let answer = (Found { announce, address }, lan);
        match found.get(&key) {
            Some((_, true)) => {}
            Some(_) if !lan => {}
            _ => {
                found.insert(key, answer);
            }
        }
    }
    readers.abort_all();
    let mut found: Vec<_> = found.into_values().map(|(found, _)| found).collect();
    found.sort_by(|a, b| a.announce.name.cmp(&b.announce.name));
    Ok(found)
}

#[cfg(all(test, windows))]
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
