//! The IPv4 default route: the network fingerprint for cached discovery
//! results and the gateway that port mapping requests go to.
use std::net::{Ipv4Addr, Ipv6Addr};

/// Local address and gateway of the default IPv4 route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Network {
    pub local: Ipv4Addr,
    /// `None` for an on-link default route (no next hop).
    pub gateway: Option<Ipv4Addr>,
}

/// The current default route, from the system's best route towards a public
/// address. Nothing is sent.
pub(crate) fn current() -> Option<Network> {
    use windows::Win32::{
        NetworkManagement::IpHelper::{GetBestRoute2, MIB_IPFORWARD_ROW2},
        Networking::WinSock::{AF_INET, SOCKADDR_INET},
    };
    unsafe {
        let mut destination: SOCKADDR_INET = std::mem::zeroed();
        destination.Ipv4.sin_family = AF_INET;
        destination.Ipv4.sin_addr.S_un.S_addr = u32::from_ne_bytes([8, 8, 8, 8]);
        let mut route: MIB_IPFORWARD_ROW2 = std::mem::zeroed();
        let mut source: SOCKADDR_INET = std::mem::zeroed();
        GetBestRoute2(None, 0, None, &destination, 0, &mut route, &mut source)
            .ok()
            .ok()?;
        if source.si_family != AF_INET {
            return None;
        }
        let local = Ipv4Addr::from(source.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes());
        let gateway = (route.NextHop.si_family == AF_INET)
            .then(|| Ipv4Addr::from(route.NextHop.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes()))
            .filter(|ip| !ip.is_unspecified());
        (!local.is_unspecified() && !local.is_loopback()).then_some(Network { local, gateway })
    }
}

/// Source address of the best IPv6 route towards a public address.
fn ipv6_source() -> Option<Ipv6Addr> {
    use windows::Win32::{
        NetworkManagement::IpHelper::{GetBestRoute2, MIB_IPFORWARD_ROW2},
        Networking::WinSock::{AF_INET6, SOCKADDR_INET},
    };
    unsafe {
        let mut destination: SOCKADDR_INET = std::mem::zeroed();
        destination.Ipv6.sin6_family = AF_INET6;
        destination.Ipv6.sin6_addr.u.Byte = "2001:4860:4860::8888".parse::<Ipv6Addr>().ok()?.octets();
        let mut route: MIB_IPFORWARD_ROW2 = std::mem::zeroed();
        let mut source: SOCKADDR_INET = std::mem::zeroed();
        GetBestRoute2(None, 0, None, &destination, 0, &mut route, &mut source)
            .ok()
            .ok()?;
        if source.si_family != AF_INET6 {
            return None;
        }
        let local = Ipv6Addr::from(source.Ipv6.sin6_addr.u.Byte);
        (!local.is_unspecified() && !local.is_loopback()).then_some(local)
    }
}

/// The paths a session can use: the IPv4 default route and the IPv6 source
/// address. Interface, route and address notifications are frequent (VPN and
/// virtual adapters, address lifetimes); only a change here moves sessions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    ipv4: Option<Network>,
    ipv6: Option<Ipv6Addr>,
}

pub(crate) fn fingerprint() -> Fingerprint {
    Fingerprint {
        ipv4: current(),
        ipv6: ipv6_source(),
    }
}
