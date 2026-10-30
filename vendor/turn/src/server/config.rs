use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::time::Duration;
use util::Conn;

use crate::allocation::*;
use crate::auth::*;
use crate::error::*;
use crate::relay::*;

/// ConnConfig is used for UDP listeners
pub struct ConnConfig {
    pub conn: Arc<dyn Conn + Send + Sync>,

    // When an allocation is generated the RelayAddressGenerator
    // creates the net.PacketConn and returns the IP/Port it is available at
    pub relay_addr_generator: Box<dyn RelayAddressGenerator + Send + Sync>,
}

impl ConnConfig {
    pub fn validate(&self) -> Result<()> {
        self.relay_addr_generator.validate()
    }
}

/// Whether a client may relay to this peer (Farwing: keeps the relay off
/// the server's own and private networks).
pub type PeerFilter = Arc<dyn Fn(std::net::SocketAddr) -> bool + Send + Sync>;

/// Farwing: what leaves the relay. Every packet an allocation sends — to
/// its client, or to a peer elsewhere — passes the allocation's gauge.
pub trait Egress: Send + Sync {
    /// The gauge of allocations made with `username` (`None`: unmetered).
    fn gauge(&self, username: &str) -> Option<Arc<dyn Gauge>>;
    /// `peer` is one of this relay's own addresses: the hop stays here.
    fn is_local(&self, peer: std::net::SocketAddr) -> bool;
}

/// One allocation's share of the relay's egress.
pub trait Gauge: Send + Sync {
    /// `bytes` of payload are about to leave; false drops them.
    fn admit(&self, bytes: usize) -> bool;
}

/// ServerConfig configures the TURN Server
pub struct ServerConfig {
    /// `conn_configs` are a list of all the turn listeners.
    /// Each listener can have custom behavior around the creation of Relays.
    pub conn_configs: Vec<ConnConfig>,

    /// `realm` sets the realm for this server
    pub realm: String,

    /// `auth_handler` is a callback used to handle incoming auth requests,
    /// allowing users to customize Pion TURN with custom behavior.
    pub auth_handler: Arc<dyn AuthHandler + Send + Sync>,

    /// `channel_bind_timeout` sets the lifetime of channel binding. Defaults to 10 minutes.
    pub channel_bind_timeout: Duration,

    /// To receive notify on allocation close event, with metrics data.
    pub alloc_close_notify: Option<mpsc::Sender<AllocationInfo>>,

    /// Peers that CreatePermission and ChannelBind may name; `None` allows
    /// every peer.
    pub peer_filter: Option<PeerFilter>,

    /// Meters (and may limit) what the allocations send; `None`: nothing.
    pub egress: Option<Arc<dyn Egress>>,
}

impl ServerConfig {
    pub fn validate(&self) -> Result<()> {
        if self.conn_configs.is_empty() {
            return Err(Error::ErrNoAvailableConns);
        }

        for cc in &self.conn_configs {
            cc.validate()?;
        }
        Ok(())
    }
}
