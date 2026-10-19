//! NetworkManager input and identity, separate from Port/Connection lifetime.
//! Tracks network enumeration, change detection, and allocator filtering.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use util::vnet::{interface::Interface, net::Net};

use crate::agent::agent_config::{InterfaceFilterFn, IpFilterFn};
use crate::network_type::NetworkType;
use crate::util::{ice_ip_precedence, LocalInterface};

#[derive(Clone, Debug)]
struct Network {
    local: LocalInterface,
    addresses: Vec<IpAddr>,
    active: bool,
}

impl Network {
    fn best_ip(addresses: &[IpAddr]) -> Option<IpAddr> {
        let first = *addresses.first()?;
        if first.is_ipv4() {
            Some(first)
        } else {
            addresses
                .iter()
                .rev()
                .copied()
                .find(|ip| matches!(ip, IpAddr::V6(ip) if ip.octets()[0] & 0xfe != 0xfc))
                .or_else(|| addresses.last().copied())
        }
    }
}

pub(crate) struct NetworkUpdate {
    pub changed: bool,
    pub interfaces: Arc<Vec<LocalInterface>>,
    /// Type signals can change cost even when a cellular-family transition
    /// deliberately does not emit a global NetworksChanged notification.
    pub type_changes: Vec<(String, u32)>,
}

pub(crate) struct NetworkCatalog {
    networks: BTreeMap<String, Network>,
    active: Vec<String>,
    next_id: u16,
    initialized: bool,
    default_v4: Option<IpAddr>,
    default_v6: Option<IpAddr>,
}

impl Default for NetworkCatalog {
    fn default() -> Self {
        Self {
            networks: BTreeMap::new(),
            active: Vec::new(),
            next_id: 1, // zero is reserved for Any-address initially.
            initialized: false,
            default_v4: None,
            default_v6: None,
        }
    }
}

impl NetworkCatalog {
    pub(super) fn network(&self, key: &str) -> Option<LocalInterface> {
        self.networks.get(key).map(|network| network.local.clone())
    }
    pub(super) fn contains_address(&self, key: &str, address: IpAddr) -> bool {
        self.networks
            .get(key)
            .is_some_and(|network| network.addresses.contains(&address))
    }
    pub fn snapshot(
        &self,
        interface_filter: &Option<InterfaceFilterFn>,
        ip_filter: &Option<IpFilterFn>,
        network_types: &[NetworkType],
        include_loopback: bool,
    ) -> Option<Vec<LocalInterface>> {
        self.initialized.then(|| {
            self.select(
                interface_filter,
                ip_filter,
                network_types,
                include_loopback,
                self.default_v4,
                self.default_v6,
            )
        })
    }
    pub fn default_ip(&self, ipv6: bool) -> Option<IpAddr> {
        if ipv6 {
            self.default_v6
        } else {
            self.default_v4
        }
    }
    pub async fn update(
        &mut self,
        net: &Arc<Net>,
        interface_filter: &Option<InterfaceFilterFn>,
        ip_filter: &Option<IpFilterFn>,
        network_types: &[NetworkType],
        include_loopback: bool,
    ) -> Result<NetworkUpdate, util::Error> {
        // Do not mutate state until enumeration succeeded. A failure
        // must not make all formerly active Network objects disappear.
        let interfaces = net.try_get_interfaces().await?;
        let (default_v4, default_v6) = if net.is_virtual() {
            (None, None)
        } else {
            (default_local_ip(false), default_local_ip(true))
        };
        let mut incoming = BTreeMap::<String, (String, u32, Vec<IpAddr>)>::new();
        for interface in interfaces {
            Self::collect(&interface, &mut incoming);
        }
        let mut changed = !self.initialized || incoming.len() != self.active.len();
        let mut active = Vec::with_capacity(incoming.len());
        let mut type_changes = Vec::new();
        // New Network IDs are allocated in network-key order, before priority
        // sorting and per-interface/IPv6 candidate limits.
        for (key, (name, adapter_type, addresses)) in incoming {
            let Some(best_ip) = Network::best_ip(&addresses) else {
                continue;
            };
            if let Some(old) = self.networks.get_mut(&key) {
                changed |= !old.active
                    || old.addresses.len() != addresses.len()
                    || addresses.iter().any(|ip| !old.addresses.contains(ip));
                // Even an order-only change replaces the IP vector,
                // but does not itself trigger a new allocation sequence.
                old.addresses = addresses;
                old.local.ip = best_ip;
                if adapter_type != 0 && adapter_type != old.local.adapter_type {
                    changed |= !(is_cellular(adapter_type) && is_cellular(old.local.adapter_type));
                    old.local.adapter_type = adapter_type;
                    type_changes.push((key.clone(), adapter_type));
                }
                old.active = true;
            } else {
                let id = self.next_id;
                self.next_id = self.next_id.wrapping_add(1);
                self.networks.insert(
                    key.clone(),
                    Network {
                        local: LocalInterface {
                            name,
                            ip: best_ip,
                            adapter_type,
                            network_key: key.clone(),
                            network_preference: 0,
                            network_id: id,
                        },
                        addresses,
                        active: true,
                    },
                );
                changed = true;
            }
            active.push(key);
        }
        for (key, network) in &mut self.networks {
            network.active = active.binary_search(key).is_ok();
        }
        if changed {
            active.sort_by(|a, b| {
                let a = &self.networks[a].local;
                let b = &self.networks[b].local;
                a.adapter_type
                    .cmp(&b.adapter_type)
                    .then_with(|| ice_ip_precedence(b.ip).cmp(&ice_ip_precedence(a.ip)))
                    .then_with(|| a.network_key.cmp(&b.network_key))
            });
            for (rank, key) in active.iter().take(128).enumerate() {
                self.networks
                    .get_mut(key)
                    .expect("active network exists")
                    .local
                    .network_preference = 127 - rank as u8;
            }
            self.active = active;
        }
        self.initialized = true;
        self.default_v4 = default_v4;
        self.default_v6 = default_v6;
        let interfaces = self.select(
            interface_filter,
            ip_filter,
            network_types,
            include_loopback,
            default_v4,
            default_v6,
        );
        Ok(NetworkUpdate {
            changed,
            interfaces: Arc::new(interfaces),
            type_changes,
        })
    }

