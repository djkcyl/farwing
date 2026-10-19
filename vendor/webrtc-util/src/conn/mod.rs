pub mod conn_bridge;
pub mod conn_disconnected_packet;
pub mod conn_pipe;
pub mod conn_udp;
pub mod conn_udp_listener;

#[cfg(test)]
mod conn_bridge_test;
#[cfg(test)]
mod conn_pipe_test;
#[cfg(test)]
mod conn_test;

//TODO: remove this conditional test
#[cfg(not(target_os = "windows"))]
#[cfg(test)]
mod conn_udp_listener_test;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tokio::net::ToSocketAddrs;

use crate::error::Result;

#[async_trait]
pub trait Conn {
    async fn connect(&self, addr: SocketAddr) -> Result<()>;
    async fn recv(&self, buf: &mut [u8]) -> Result<usize>;
    async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)>;
    /// Earliest observed packet arrival, before application dispatch queues.
    /// Physical packet transports can use this default. Queued adapters must
    /// override it to carry the upstream timestamp alongside the packet.
    async fn recv_with_timestamp(&self, buf: &mut [u8]) -> Result<(usize, Instant)> {
        let n = self.recv(buf).await?;
        Ok((n, Instant::now()))
    }
    async fn recv_from_with_timestamp(
        &self,
        buf: &mut [u8],
    ) -> Result<(usize, SocketAddr, Instant)> {
        let (n, from) = self.recv_from(buf).await?;
        Ok((n, from, Instant::now()))
    }
    async fn send(&self, buf: &[u8]) -> Result<usize>;
    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> Result<usize>;
    /// Port-owned ICE/probe traffic, not Connection application data. TURN
    /// uses this distinction to decide whether to begin channel binding.
    async fn send_control_to(&self, buf: &[u8], target: SocketAddr) -> Result<usize> {
        self.send_to(buf, target).await
    }
    /// Single nonblocking datagram attempt used by the probe sender. Stream
    /// framing and request retransmissions must use their owning async path.
    fn try_send_to(&self, _buf: &[u8], _target: SocketAddr) -> Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "transport has no raw datagram send",
        )
        .into())
    }
    fn local_addr(&self) -> Result<SocketAddr>;
    fn remote_addr(&self) -> Option<SocketAddr>;
    /// Transport-specific peer ownership. Plain datagram transports need no
    /// setup; TURN prepares its entry before the first connectivity packet.
    async fn prepare_peer(&self, _peer: SocketAddr) -> Result<()> {
        Ok(())
    }
    async fn release_peer(&self, _peer: SocketAddr) {}
    fn take_failed_peer_addrs(&self) -> Vec<SocketAddr> {
        Vec::new()
    }
    async fn close(&self) -> Result<()>;
    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync);
}

/// A Listener is a generic network listener for connection-oriented protocols.
/// Multiple connections may invoke methods on a Listener simultaneously.
#[async_trait]
pub trait Listener {
    /// accept waits for and returns the next connection to the listener.
    async fn accept(&self) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr)>;

    /// close closes the listener.
    /// Any blocked accept operations will be unblocked and return errors.
    async fn close(&self) -> Result<()>;

    /// addr returns the listener's network address.
    async fn addr(&self) -> Result<SocketAddr>;
}

pub async fn lookup_host<T>(use_ipv4: bool, host: T) -> Result<SocketAddr>
where
    T: ToSocketAddrs,
{
    for remote_addr in tokio::net::lookup_host(host).await? {
        if (use_ipv4 && remote_addr.is_ipv4()) || (!use_ipv4 && remote_addr.is_ipv6()) {
            return Ok(remote_addr);
        }
    }

    Err(std::io::Error::other(format!(
        "No available {} IP address found!",
        if use_ipv4 { "ipv4" } else { "ipv6" },
    ))
    .into())
}
