#[cfg(test)]
mod util_test;

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use stun::agent::*;
use stun::attributes::*;
use stun::message::*;
use stun::textattrs::*;
use stun::xoraddr::*;
use tokio::time::Duration;
use util::vnet::net::*;
use util::Conn;

use crate::agent::agent_config::{InterfaceFilterFn, IpFilterFn};
use crate::error::*;
use crate::network_type::*;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct LocalInterface {
    pub(crate) name: String,
    pub(crate) ip: IpAddr,
    pub(crate) adapter_type: u32,
    pub(crate) network_key: String,
    pub(crate) network_preference: u8,
    pub(crate) network_id: u16,
}

/// AddNetworkCostToVpn is enabled, DifferentiatedCellularCosts is not.
/// Native enumeration has no underlying-network monitor; like the Windows
/// BasicNetworkManager, a VPN therefore has underlying type UNKNOWN (cost 50).
/// This is a wire cost, not a guess based on the interface's display name.
pub(crate) fn ice_network_cost(adapter_type: u32) -> u16 {
    match adapter_type {
        1 | 16 => 0,
        2 => 10,
        4 | 64 | 128 | 256 | 512 => 900,
        8 => 2050,
        32 => 999,
        _ => 50,
    }
}

/// rtc::IPPrecedence, including ULA and transition ranges.
pub(crate) fn ice_ip_precedence(ip: IpAddr) -> u32 {
    match ip {
        IpAddr::V4(_) => 30,
        IpAddr::V6(ip) => {
            let b = ip.octets();
            if ip.is_loopback() {
                60
            } else if b[0] & 0xfe == 0xfc {
                50
            } else if ip.to_ipv4_mapped().is_some() {
                30
            } else if b[0..2] == [0x20, 0x02] {
                20
            } else if b[0..4] == [0x20, 0x01, 0, 0] {
                10
            } else if b[..12].iter().all(|b| *b == 0)
                || (b[0] == 0xfe && b[1] & 0xc0 == 0xc0)
                || b[0..2] == [0x3f, 0xfe]
            {
                1
            } else {
                40
            }
        }
    }
}

/// Port::AddAddress -> Candidate::GetPriority.
pub(crate) fn ice_candidate_priority(
    type_preference: u32,
    network_preference: u8,
    address: IpAddr,
    relay_preference: u32,
) -> u32 {
    (type_preference << 24)
        | ((((u32::from(network_preference) << 8) | ice_ip_precedence(address)) + relay_preference)
            << 8)
        | 255
}

pub fn create_addr(_network: NetworkType, ip: IpAddr, port: u16) -> SocketAddr {
    /*if network.is_tcp(){
        return &net.TCPAddr{IP: ip, Port: port}
    default:
        return &net.UDPAddr{IP: ip, Port: port}
    }*/
    SocketAddr::new(ip, port)
}

pub fn assert_inbound_username(m: &Message, expected_username: &str) -> Result<()> {
    let mut username = Username::new(ATTR_USERNAME, String::new());
    username.get_from(m)?;

    if username.to_string() != expected_username {
        return Err(Error::Other(format!(
            "{:?} expected({}) actual({})",
            Error::ErrMismatchUsername,
            expected_username,
            username,
        )));
    }

    Ok(())
}

pub fn assert_inbound_message_integrity(m: &mut Message, key: &[u8]) -> Result<()> {
    if turn::client::integrity::verify(m, key) {
        Ok(())
    } else {
        Err(Error::Other("invalid ICE message integrity".into()))
    }
}

/// Initiates a stun requests to `server_addr` using conn, reads the response and returns the
/// `XORMappedAddress` returned by the stun server.
/// Adapted from stun v0.2.
pub async fn get_xormapped_addr(
    conn: &Arc<dyn Conn + Send + Sync>,
    server_addr: SocketAddr,
    deadline: Duration,
) -> Result<XorMappedAddress> {
    let resp = stun_request(conn, server_addr, deadline).await?;
    let mut addr = XorMappedAddress::default();
    addr.get_from(&resp)?;
    Ok(addr)
}

const MAX_MESSAGE_SIZE: usize = 1280;

