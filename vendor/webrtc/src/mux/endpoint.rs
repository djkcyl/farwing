use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use util::{Buffer, Conn};

use crate::mux::mux_func::MatchFunc;
use crate::mux::Endpoints;

/// Endpoint implements net.Conn. It is used to read muxed packets.
pub struct Endpoint {
    pub(crate) id: usize,
    pub(crate) buffer: Buffer,
    pub(crate) match_fn: MatchFunc,
    pub(crate) next_conn: Arc<dyn Conn + Send + Sync>,
    pub(crate) endpoints: Endpoints,
}

impl Endpoint {
    /// Close unregisters the endpoint from the Mux
    pub async fn close(&self) -> Result<()> {
        self.buffer.close().await;

        let mut endpoints = self.endpoints.lock().await;
        if let Some(endpoints) = endpoints.as_mut() {
            endpoints.remove(&self.id);
        }

        Ok(())
    }
}

type Result<T> = std::result::Result<T, util::Error>;

#[async_trait]
impl Conn for Endpoint {
    async fn connect(&self, _addr: SocketAddr) -> Result<()> {
        Err(io::Error::other("Not applicable").into())
    }

    /// reads a packet of len(p) bytes from the underlying conn
    /// that are matched by the associated MuxFunc
    async fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        self.buffer.read(buf, None).await
    }
    async fn recv_with_timestamp(&self, buf: &mut [u8]) -> Result<(usize, Instant)> {
        let (n, received_at) = self.buffer.read_with_timestamp(buf, None).await?;
        Ok((n, received_at.unwrap_or_else(Instant::now)))
    }
    async fn recv_from(&self, _buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        Err(io::Error::other("Not applicable").into())
    }

    /// writes bytes to the underlying conn
    async fn send(&self, buf: &[u8]) -> Result<usize> {
        if self.buffer.is_closed().await {
            return Err(util::Error::ErrBufferClosed);
        }
        self.next_conn.send(buf).await
    }

    async fn send_to(&self, _buf: &[u8], _target: SocketAddr) -> Result<usize> {
        Err(io::Error::other("Not applicable").into())
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        self.next_conn.local_addr()
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        self.next_conn.remote_addr()
    }

    async fn close(&self) -> Result<()> {
        // A protocol endpoint never owns its sibling protocols' transport.
        Endpoint::close(self).await
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}
