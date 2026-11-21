//! Native interface enumeration. Use SDK bindings so nested Rust structs
//! cannot insert padding into IP_ADAPTER_ADDRESSES's flat ABI.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::{io, mem, ptr, slice};
use winapi::shared::ifdef::IfOperStatusUp;
use winapi::shared::winerror::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
use winapi::shared::ws2def::{AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKET_ADDRESS};
use winapi::shared::ws2ipdef::SOCKADDR_IN6;
use winapi::um::iphlpapi::GetAdaptersAddresses;
use winapi::um::iptypes::{IP_ADAPTER_ADDRESSES_LH, IP_ADAPTER_PREFIX_XP};

use crate::ifaces::{Interface, Kind};

pub(crate) fn native_interfaces(include_virtual_adapters: bool) -> io::Result<Vec<Interface>> {
    let mut byte_count = 16_384_u32;
    loop {
        // The SDK struct's alignment union requires eight-byte alignment.
        let mut storage = vec![0_u64; (byte_count as usize).div_ceil(8)];
        let adapters = storage.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        let status = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC as u32,
                0x1e,
                ptr::null_mut(),
                adapters,
                &mut byte_count,
            )
        };
        if status == ERROR_BUFFER_OVERFLOW {
            continue;
        }
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        return Ok(unsafe { collect_adapters(adapters, include_virtual_adapters) });
    }
}

unsafe fn collect_adapters(
    mut adapter: *const IP_ADAPTER_ADDRESSES_LH,
    include_virtual_adapters: bool,
) -> Vec<Interface> {
    let mut result = Vec::new();
    let mut network_index = 0_u32;
    while let Some(current) = adapter.as_ref() {
        adapter = current.Next;
        if current.OperStatus != IfOperStatusUp {
            continue;
        }
        // UU BasicNetworkManager::CreateNetworks (0x180078678).
        let name = network_index.to_string();
        network_index += 1;
        let description = wide_string(current.Description);
        // UU 78678 -> 79096 -> 7665B. The adapter index still advances for
        // ignored adapters; the normal NetworkManager snapshot excludes them.
        let lower_description = description.to_ascii_lowercase();
        if !include_virtual_adapters
            && ["vmware", "hyper-v", "virtualbox", "vmnet", "parallels", "qemu"]
                .iter()
                .any(|part| lower_description.contains(part))
        {
            continue;
        }
        let mac = &current.PhysicalAddress
            [..(current.PhysicalAddressLength as usize).min(current.PhysicalAddress.len())];
        let adapter_type = uu_adapter_type(current.IfType, &description, mac);
        let mut unicast = current.FirstUnicastAddress;
        while let Some(address) = unicast.as_ref() {
            unicast = address.Next;
            let Some(mut socket) = socket_addr(&address.Address) else {
                continue;
            };
            if let SocketAddr::V6(v6) = &mut socket {
                v6.set_scope_id(current.Ipv6IfIndex);
            }
            result.push(Interface {
                name: name.clone(),
                kind: if socket.is_ipv4() {
                    Kind::Ipv4
                } else {
                    Kind::Ipv6
                },
                addr: Some(socket),
                mask: Some(network_mask(current.FirstPrefix, socket.ip())),
                hop: None,
                adapter_type,
            });
        }
    }
    result
}

unsafe fn socket_addr(address: &SOCKET_ADDRESS) -> Option<SocketAddr> {
    let socket = address.lpSockaddr.as_ref()?;
    match socket.sa_family as i32 {
        AF_INET if address.iSockaddrLength as usize >= mem::size_of::<SOCKADDR_IN>() => {
            let raw = &*address.lpSockaddr.cast::<SOCKADDR_IN>();
            let ip = Ipv4Addr::from(raw.sin_addr.S_un.S_addr().to_ne_bytes());
            Some(SocketAddr::V4(SocketAddrV4::new(
                ip,
                u16::from_be(raw.sin_port),
            )))
        }
        AF_INET6 if address.iSockaddrLength as usize >= mem::size_of::<SOCKADDR_IN6>() => {
            let raw = &*address.lpSockaddr.cast::<SOCKADDR_IN6>();
            let ip = Ipv6Addr::from(*raw.sin6_addr.u.Byte());
            Some(SocketAddr::V6(SocketAddrV6::new(
                ip,
                u16::from_be(raw.sin6_port),
                raw.sin6_flowinfo,
                *raw.u.sin6_scope_id(),
            )))
        }
        _ => None,
    }
}

// UU 0x180078529 chooses the longest matching FirstPrefix.
unsafe fn network_mask(mut prefix: *const IP_ADAPTER_PREFIX_XP, ip: IpAddr) -> SocketAddr {
    let mut best = 0;
    while let Some(entry) = prefix.as_ref() {
        prefix = entry.Next;
        let Some(network) = socket_addr(&entry.Address).map(|address| address.ip()) else {
            continue;
        };
        let matches = match (ip, network) {
            (IpAddr::V4(ip), IpAddr::V4(network)) if entry.PrefixLength <= 32 => {
                let mask = u32::MAX.checked_shl(32 - entry.PrefixLength).unwrap_or(0);
                u32::from(ip) & mask == u32::from(network)
            }
            (IpAddr::V6(ip), IpAddr::V6(network)) if entry.PrefixLength <= 128 => {
                let mask = u128::MAX.checked_shl(128 - entry.PrefixLength).unwrap_or(0);
                u128::from(ip) & mask == u128::from(network)
            }
            _ => false,
        };
        if matches {
            best = best.max(entry.PrefixLength);
        }
    }
    let mask = match ip {
        IpAddr::V4(_) => Ipv4Addr::from(u32::MAX.checked_shl(32 - best).unwrap_or(0)).into(),
        IpAddr::V6(_) => Ipv6Addr::from(u128::MAX.checked_shl(128 - best).unwrap_or(0)).into(),
    };
    SocketAddr::new(mask, 0)
}

// Exact description/MAC tables from 0x180076574 / 0x180077ED8.
fn uu_adapter_type(if_type: u32, description: &str, mac: &[u8]) -> u32 {
    let description = description.to_ascii_lowercase();
    if [
        "tap-windows",
        "wintun",
        "vpn",
        "wireguard",
        "fortinet",
        "cisco anyconnect",
        "openvpn",
        "zerotier",
        "tailscale",
    ]
    .iter()
    .any(|name| description.contains(name))
        || mac == [0x00, 0x05, 0x9a, 0x3c, 0x7a, 0x00]
        || mac == [0x02, 0x50, 0x41, 0x00, 0x00, 0x01]
    {
        return 8;
    }
    match if_type {
        6 | 26 | 55 | 62 | 69 | 117 => 1,
        71 => 2,
        243 | 244 => 4,
        24 => 16,
        _ => 0,
    }
}

unsafe fn wide_string(value: *const u16) -> String {
    if value.is_null() {
        return String::new();
    }
    let mut len = 0;
    while *value.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(slice::from_raw_parts(value, len))
}
