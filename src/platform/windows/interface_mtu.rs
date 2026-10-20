//! The MTU of the network interface that holds a local address.
use std::net::IpAddr;
use windows::Win32::{
    Foundation::*,
    NetworkManagement::IpHelper::*,
    Networking::WinSock::*,
};

/// `None` when no interface holds `address` or the list cannot be read.
pub(crate) fn interface_mtu(address: IpAddr) -> Option<u32> {
    let mut size = 16384u32;
    for _ in 0..3 {
        let mut memory = vec![0u64; (size as usize).div_ceil(8)];
        let first = memory.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        let code = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC.0 as u32,
                GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                None,
                Some(first),
                &mut size,
            )
        };
        if code == ERROR_BUFFER_OVERFLOW.0 && size <= 1024 * 1024 {
            continue;
        }
        if code != ERROR_SUCCESS.0 {
            return None;
        }
        let mut adapter = first.cast_const();
        while !adapter.is_null() {
            let a = unsafe { &*adapter };
            adapter = a.Next;
            let mut unicast = a.FirstUnicastAddress.cast_const();
            while !unicast.is_null() {
                let u = unsafe { &*unicast };
                unicast = u.Next;
                if !u.Address.lpSockaddr.is_null() && socket_address(u.Address.lpSockaddr) == Some(address) {
                    return Some(a.Mtu);
                }
            }
        }
        return None;
    }
    None
}

fn socket_address(address: *const SOCKADDR) -> Option<IpAddr> {
    unsafe {
        match (*address).sa_family {
            AF_INET => {
                let v4 = &*address.cast::<SOCKADDR_IN>();
                Some(IpAddr::from(v4.sin_addr.S_un.S_addr.to_ne_bytes()))
            }
            AF_INET6 => {
                let v6 = &*address.cast::<SOCKADDR_IN6>();
                Some(IpAddr::from(v6.sin6_addr.u.Byte))
            }
            _ => None,
        }
    }
}
