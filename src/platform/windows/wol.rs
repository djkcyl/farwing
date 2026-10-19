//! Read-only physical IPv4 inventory, change notifications, and interface-bound WoL UDP.
pub(crate) mod setup;
use crate::features::host::wol::packet::{LanInfo, mask, valid_ip};
use anyhow::{Context, Result, ensure};
use std::{
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    os::windows::io::AsRawSocket,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use windows::Win32::{
    Foundation::*,
    NetworkManagement::{IpHelper::*, Ndis::*},
    Networking::WinSock::*,
};
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Interface {
    pub index: u32,
    pub luid: u64,
    pub name: String,
    pub mac: [u8; 6],
    pub ip: Ipv4Addr,
    pub prefix: u8,
    pub default_metric: Option<u64>,
}
impl Interface {
    pub fn registration(&self) -> LanInfo {
        LanInfo {
            mac: self
                .mac
                .iter()
                .map(|v| format!("{v:02X}"))
                .collect::<Vec<_>>()
                .join(":"),
            inner_ip: self.ip.to_string(),
            subnet_mask: mask(self.prefix).to_string(),
        }
    }
}
pub(crate) fn inventory() -> Result<Vec<Interface>> {
    unsafe {
        let mut routes = std::ptr::null_mut();
        GetIpForwardTable2(AF_INET, &mut routes).ok()?;
        struct Table(*mut MIB_IPFORWARD_TABLE2);
        impl Drop for Table {
            fn drop(&mut self) {
                unsafe {
                    FreeMibTable(self.0.cast());
                }
            }
        }
        let _table = Table(routes);
        ensure!(!routes.is_null(), "无法读取路由表");
        let routes =
            std::slice::from_raw_parts((*routes).Table.as_ptr(), (*routes).NumEntries as usize);
        let mut size = 16384;
        for _ in 0..3 {
            let mut memory = vec![0u64; (size as usize).div_ceil(8)];
            let first = memory.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
            let code = GetAdaptersAddresses(
                AF_INET.0 as u32,
                GAA_FLAG_INCLUDE_GATEWAYS,
                None,
                Some(first),
                &mut size,
            );
            if code == ERROR_BUFFER_OVERFLOW.0 {
                ensure!(size <= 1024 * 1024, "网卡列表过大");
                continue;
            }
            WIN32_ERROR(code).ok()?;
            let mut result = Vec::new();
            let mut p = first;
            while !p.is_null() {
                let a = &*p;
                p = a.Next;
                if !matches!(a.IfType, 6 | 71)
                    || a.OperStatus != IfOperStatusUp
                    || a.PhysicalAddressLength != 6
                {
                    continue;
                }
                let mut row = MIB_IF_ROW2 {
                    InterfaceLuid: a.Luid,
                    ..Default::default()
                };
                if GetIfEntry2(&mut row).ok().is_err() {
                    continue;
                }
                if row.InterfaceAndOperStatusFlags._bitfield & 1 == 0 {
                    continue;
                }
                let index = row.InterfaceIndex;
                let luid = a.Luid.Value;
                let mac: [u8; 6] = a.PhysicalAddress[..6].try_into().unwrap();
                if mac == [0; 6] || mac[0] & 1 != 0 {
                    continue;
                }
                let default_metric = routes
                    .iter()
                    .filter(|r| r.InterfaceIndex == index && r.DestinationPrefix.PrefixLength == 0)
                    .map(|r| u64::from(r.Metric) + u64::from(a.Ipv4Metric))
                    .min();
                let name = a
                    .FriendlyName
                    .to_string()
                    .unwrap_or_else(|_| format!("网卡 {index}"));
                let mut address = a.FirstUnicastAddress;
                while !address.is_null() {
                    let u = &*address;
                    address = u.Next;
                    if u.DadState != IpDadStatePreferred
                        || !(1..=30).contains(&u.OnLinkPrefixLength)
                        || u.Address.lpSockaddr.is_null()
                    {
                        continue;
                    }
                    if (*u.Address.lpSockaddr).sa_family != AF_INET {
                        continue;
                    }
                    let addr = &*u.Address.lpSockaddr.cast::<SOCKADDR_IN>();
                    let ip = Ipv4Addr::from(addr.sin_addr.S_un.S_addr.to_ne_bytes());
                    if !valid_ip(ip) {
                        continue;
                    }
                    result.push(Interface {
                        index,
                        luid,
                        name: name.clone(),
                        mac,
                        ip,
                        prefix: u.OnLinkPrefixLength,
                        default_metric,
                    });
                }
            }
            result.sort_by_key(|v| (v.default_metric.unwrap_or(u64::MAX), v.index, v.ip));
            return Ok(result);
        }
    }
    anyhow::bail!("网卡列表持续变化")
}
#[derive(Default)]
pub(crate) struct Changes {
    pub revision: AtomicU64,
    pub notify: tokio::sync::Notify,
}
impl Changes {
    fn changed(&self) {
        self.revision.fetch_add(1, Ordering::AcqRel);
        self.notify.notify_one();
    }
}
pub(crate) struct Watcher {
    handles: Vec<HANDLE>,
    context: Option<Box<Arc<Changes>>>,
}
// MIB registrations can be cancelled on a different thread; callback context stays boxed.
unsafe impl Send for Watcher {}
impl Watcher {
    pub fn new(changes: Arc<Changes>) -> Result<Self> {
        let mut watcher = Self {
            handles: Vec::new(),
            context: Some(Box::new(changes)),
        };
        let context = (watcher.context.as_deref().unwrap() as *const Arc<Changes>).cast();
        unsafe {
            let mut h = HANDLE::default();
            NotifyIpInterfaceChange(
                AF_INET,
                Some(interface_changed),
                Some(context),
                false,
                &mut h,
            )
            .ok()?;
            watcher.handles.push(h);
            let mut h = HANDLE::default();
            NotifyUnicastIpAddressChange(
                AF_INET,
                Some(address_changed),
                Some(context),
                false,
                &mut h,
            )
            .ok()?;
            watcher.handles.push(h);
            let mut h = HANDLE::default();
            NotifyRouteChange2(AF_INET, Some(route_changed), context, false, &mut h).ok()?;
            watcher.handles.push(h);
        }
        Ok(watcher)
    }
}
impl Drop for Watcher {
    fn drop(&mut self) {
        let mut failed = false;
        for h in self.handles.drain(..) {
            if unsafe { CancelMibChangeNotify2(h) }.ok().is_err() {
                failed = true;
            }
        }
        if failed {
            // Never free memory that an OS callback may still reference.
            if let Some(context) = self.context.take() {
                let _ = Box::leak(context);
            }
            tracing::warn!(
                "WoL network notification cancellation failed; callback context retained until exit"
            );
        }
    }
}
unsafe fn changed(p: *const core::ffi::c_void) {
    if !p.is_null() {
        unsafe {
            (&*p.cast::<Arc<Changes>>()).changed();
        }
    }
}
unsafe extern "system" fn interface_changed(
    p: *const core::ffi::c_void,
    _: *const MIB_IPINTERFACE_ROW,
    _: MIB_NOTIFICATION_TYPE,
) {
    unsafe {
        changed(p);
    }
}
unsafe extern "system" fn address_changed(
    p: *const core::ffi::c_void,
    _: *const MIB_UNICASTIPADDRESS_ROW,
    _: MIB_NOTIFICATION_TYPE,
) {
    unsafe {
        changed(p);
    }
}
unsafe extern "system" fn route_changed(
    p: *const core::ffi::c_void,
    _: *const MIB_IPFORWARD_ROW2,
    _: MIB_NOTIFICATION_TYPE,
) {
    unsafe {
        changed(p);
    }
}
#[derive(Default)]
pub(crate) struct Sent {
    pub attempted: u32,
    pub sent: u32,
}
pub(crate) fn send(
    interface: &Interface,
    packet: &[u8; 102],
    destinations: &[SocketAddrV4],
    valid: impl Fn() -> bool,
) -> Result<Sent> {
    ensure!(valid(), "唤醒请求已失效");
    let socket = UdpSocket::bind(SocketAddrV4::new(interface.ip, 0)).context("绑定唤醒源网卡")?;
    socket.set_broadcast(true)?;
    socket.set_write_timeout(Some(std::time::Duration::from_millis(500)))?;
    let index = interface.index.to_be().to_ne_bytes();
    unsafe {
        if setsockopt(
            SOCKET(socket.as_raw_socket() as usize),
            IPPROTO_IP.0,
            IP_UNICAST_IF,
            Some(&index),
        ) == SOCKET_ERROR
        {
            return Err(std::io::Error::from_raw_os_error(WSAGetLastError().0).into());
        }
    }
    let mut result = Sent::default();
    for destination in destinations {
        ensure!(valid(), "唤醒请求已取消或网络已改变");
        result.attempted += 1;
        match socket.send_to(packet, *destination) {
            Ok(n) if n == packet.len() => result.sent += 1,
            Ok(_) => tracing::warn!(port = destination.port(), "WoL UDP write was incomplete"),
            Err(error) => tracing::debug!(
                port = destination.port(),
                code = error.raw_os_error(),
                "WoL UDP send failed"
            ),
        }
    }
    ensure!(result.sent > 0, "所有唤醒端口均发送失败");
    Ok(result)
}
