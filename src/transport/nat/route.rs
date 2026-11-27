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
#[cfg(windows)]
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
#[cfg(windows)]
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

#[cfg(target_os = "linux")]
pub(crate) fn current() -> Option<Network> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:9").ok()?;
    let std::net::IpAddr::V4(local) = socket.local_addr().ok()?.ip() else { return None };
    let routes = std::fs::read_to_string("/proc/net/route").ok();
    let gateway = routes.as_deref().and_then(|routes| {
        routes.lines().skip(1).filter_map(|line| {
            let mut fields = line.split_ascii_whitespace();
            let _interface = fields.next()?;
            let destination = fields.next()?;
            let gateway = u32::from_str_radix(fields.next()?, 16).ok()?;
            let flags = u32::from_str_radix(fields.next()?, 16).ok()?;
            let metric = fields.nth(2)?.parse::<u32>().ok()?;
            (destination == "00000000" && gateway != 0 && flags & 3 == 3)
                .then_some((metric, Ipv4Addr::from(gateway.to_le_bytes())))
        }).min_by_key(|(metric, _)| *metric).map(|(_, gateway)| gateway)
    });
    (!local.is_unspecified() && !local.is_loopback()).then_some(Network { local, gateway })
}

#[cfg(target_os = "linux")]
fn ipv6_source() -> Option<Ipv6Addr> {
    let socket = std::net::UdpSocket::bind("[::]:0").ok()?;
    socket.connect("[2001:4860:4860::8888]:9").ok()?;
    let std::net::IpAddr::V6(local) = socket.local_addr().ok()?.ip() else { return None };
    (!local.is_unspecified() && !local.is_loopback()).then_some(local)
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
