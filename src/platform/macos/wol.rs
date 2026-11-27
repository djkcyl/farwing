//! Read-only physical IPv4 inventory and interface-bound WoL UDP, viewer-only:
//! macOS reports no adapters in this port, so there is nothing to wake from.
pub(crate) mod setup;
use crate::features::host::wol::packet::LanInfo;
use anyhow::{Result, bail};
use std::{
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
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
            subnet_mask: crate::features::host::wol::packet::mask(self.prefix).to_string(),
        }
    }
}

pub(crate) fn inventory() -> Result<Vec<Interface>> {
    Ok(Vec::new())
}

#[derive(Default)]
pub(crate) struct Changes {
    pub revision: AtomicU64,
    pub notify: tokio::sync::Notify,
}

/// Reports a changed inventory; with an always-empty inventory the watcher
/// keeps the shape but never fires.
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
                while !stopping.load(Ordering::Acquire) {
                    std::thread::park_timeout(Duration::from_secs(2));
                }
            })
            .map_err(|error| anyhow::anyhow!("启动网络变化监视失败：{error}"))?;
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
    _interface: &Interface,
    _packet: &[u8; 102],
    _destinations: &[SocketAddrV4],
    valid: impl Fn() -> bool,
) -> Result<Sent> {
    anyhow::ensure!(valid(), "唤醒请求已失效");
    let _ = UdpSocket::bind("0.0.0.0:0");
    bail!("macOS 暂不支持网络唤醒")
}
