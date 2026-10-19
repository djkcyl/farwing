use std::net::SocketAddr;

use ipnet::*;

use crate::error::*;

#[derive(Debug, Clone, Default)]
pub struct Interface {
    pub(crate) name: String,
    pub(crate) addrs: Vec<IpNet>,
    pub(crate) adapter_type: u32,
}

impl Interface {
    pub fn new(name: String, addrs: Vec<IpNet>) -> Self {
        Interface {
            name,
            addrs,
            adapter_type: 0,
        }
    }

    pub fn new_with_adapter_type(name: String, addrs: Vec<IpNet>, adapter_type: u32) -> Self {
        Interface {
            name,
            addrs,
            adapter_type,
        }
    }

    pub fn add_addr(&mut self, addr: IpNet) {
        self.addrs.push(addr);
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn addrs(&self) -> &[IpNet] {
        &self.addrs
    }

    pub fn adapter_type(&self) -> u32 {
        self.adapter_type
    }

    pub fn convert(addr: SocketAddr, mask: Option<SocketAddr>) -> Result<IpNet> {
        if let Some(mask) = mask {
            Ok(IpNet::with_netmask(addr.ip(), mask.ip()).map_err(|_| Error::ErrInvalidMask)?)
        } else {
            Ok(IpNet::new(addr.ip(), if addr.is_ipv4() { 32 } else { 128 })
                .expect("ipv4 should always work with prefix 32 and ipv6 with prefix 128"))
        }
    }
}