    fn collect(interface: &Interface, output: &mut BTreeMap<String, (String, u32, Vec<IpAddr>)>) {
        for ipnet in interface.addrs() {
            let ip = ipnet.addr();
            // GAA's normal snapshot excludes ignored IPv4 prefixes.
            if matches!(ipnet.network(), IpAddr::V4(ip) if u32::from(ip) < 0x0100_0000) {
                continue;
            }
            if let IpAddr::V6(ip) = ip {
                let bytes = ip.octets();
                if ip.is_unicast_link_local()
                    || (bytes[8] & 2 != 0 && bytes[11] == 0xff && bytes[12] == 0xfe)
                {
                    continue;
                }
            }
            let key = format!(
                "{}%{}/{}",
                interface.name(),
                ipnet.network(),
                ipnet.prefix_len()
            );
            let entry = output.entry(key).or_insert_with(|| {
                (
                    interface.name().to_owned(),
                    interface.adapter_type(),
                    Vec::new(),
                )
            });
            entry.2.push(ip);
        }
    }

    fn select(
        &self,
        interface_filter: &Option<InterfaceFilterFn>,
        ip_filter: &Option<IpFilterFn>,
        network_types: &[NetworkType],
        include_loopback: bool,
        default_v4: Option<IpAddr>,
        default_v6: Option<IpAddr>,
    ) -> Vec<LocalInterface> {
        if self.active.is_empty() {
            // Any networks exist independently of numbered
            // native Network objects, with id/preference zero and type ANY.
            return [
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            ]
            .into_iter()
            .filter(|ip| {
                network_types
                    .iter()
                    .any(|kind| kind.is_ipv4() == ip.is_ipv4())
                    && !interface_filter
                        .as_ref()
                        .is_some_and(|filter| !filter("any"))
                    && !ip_filter.as_ref().is_some_and(|filter| !filter(*ip))
            })
            .map(|ip| LocalInterface {
                name: "any".into(),
                ip,
                adapter_type: 32,
                network_key: format!("any%{ip}/0"),
                network_preference: 0,
                network_id: 0,
            })
            .collect();
        }
        let networks = self
            .active
            .iter()
            .map(|key| &self.networks[key])
            .filter(|network| {
                let local = &network.local;
                (include_loopback || !local.ip.is_loopback())
                    && !interface_filter
                        .as_ref()
                        .is_some_and(|filter| !filter(&local.name))
                    && !ip_filter.as_ref().is_some_and(|filter| !filter(local.ip))
                    && network_types
                        .iter()
                        .any(|kind| kind.is_ipv4() == local.ip.is_ipv4())
            })
            .collect::<Vec<_>>();
        let mut groups = HashMap::<(&str, bool), Vec<usize>>::new();
        for (index, network) in networks.iter().enumerate() {
            groups
                .entry((&network.local.name, network.local.ip.is_ipv6()))
                .or_default()
                .push(index);
        }
        let mut retained = HashSet::new();
        for ((_, ipv6), indices) in groups {
            // SDK's actual default is 20 per interface/family, then 5 IPv6
            // networks globally. Preserve original priority order in output.
            if indices.len() <= 20 {
                retained.extend(indices);
                continue;
            }
            let default = if ipv6 { default_v6 } else { default_v4 };
            let best = default.and_then(|ip| {
                indices
                    .iter()
                    .copied()
                    .find(|index| networks[*index].addresses.contains(&ip))
            });
            if let Some(index) = best {
                retained.insert(index);
            }
            let mut count = usize::from(best.is_some());
            for index in indices {
                if count == 20 {
                    break;
                }
                if retained.insert(index) {
                    count += 1;
                }
            }
        }
        let mut ipv6_count = 0;
        networks
            .into_iter()
            .enumerate()
            .filter_map(|(index, network)| {
                if !retained.contains(&index) {
                    return None;
                }
                if network.local.ip.is_ipv6() {
                    if ipv6_count == 5 {
                        return None;
                    }
                    ipv6_count += 1;
                }
                Some(network.local.clone())
            })
            .collect()
    }
}

fn is_cellular(adapter_type: u32) -> bool {
    matches!(adapter_type, 4 | 64 | 128 | 256 | 512)
}

fn default_local_ip(ipv6: bool) -> Option<IpAddr> {
    // Performs connect + getsockname, without sending a DNS packet.
    let (local, remote) = if ipv6 {
        (
            SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0),
            SocketAddr::new(
                Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888).into(),
                53,
            ),
        )
    } else {
        (
            SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0),
            SocketAddr::new(Ipv4Addr::new(8, 8, 8, 8).into(), 53),
        )
    };
    let socket = std::net::UdpSocket::bind(local).ok()?;
    socket.connect(remote).ok()?;
    socket.local_addr().ok().map(|address| address.ip())
}
