//! Read-only physical IPv4 inventory, change notifications, and
//! interface-bound WoL UDP, as the Windows module provides them.
//!
//! The inventory comes from `getifaddrs` plus `/sys/class/net`: connected
//! physical Ethernet and Wi-Fi adapters only, ordered by the metric of their
//! default route. Changes are noticed by comparing the inventory every two
//! seconds rather than through a netlink subscription.
pub(crate) mod setup;
use crate::features::host::wol::packet::{LanInfo, mask, valid_ip};
use anyhow::{Context, Result, ensure};
use std::{
    ffi::CStr,
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Interface {
    pub index: u32,
    /// Windows' interface LUID; the kernel interface index here.
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

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
}

/// A connected physical Ethernet or Wi-Fi adapter and its MAC address.
fn physical(name: &str) -> Option<[u8; 6]> {
    let device = Path::new("/sys/class/net").join(name);
    if !device.join("device").exists()
        || read(&device.join("type")).as_deref() != Some("1")
        || read(&device.join("operstate")).as_deref() != Some("up")
    {
        return None;
    }
    let text = read(&device.join("address"))?;
    let mut mac = [0u8; 6];
    let mut parts = text.split(':');
    for byte in &mut mac {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    (parts.next().is_none() && mac != [0; 6] && mac[0] & 1 == 0).then_some(mac)
}

/// The lowest metric of each interface's default routes.
fn default_metrics() -> std::collections::HashMap<String, u64> {
    let mut metrics = std::collections::HashMap::new();
    let Ok(routes) = std::fs::read_to_string("/proc/net/route") else {
        return metrics;
    };
    for line in routes.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [interface, destination, _, flags, _, _, metric, ..] = fields[..] else {
            continue;
        };
        let up = u32::from_str_radix(flags, 16).is_ok_and(|f| f & 1 != 0);
        if destination != "00000000" || !up {
            continue;
        }
        let metric = metric.parse::<u64>().unwrap_or(u64::MAX);
        metrics
            .entry(interface.to_owned())
            .and_modify(|m: &mut u64| *m = (*m).min(metric))
            .or_insert(metric);
    }
    metrics
}

pub(crate) fn inventory() -> Result<Vec<Interface>> {
    let metrics = default_metrics();
    let mut addresses: *mut libc::ifaddrs = std::ptr::null_mut();
    ensure!(
        unsafe { libc::getifaddrs(&mut addresses) } == 0,
        "无法读取网卡地址：{}",
        std::io::Error::last_os_error()
    );
    struct List(*mut libc::ifaddrs);
    impl Drop for List {
        fn drop(&mut self) {
            unsafe { libc::freeifaddrs(self.0) }
        }
    }
    let _list = List(addresses);
    let mut result = Vec::new();
    let mut next = addresses;
    while !next.is_null() {
        let entry = unsafe { &*next };
        next = entry.ifa_next;
        if entry.ifa_addr.is_null()
            || entry.ifa_netmask.is_null()
            || i32::from(unsafe { (*entry.ifa_addr).sa_family }) != libc::AF_INET
        {
            continue;
        }
        let name = unsafe { CStr::from_ptr(entry.ifa_name) }
            .to_string_lossy()
            .into_owned();
        let Some(mac) = physical(&name) else {
            continue;
        };
        let address = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_in>() };
        let netmask = unsafe { &*entry.ifa_netmask.cast::<libc::sockaddr_in>() };
        let ip = Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr));
        let prefix = u32::from_be(netmask.sin_addr.s_addr).count_ones() as u8;
        if !valid_ip(ip) || !(1..=30).contains(&prefix) {
            continue;
        }
        let index = unsafe { libc::if_nametoindex(entry.ifa_name) };
        if index == 0 {
            continue;
        }
        result.push(Interface {
            index,
            luid: u64::from(index),
            default_metric: metrics.get(&name).copied(),
            name,
            mac,
            ip,
            prefix,
        });
    }
    result.sort_by_key(|v| (v.default_metric.unwrap_or(u64::MAX), v.index, v.ip));
    Ok(result)
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

/// Reports a changed inventory, checked every two seconds.
pub(crate) struct Watcher {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Watcher {
    pub fn new(changes: Arc<Changes>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::Builder::new()
            .name("wol-network-watch".into())
            .spawn(move || {
                let mut last = inventory().ok();
                while !stopping.load(Ordering::Acquire) {
                    std::thread::park_timeout(Duration::from_secs(2));
                    if stopping.load(Ordering::Acquire) {
                        break;
                    }
                    let current = inventory().ok();
                    if current != last {
                        last = current;
                        changes.changed();
                    }
                }
            })
            .context("启动网络变化监视失败")?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}
impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
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
    // Bound to the adapter's own address, so the packet leaves through it.
    let socket = UdpSocket::bind(SocketAddrV4::new(interface.ip, 0)).context("绑定唤醒源网卡")?;
    socket.set_broadcast(true)?;
    socket.set_write_timeout(Some(Duration::from_millis(500)))?;
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

#[cfg(test)]
mod tests {
    #[test]
    fn inventory_lists_valid_physical_addresses() {
        for interface in super::inventory().unwrap() {
            assert!(super::valid_ip(interface.ip));
            assert!((1..=30).contains(&interface.prefix));
            assert_eq!(interface.registration().mac.len(), 17);
        }
    }
}