pub async fn stun_request(
    conn: &Arc<dyn Conn + Send + Sync>,
    server_addr: SocketAddr,
    deadline: Duration,
) -> Result<Message> {
    let mut request = Message::new();
    request.build(&[Box::new(BINDING_REQUEST), Box::new(TransactionId::new())])?;

    conn.send_to(&request.raw, server_addr).await?;
    let mut bs = vec![0_u8; MAX_MESSAGE_SIZE];
    let (n, _) = if deadline > Duration::from_secs(0) {
        match tokio::time::timeout(deadline, conn.recv_from(&mut bs)).await {
            Ok(result) => match result {
                Ok((n, addr)) => (n, addr),
                Err(err) => return Err(Error::Other(err.to_string())),
            },
            Err(err) => return Err(Error::Other(err.to_string())),
        }
    } else {
        conn.recv_from(&mut bs).await?
    };

    let mut res = Message::new();
    res.raw = bs[..n].to_vec();
    res.decode()?;

    Ok(res)
}

pub async fn local_interfaces(
    vnet: &Arc<Net>,
    interface_filter: &Option<InterfaceFilterFn>,
    ip_filter: &Option<IpFilterFn>,
    network_types: &[NetworkType],
    include_loopback: bool,
) -> HashSet<IpAddr> {
    local_interfaces_with_names(
        vnet,
        interface_filter,
        ip_filter,
        network_types,
        include_loopback,
    )
    .await
    .into_iter()
    .map(|interface| interface.ip)
    .collect()
}

pub(crate) async fn local_interfaces_with_names(
    vnet: &Arc<Net>,
    interface_filter: &Option<InterfaceFilterFn>,
    ip_filter: &Option<IpFilterFn>,
    network_types: &[NetworkType],
    include_loopback: bool,
) -> Vec<LocalInterface> {
    let mut catalog = crate::agent::network_catalog::NetworkCatalog::default();
    match catalog
        .update(
            vnet,
            interface_filter,
            ip_filter,
            network_types,
            include_loopback,
        )
        .await
    {
        Ok(update) => (*update.interfaces).clone(),
        Err(error) => {
            log::warn!("interface inventory failed: {error}");
            Vec::new()
        }
    }
}

pub async fn listen_udp_in_port_range(
    vnet: &Arc<Net>,
    port_max: u16,
    port_min: u16,
    laddr: SocketAddr,
) -> Result<Arc<dyn Conn + Send + Sync>> {
    if laddr.port() != 0 || (port_min == 0 && port_max == 0) {
        return Ok(vnet.bind(laddr).await?);
    }
    let i = if port_min == 0 { 1 } else { port_min };
    let j = if port_max == 0 { 0xFFFF } else { port_max };
    if i > j {
        return Err(Error::ErrPort);
    }

    let port_start = rand::random::<u16>() % (j - i + 1) + i;
    let mut port_current = port_start;
    loop {
        let laddr = SocketAddr::new(laddr.ip(), port_current);
        match vnet.bind(laddr).await {
            Ok(c) => return Ok(c),
            Err(err) => log::debug!("failed to listen {laddr}: {err}"),
        };

        port_current = port_current.checked_add(1).unwrap_or(i);

        if port_current > j {
            port_current = i;
        }
        if port_current == port_start {
            break;
        }
    }

    Err(Error::ErrPort)
}

#[cfg(test)]
mod tests {
    use util::vnet::net::{Net, NetConfig};

    use crate::util::listen_udp_in_port_range;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_listen_udp_in_port_range_overflow() {
        let vnet = Arc::new(Net::new(Some(NetConfig {
            static_ips: vec![],
            static_ip: "127.0.0.1".parse().unwrap(),
        })));

        // Preemptively bind to port 65535 (u16::MAX) to ensure that listen_udp_in_port_range
        // triggers overflow logic by being unable to bind to port 65535 itself.
        let _conn = vnet
            .bind("127.0.0.1:65535".parse().unwrap())
            .await
            .expect("Expected binding to vnet to succeed");

        let port_max = u16::MAX;
        let port_min = u16::MAX;

        assert!(matches!(
            listen_udp_in_port_range(&vnet, port_max, port_min, "127.0.0.1:0".parse().unwrap())
                .await,
            Err(crate::Error::ErrPort)
        ))
    }
}
