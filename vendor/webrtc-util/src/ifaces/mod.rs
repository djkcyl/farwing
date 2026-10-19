pub mod ffi;
pub use ffi::ifaces;

#[derive(PartialEq, Eq, Debug, Clone)]
pub enum NextHop {
    Broadcast(::std::net::SocketAddr),
    Destination(::std::net::SocketAddr),
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub enum Kind {
    Packet,
    Link,
    Ipv4,
    Ipv6,
    Unknow(i32),
}

#[derive(Debug, Clone)]
pub struct Interface {
    pub name: String,
    pub kind: Kind,
    pub addr: Option<::std::net::SocketAddr>,
    pub mask: Option<::std::net::SocketAddr>,
    pub hop: Option<NextHop>,
    /// WebRTC adapter classification used by the network-cost policy.
    ///
    /// Native platform backends fill this from the OS adapter metadata.  A
    /// value of zero means that the platform did not expose an equivalent
    /// classification; callers must then use the default unknown-network
    /// cost instead of inferring a type from the interface name.
    pub adapter_type: u32,
}
