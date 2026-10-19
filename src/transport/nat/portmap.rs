//! Gateway port mappings for the ICE UDP port of a session: PCP (RFC 6887),
//! then NAT-PMP (RFC 6886), then UPnP-IGD (`AddPortMapping` on
//! WANIPConnection / WANPPPConnection).
//!
//! The ICE agent binds one UDP socket per network on an ephemeral port. The
//! port of the default-route host candidate is mapped once the agent
//! publishes it; no fixed local port is needed. The external address is
//! published as an extra srflx candidate. Leases last two hours, are renewed
//! at half life while the session lives, and are deleted when it closes.
use super::{Emit, candidates, route};
use anyhow::{Context, Result, bail, ensure};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;

const LIFETIME: Duration = Duration::from_secs(2 * 3600);
/// PCP and NAT-PMP server port.
const SERVER_PORT: u16 = 5351;
/// Retransmissions to the gateway (it is one hop away).
const GATEWAY_RETRANSMITS: [Duration; 2] = [Duration::from_millis(250), Duration::from_millis(500)];
/// Mapped ports per session (re-gathered sockets on the default route).
const MAX_MAPPINGS: usize = 2;
/// How long a gateway's protocol (or its absence) is remembered.
const GATEWAY_TTL: Duration = Duration::from_secs(10 * 60);
/// Deletion wait when a session closes.
const CLOSE_BUDGET: Duration = Duration::from_secs(2);
const DESCRIPTION: &str = "Farwing";

/// PCP MAP (RFC 6887 §11) for UDP.
mod pcp {
    use super::*;

    pub const VERSION: u8 = 2;
    pub const OPCODE_MAP: u8 = 1;
    pub const UDP: u8 = 17;
    pub const RESULT_UNSUPP_VERSION: u8 = 1;

    pub fn map_request(
        client: Ipv4Addr,
        nonce: &[u8; 12],
        internal_port: u16,
        suggested: Option<SocketAddrV4>,
        lifetime: u32,
    ) -> Vec<u8> {
        let mut message = vec![0u8; 60];
        message[0] = VERSION;
        message[1] = OPCODE_MAP;
        message[4..8].copy_from_slice(&lifetime.to_be_bytes());
        message[8..24].copy_from_slice(&client.to_ipv6_mapped().octets());
        message[24..36].copy_from_slice(nonce);
        message[36] = UDP;
        message[40..42].copy_from_slice(&internal_port.to_be_bytes());
        let (port, ip) = suggested.map_or((0, Ipv4Addr::UNSPECIFIED), |s| (s.port(), *s.ip()));
        message[42..44].copy_from_slice(&port.to_be_bytes());
        message[44..60].copy_from_slice(&ip.to_ipv6_mapped().octets());
        message
    }

    #[derive(Debug, PartialEq, Eq)]
    pub enum Response {
        Mapped {
            external: SocketAddrV4,
            lifetime: u32,
        },
        Failed(u8),
        /// A NAT-PMP-only gateway (version 0 reply) or another PCP version.
        UnsupportedVersion,
    }

    pub fn parse(packet: &[u8], nonce: &[u8; 12], internal_port: u16) -> Option<Response> {
        if packet.len() >= 4 && packet[0] == 0 {
            return Some(Response::UnsupportedVersion);
        }
        if packet.len() < 24 || packet[1] != 0x80 | OPCODE_MAP {
            return None;
        }
        let result = packet[3];
        if packet[0] != VERSION || result == RESULT_UNSUPP_VERSION {
            return Some(Response::UnsupportedVersion);
        }
        let payload = packet.get(24..60);
        if payload.is_some_and(|p| &p[..12] != nonce) {
            return None;
        }
        if result != 0 {
            return Some(Response::Failed(result));
        }
        let payload = payload?;
        if payload[12] != UDP || u16::from_be_bytes([payload[16], payload[17]]) != internal_port {
            return None;
        }
        let port = u16::from_be_bytes([payload[18], payload[19]]);
        let ip = Ipv6Addr::from(<[u8; 16]>::try_from(&payload[20..36]).ok()?).to_ipv4_mapped()?;
        Some(Response::Mapped {
            external: SocketAddrV4::new(ip, port),
            lifetime: u32::from_be_bytes(packet[4..8].try_into().ok()?),
        })
    }
}

/// NAT-PMP (RFC 6886) external address and UDP mapping.
mod natpmp {
    use super::*;

    pub const EXTERNAL_ADDRESS_REQUEST: [u8; 2] = [0, 0];

    pub fn map_request(internal_port: u16, external_port: u16, lifetime: u32) -> [u8; 12] {
        let mut message = [0u8; 12];
        message[1] = 1; // map UDP
        message[4..6].copy_from_slice(&internal_port.to_be_bytes());
        message[6..8].copy_from_slice(&external_port.to_be_bytes());
        message[8..12].copy_from_slice(&lifetime.to_be_bytes());
        message
    }

    /// The external IP, or the result code.
    pub fn parse_external_address(packet: &[u8]) -> Option<Result<Ipv4Addr, u16>> {
        if packet.len() < 12 || packet[0] != 0 || packet[1] != 128 {
            return None;
        }
        let result = u16::from_be_bytes([packet[2], packet[3]]);
        if result != 0 {
            return Some(Err(result));
        }
        Some(Ok(Ipv4Addr::new(
            packet[8], packet[9], packet[10], packet[11],
        )))
    }

    /// The mapped external port and lifetime, or the result code.
    pub fn parse_map(packet: &[u8], internal_port: u16) -> Option<Result<(u16, u32), u16>> {
        if packet.len() < 16 || packet[0] != 0 || packet[1] != 129 {
            return None;
        }
        if u16::from_be_bytes([packet[8], packet[9]]) != internal_port {
            return None;
        }
        let result = u16::from_be_bytes([packet[2], packet[3]]);
        if result != 0 {
            return Some(Err(result));
        }
        Some(Ok((
            u16::from_be_bytes([packet[10], packet[11]]),
            u32::from_be_bytes([packet[12], packet[13], packet[14], packet[15]]),
        )))
    }
}

/// UPnP-IGD: SSDP discovery and the SOAP calls of WANIPConnection.
mod upnp {
    use super::*;

    pub const SEARCH_TARGETS: [&str; 2] = [
        "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
        "urn:schemas-upnp-org:device:InternetGatewayDevice:2",
    ];
    /// Preferred first.
    const SERVICE_TYPES: [&str; 3] = [
        "urn:schemas-upnp-org:service:WANIPConnection:2",
        "urn:schemas-upnp-org:service:WANIPConnection:1",
        "urn:schemas-upnp-org:service:WANPPPConnection:1",
    ];
    /// ConflictInMappingEntry.
    pub const CONFLICT: u16 = 718;

    pub fn search_request(target: &str) -> String {
        format!(
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: {target}\r\n\r\n"
        )
    }

    /// The LOCATION header of a successful SSDP search response.
    pub fn location(response: &str) -> Option<&str> {
        let mut lines = response.lines();
        let status = lines.next()?;
        if !status.starts_with("HTTP/1.") || !status.contains(" 200") {
            return None;
        }
        lines.find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("location")
                .then(|| value.trim())
        })
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Service {
        pub kind: String,
        pub control: reqwest::Url,
    }

    /// The preferred WAN connection service of a device description, with
    /// its control URL on the description's own host.
    pub fn service(description: &str, location: &reqwest::Url) -> Option<Service> {
        let base = element(description, "URLBase")
            .and_then(|base| reqwest::Url::parse(&unescape(base)).ok())
            .unwrap_or_else(|| location.clone());
        let mut found: Vec<(usize, Service)> = blocks(description, "service")
            .into_iter()
            .filter_map(|block| {
                let kind = element(block, "serviceType")?;
                let rank = SERVICE_TYPES.iter().position(|t| *t == kind)?;
                let control = base.join(&unescape(element(block, "controlURL")?)).ok()?;
                (control.scheme() == "http" && control.host() == location.host()).then(|| {
                    (
                        rank,
                        Service {
                            kind: kind.to_owned(),
                            control,
                        },
                    )
                })
            })
            .collect();
        found.sort_by_key(|(rank, _)| *rank);
        found.into_iter().next().map(|(_, service)| service)
    }

    pub fn body(kind: &str, action: &str, arguments: &[(&str, String)]) -> String {
        let arguments: String = arguments
            .iter()
            .map(|(name, value)| format!("<{name}>{value}</{name}>"))
            .collect();
        format!(
            "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{kind}\">{arguments}</u:{action}></s:Body></s:Envelope>\r\n"
        )
    }

    pub fn add_arguments(
        external_port: u16,
        internal: SocketAddrV4,
        lease: u32,
    ) -> Vec<(&'static str, String)> {
        vec![
            ("NewRemoteHost", String::new()),
            ("NewExternalPort", external_port.to_string()),
            ("NewProtocol", "UDP".into()),
            ("NewInternalPort", internal.port().to_string()),
            ("NewInternalClient", internal.ip().to_string()),
            ("NewEnabled", "1".into()),
            ("NewPortMappingDescription", DESCRIPTION.into()),
            ("NewLeaseDuration", lease.to_string()),
        ]
    }

    pub fn delete_arguments(external_port: u16) -> Vec<(&'static str, String)> {
        vec![
            ("NewRemoteHost", String::new()),
            ("NewExternalPort", external_port.to_string()),
            ("NewProtocol", "UDP".into()),
        ]
    }

    /// Text of the first element with this local name (any namespace prefix).
    pub fn element<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
        let mut rest = xml;
        while let Some(start) = rest.find('<') {
            let after = &rest[start + 1..];
            let end = after.find('>')?;
            let tag = &after[..end];
            let tag_name = tag.split_whitespace().next().unwrap_or_default();
            let local = tag_name.rsplit(':').next().unwrap_or(tag_name);
            rest = &after[end + 1..];
            if local == name && !tag_name.starts_with('/') && !tag.ends_with('/') {
                return Some(rest[..rest.find('<')?].trim());
            }
        }
        None
    }

    /// Contents of each `<name>...</name>` block.
    fn blocks<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
        let (open, close) = (format!("<{name}>"), format!("</{name}>"));
        let mut found = Vec::new();
        let mut rest = xml;
        while let Some(start) = rest.find(&open) {
            let after = &rest[start + open.len()..];
            let Some(end) = after.find(&close) else {
                break;
            };
            found.push(&after[..end]);
            rest = &after[end + close.len()..];
        }
        found
    }

    fn unescape(text: &str) -> String {
        text.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    }

    /// A UPnP error code from a SOAP fault.
    #[derive(Debug)]
    pub struct Fault(pub u16);

    impl std::fmt::Display for Fault {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "UPnP error {}", self.0)
        }
    }

    impl std::error::Error for Fault {}
}

/// How a mapping was made, and what deleting it needs.
#[derive(Clone, Debug)]
enum Method {
    Pcp { nonce: [u8; 12] },
    NatPmp,
    Upnp(upnp::Service),
}

/// What a gateway was found to speak.
#[derive(Clone, Debug)]
enum Known {
    Pcp,
    NatPmp,
    Upnp(upnp::Service),
    Nothing,
}

static GATEWAYS: LazyLock<Mutex<HashMap<Ipv4Addr, (Known, Instant)>>> =
    LazyLock::new(Mutex::default);

fn remember(gateway: Ipv4Addr, known: Known) {
    let mut gateways = GATEWAYS.lock().unwrap_or_else(|e| e.into_inner());
    gateways.retain(|_, (_, at)| at.elapsed() < GATEWAY_TTL);
    gateways.insert(gateway, (known, Instant::now()));
}

fn known(gateway: Ipv4Addr) -> Option<Known> {
    GATEWAYS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&gateway)
        .filter(|(_, at)| at.elapsed() < GATEWAY_TTL)
        .map(|(known, _)| known.clone())
}

/// A mapping on the gateway.
#[derive(Clone, Debug)]
struct Lease {
    method: Method,
    gateway: Ipv4Addr,
    internal: SocketAddrV4,
    external: SocketAddrV4,
    lifetime: Duration,
}

/// Send `request` to the gateway's PCP/NAT-PMP port until `parse` accepts a
/// reply; `None` when it never answers.
async fn exchange<T>(
    local: Ipv4Addr,
    gateway: Ipv4Addr,
    request: &[u8],
    mut parse: impl FnMut(&[u8]) -> Option<T>,
) -> Result<Option<T>> {
    let socket = UdpSocket::bind((local, 0)).await?;
    let server = SocketAddr::from((gateway, SERVER_PORT));
    let mut buffer = [0u8; 1100];
    for wait in GATEWAY_RETRANSMITS {
        socket.send_to(request, server).await?;
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await {
                Err(_) => break,
                Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionReset => {
                    // ICMP port unreachable: nothing listens on 5351.
                    return Ok(None);
                }
                Ok(Err(error)) => return Err(error.into()),
                Ok(Ok((length, from))) if from == server => {
                    if let Some(reply) = parse(&buffer[..length]) {
                        return Ok(Some(reply));
                    }
                }
                Ok(Ok(_)) => {}
            }
        }
    }
    Ok(None)
}

enum Attempt {
    Mapped(Lease),
    /// The protocol is not spoken here; try the next one.
    Unsupported,
    /// Nothing answers on the PCP/NAT-PMP port.
    Silent,
}

async fn pcp_map(
    gateway: Ipv4Addr,
    internal: SocketAddrV4,
    nonce: [u8; 12],
    suggested: Option<SocketAddrV4>,
    lifetime: u32,
) -> Result<Attempt> {
    let request = pcp::map_request(*internal.ip(), &nonce, internal.port(), suggested, lifetime);
    let reply = exchange(*internal.ip(), gateway, &request, |packet| {
        pcp::parse(packet, &nonce, internal.port())
    })
    .await?;
    Ok(match reply {
        None => Attempt::Silent,
        Some(pcp::Response::UnsupportedVersion) => Attempt::Unsupported,
        Some(pcp::Response::Failed(code)) => bail!("PCP result {code}"),
        Some(pcp::Response::Mapped { external, lifetime }) => Attempt::Mapped(Lease {
            method: Method::Pcp { nonce },
            gateway,
            internal,
            external,
            lifetime: Duration::from_secs(u64::from(lifetime)),
        }),
    })
}

async fn natpmp_map(
    gateway: Ipv4Addr,
    internal: SocketAddrV4,
    suggested_port: u16,
    lifetime: u32,
) -> Result<Attempt> {
    let local = *internal.ip();
    let Some(address) = exchange(
        local,
        gateway,
        &natpmp::EXTERNAL_ADDRESS_REQUEST,
        natpmp::parse_external_address,
    )
    .await?
    else {
        return Ok(Attempt::Silent);
    };
    let external_ip = address.map_err(|code| anyhow::anyhow!("NAT-PMP result {code}"))?;
    let request = natpmp::map_request(internal.port(), suggested_port, lifetime);
    let reply = exchange(local, gateway, &request, |packet| {
        natpmp::parse_map(packet, internal.port())
    })
    .await?
    .context("NAT-PMP mapping not answered")?;
    let (port, lifetime) = reply.map_err(|code| anyhow::anyhow!("NAT-PMP result {code}"))?;
    Ok(Attempt::Mapped(Lease {
        method: Method::NatPmp,
        gateway,
        internal,
        external: SocketAddrV4::new(external_ip, port),
        lifetime: Duration::from_secs(u64::from(lifetime)),
    }))
}

fn http(local: Ipv4Addr) -> Result<reqwest::Client> {
    crate::transport::http_client()
        .no_proxy()
        .http1_only()
        .local_address(IpAddr::V4(local))
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("UPnP HTTP client")
}

async fn http_text(response: reqwest::Response) -> Result<(reqwest::StatusCode, String)> {
    let status = response.status();
    let bytes = response.bytes().await?;
    ensure!(bytes.len() <= 256 * 1024, "UPnP response too large");
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

/// Find the gateway's IGD by SSDP and read its WAN connection service.
async fn upnp_discover(local: Ipv4Addr, gateway: Ipv4Addr) -> Result<Option<upnp::Service>> {
    let socket = UdpSocket::bind((local, 0)).await?;
    for target in upnp::SEARCH_TARGETS {
        socket
            .send_to(
                upnp::search_request(target).as_bytes(),
                "239.255.255.250:1900",
            )
            .await?;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    let mut buffer = [0u8; 2048];
    let mut locations = Vec::new();
    while let Ok(received) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await
    {
        let Ok((length, from)) = received else {
            continue;
        };
        let text = String::from_utf8_lossy(&buffer[..length]);
        let Some(location) = upnp::location(&text).and_then(|l| reqwest::Url::parse(l).ok()) else {
            continue;
        };
        // Only the responder's own description, and the gateway first.
        if location.scheme() != "http"
            || location.host_str() != Some(from.ip().to_string().as_str())
            || locations.contains(&location)
        {
            continue;
        }
        let gateway_reply = from.ip() == IpAddr::V4(gateway);
        locations.push(location.clone());
        if gateway_reply {
            locations.rotate_right(1);
            break;
        }
    }
    let client = http(local)?;
    for location in locations {
        let Ok(response) = client.get(location.clone()).send().await else {
            continue;
        };
        let Ok((status, text)) = http_text(response).await else {
            continue;
        };
        if status.is_success()
            && let Some(service) = upnp::service(&text, &location)
        {
            return Ok(Some(service));
        }
    }
    Ok(None)
}

async fn soap(
    client: &reqwest::Client,
    service: &upnp::Service,
    action: &str,
    arguments: &[(&str, String)],
) -> Result<String> {
    let response = client
        .post(service.control.clone())
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        .header("SOAPAction", format!("\"{}#{action}\"", service.kind))
        .body(upnp::body(&service.kind, action, arguments))
        .send()
        .await?;
    let (status, text) = http_text(response).await?;
    if !status.is_success() {
        let code = upnp::element(&text, "errorCode")
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        return Err(upnp::Fault(code).into());
    }
    Ok(text)
}

async fn upnp_map(
    service: upnp::Service,
    gateway: Ipv4Addr,
    internal: SocketAddrV4,
    suggested_port: u16,
) -> Result<Lease> {
    let client = http(*internal.ip())?;
    let reply = soap(&client, &service, "GetExternalIPAddress", &[]).await?;
    let external_ip: Ipv4Addr = upnp::element(&reply, "NewExternalIPAddress")
        .context("UPnP gateway has no external address")?
        .parse()
        .context("UPnP external address")?;
    let mut port = suggested_port;
    for attempt in 0..3 {
        let arguments = upnp::add_arguments(port, internal, LIFETIME.as_secs() as u32);
        match soap(&client, &service, "AddPortMapping", &arguments).await {
            Ok(_) => {
                return Ok(Lease {
                    method: Method::Upnp(service),
                    gateway,
                    internal,
                    external: SocketAddrV4::new(external_ip, port),
                    lifetime: LIFETIME,
                });
            }
            Err(error)
                if attempt < 2
                    && error
                        .downcast_ref::<upnp::Fault>()
                        .is_some_and(|fault| fault.0 == upnp::CONFLICT) =>
            {
                // Taken by another host: any free high port will do.
                port = rand::random_range(49152..=65535);
            }
            // 725 OnlyPermanentLeasesSupported included: a permanent mapping
            // would outlive a crashed session.
            Err(error) => return Err(error),
        }
    }
    bail!("UPnP port mapping conflicts")
}

/// Map `internal` on `gateway`, trying the protocol known to work first,
/// otherwise PCP, NAT-PMP and UPnP-IGD in turn.
async fn map(
    gateway: Ipv4Addr,
    internal: SocketAddrV4,
    suggested: Option<SocketAddrV4>,
) -> Result<Lease> {
    let lifetime = LIFETIME.as_secs() as u32;
    let suggested_port = suggested.map_or(internal.port(), |s| s.port());
    let known = known(gateway);
    if matches!(known, Some(Known::Nothing)) {
        bail!("gateway offers no port mapping");
    }
    // A NAT-PMP gateway answers a PCP request with "unsupported version";
    // silence on the port means neither protocol is there. A refusal still
    // leaves UPnP-IGD to try.
    let mut try_natpmp = matches!(known, None | Some(Known::NatPmp));
    let mut refused = None;
    if matches!(known, None | Some(Known::Pcp)) {
        match pcp_map(gateway, internal, rand::random(), suggested, lifetime).await {
            Ok(Attempt::Mapped(lease)) => {
                remember(gateway, Known::Pcp);
                return Ok(lease);
            }
            Ok(Attempt::Unsupported) => {}
            Ok(Attempt::Silent) => try_natpmp = false,
            Err(error) => {
                try_natpmp = false;
                refused = Some(error);
            }
        }
    }
    if try_natpmp {
        match natpmp_map(gateway, internal, suggested_port, lifetime).await {
            Ok(Attempt::Mapped(lease)) => {
                remember(gateway, Known::NatPmp);
                return Ok(lease);
            }
            Ok(Attempt::Unsupported | Attempt::Silent) => {}
            Err(error) => refused = Some(error),
        }
    }
    let service = match known {
        Some(Known::Upnp(service)) => Some(service),
        _ => upnp_discover(*internal.ip(), gateway).await?,
    };
    let Some(service) = service else {
        remember(gateway, Known::Nothing);
        return Err(refused.unwrap_or_else(|| anyhow::anyhow!("gateway offers no port mapping")));
    };
    remember(gateway, Known::Upnp(service.clone()));
    upnp_map(service, gateway, internal, suggested_port).await
}

impl Lease {
    /// Renew with the same external address.
    async fn renew(&self) -> Result<Lease> {
        let lifetime = LIFETIME.as_secs() as u32;
        let attempt = match &self.method {
            Method::Pcp { nonce } => {
                pcp_map(
                    self.gateway,
                    self.internal,
                    *nonce,
                    Some(self.external),
                    lifetime,
                )
                .await?
            }
            Method::NatPmp => {
                natpmp_map(self.gateway, self.internal, self.external.port(), lifetime).await?
            }
            Method::Upnp(service) => Attempt::Mapped(
                upnp_map(
                    service.clone(),
                    self.gateway,
                    self.internal,
                    self.external.port(),
                )
                .await?,
            ),
        };
        match attempt {
            Attempt::Mapped(lease) => Ok(lease),
            Attempt::Unsupported | Attempt::Silent => bail!("gateway stopped answering"),
        }
    }

    async fn delete(&self) -> Result<()> {
        match &self.method {
            Method::Pcp { nonce } => {
                pcp_map(self.gateway, self.internal, *nonce, Some(self.external), 0).await?;
            }
            Method::NatPmp => {
                let request = natpmp::map_request(self.internal.port(), 0, 0);
                exchange(*self.internal.ip(), self.gateway, &request, |packet| {
                    natpmp::parse_map(packet, self.internal.port())
                })
                .await?;
            }
            Method::Upnp(service) => {
                let client = http(*self.internal.ip())?;
                soap(
                    &client,
                    service,
                    "DeletePortMapping",
                    &upnp::delete_arguments(self.external.port()),
                )
                .await?;
            }
        }
        Ok(())
    }

    fn protocol(&self) -> &'static str {
        match self.method {
            Method::Pcp { .. } => "PCP",
            Method::NatPmp => "NAT-PMP",
            Method::Upnp(_) => "UPnP-IGD",
        }
    }
}

/// An external address worth advertising: public IPv4, not a second NAT.
fn advertisable(ip: &Ipv4Addr) -> bool {
    let shared = ip.octets()[0] == 100 && ip.octets()[1] & 0xc0 == 64;
    !(ip.is_private()
        || shared
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast())
}

/// Keep one mapping alive until `stop`, publishing its srflx candidate
/// unless the agent already published that address (a port-preserving
/// endpoint-independent NAT).
async fn maintain(
    gateway: Ipv4Addr,
    internal: SocketAddrV4,
    host: RTCIceCandidateInit,
    emit: Emit,
    reflexive: Arc<Mutex<Vec<SocketAddrV4>>>,
    stop: CancellationToken,
) {
    let publish = |lease: &Lease| {
        if reflexive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&lease.external)
        {
            tracing::debug!(external = %lease.external, "gateway port mapping matches a published srflx candidate");
            return;
        }
        if let Some(candidate) = candidates::mapped(&host, lease.external) {
            tracing::info!(
                protocol = lease.protocol(),
                %internal,
                external = %lease.external,
                lifetime_s = lease.lifetime.as_secs(),
                "gateway port mapping published as srflx candidate"
            );
            emit(candidate);
        }
    };
    // Not cancelled midway: an unanswered request may still have created a
    // mapping that the deletion below must remove.
    let mut lease = match map(gateway, internal, None).await {
        Ok(lease) => lease,
        Err(error) => {
            tracing::debug!(error = %format!("{error:#}"), %gateway, "no gateway port mapping");
            return;
        }
    };
    if !advertisable(lease.external.ip()) {
        tracing::debug!(external = %lease.external, "gateway is behind another NAT; mapping not advertised");
    } else if !stop.is_cancelled() {
        publish(&lease);
    }
    while advertisable(lease.external.ip()) {
        let renew_after = (lease.lifetime / 2).max(Duration::from_secs(60));
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = tokio::time::sleep(renew_after) => {}
        }
        match lease.renew().await {
            Ok(renewed) => {
                if renewed.external != lease.external {
                    publish(&renewed);
                }
                lease = renewed;
            }
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "gateway port mapping renewal failed");
            }
        }
    }
    match tokio::time::timeout(CLOSE_BUDGET, lease.delete()).await {
        Ok(Ok(())) => tracing::debug!(external = %lease.external, "gateway port mapping deleted"),
        Ok(Err(error)) => {
            tracing::debug!(error = %format!("{error:#}"), "gateway port mapping deletion failed; it expires with its lease")
        }
        Err(_) => {
            tracing::debug!("gateway port mapping deletion timed out; it expires with its lease")
        }
    }
}

/// The mappings of one peer connection.
pub(super) struct Session {
    enabled: bool,
    stop: CancellationToken,
    /// Mapped ports and their tasks.
    state: Mutex<(Vec<u16>, Vec<tokio::task::JoinHandle<()>>)>,
    /// srflx addresses the agent published.
    reflexive: Arc<Mutex<Vec<SocketAddrV4>>>,
}

impl Session {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            stop: CancellationToken::new(),
            state: Mutex::new((Vec::new(), Vec::new())),
            reflexive: Arc::default(),
        }
    }

    /// Map the port of a private UDP/IPv4 host candidate on the default
    /// route. srflx candidates are remembered; others are ignored.
    pub(super) fn observe(&self, candidate: &RTCIceCandidateInit, emit: &Emit) {
        if !self.enabled || self.stop.is_cancelled() {
            return;
        }
        if let Some(address) = candidates::udp_v4(candidate, "srflx") {
            let mut reflexive = self.reflexive.lock().unwrap_or_else(|e| e.into_inner());
            if reflexive.len() < 16 && !reflexive.contains(&address) {
                reflexive.push(address);
            }
            return;
        }
        let Some(internal) = candidates::udp_v4(candidate, "host") else {
            return;
        };
        if !internal.ip().is_private() {
            return;
        }
        let Some(route::Network {
            local,
            gateway: Some(gateway),
        }) = route::current()
        else {
            return;
        };
        if local != *internal.ip() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.0.contains(&internal.port()) || state.0.len() >= MAX_MAPPINGS {
            return;
        }
        state.0.push(internal.port());
        state.1.push(runtime.spawn(maintain(
            gateway,
            internal,
            candidate.clone(),
            emit.clone(),
            self.reflexive.clone(),
            self.stop.clone(),
        )));
    }

    pub(super) fn stop(&self) {
        self.stop.cancel();
    }

    /// Stop and wait (bounded) for the deletions.
    pub(super) async fn close(&self) {
        self.stop.cancel();
        let tasks = std::mem::take(&mut self.state.lock().unwrap_or_else(|e| e.into_inner()).1);
        if tasks.is_empty() {
            return;
        }
        let all = async {
            for task in tasks {
                let _ = task.await;
            }
        };
        let _ = tokio::time::timeout(CLOSE_BUDGET + Duration::from_millis(500), all).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcp_map_request_layout() {
        let nonce = [9u8; 12];
        let request = pcp::map_request(
            Ipv4Addr::new(192, 168, 1, 5),
            &nonce,
            50000,
            Some("203.0.113.7:50001".parse().unwrap()),
            7200,
        );
        assert_eq!(request.len(), 60);
        assert_eq!(&request[..4], &[2, 1, 0, 0]);
        assert_eq!(&request[4..8], &7200u32.to_be_bytes());
        assert_eq!(
            &request[8..24],
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 192, 168, 1, 5]
        );
        assert_eq!(&request[24..36], &nonce);
        assert_eq!(&request[36..40], &[17, 0, 0, 0]);
        assert_eq!(&request[40..44], &[0xc3, 0x50, 0xc3, 0x51]);
        assert_eq!(&request[54..60], &[0xff, 0xff, 203, 0, 113, 7]);
        // No suggestion: port 0 and the IPv4-mapped unspecified address.
        let any = pcp::map_request(Ipv4Addr::new(192, 168, 1, 5), &nonce, 50000, None, 7200);
        assert_eq!(
            &any[42..60],
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0, 0, 0, 0]
        );
    }

    fn pcp_response(nonce: &[u8; 12], result: u8, port: u16) -> Vec<u8> {
        let mut packet = vec![0u8; 60];
        packet[0] = 2;
        packet[1] = 0x81;
        packet[3] = result;
        packet[4..8].copy_from_slice(&3600u32.to_be_bytes());
        packet[24..36].copy_from_slice(nonce);
        packet[36] = 17;
        packet[40..42].copy_from_slice(&50000u16.to_be_bytes());
        packet[42..44].copy_from_slice(&port.to_be_bytes());
        packet[44..60].copy_from_slice(&Ipv4Addr::new(203, 0, 113, 7).to_ipv6_mapped().octets());
        packet
    }

    #[test]
    fn pcp_responses() {
        let nonce = [9u8; 12];
        assert_eq!(
            pcp::parse(&pcp_response(&nonce, 0, 50001), &nonce, 50000),
            Some(pcp::Response::Mapped {
                external: "203.0.113.7:50001".parse().unwrap(),
                lifetime: 3600
            })
        );
        // Another request's nonce or internal port.
        assert_eq!(
            pcp::parse(&pcp_response(&[1; 12], 0, 50001), &nonce, 50000),
            None
        );
        assert_eq!(
            pcp::parse(&pcp_response(&nonce, 0, 50001), &nonce, 40000),
            None
        );
        // NO_RESOURCES.
        assert_eq!(
            pcp::parse(&pcp_response(&nonce, 8, 0), &nonce, 50000),
            Some(pcp::Response::Failed(8))
        );
        // A NAT-PMP gateway answers version 2 with "unsupported version".
        assert_eq!(
            pcp::parse(&[0, 0x81, 0, 1, 0, 0, 0, 0], &nonce, 50000),
            Some(pcp::Response::UnsupportedVersion)
        );
    }

    #[test]
    fn natpmp_messages() {
        assert_eq!(
            natpmp::map_request(50000, 50000, 7200),
            [0, 1, 0, 0, 0xc3, 0x50, 0xc3, 0x50, 0, 0, 0x1c, 0x20]
        );
        // Deletion: external port and lifetime zero.
        assert_eq!(
            natpmp::map_request(50000, 0, 0),
            [0, 1, 0, 0, 0xc3, 0x50, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            natpmp::parse_external_address(&[0, 128, 0, 0, 0, 0, 0, 5, 203, 0, 113, 7]),
            Some(Ok(Ipv4Addr::new(203, 0, 113, 7)))
        );
        assert_eq!(
            natpmp::parse_external_address(&[0, 128, 0, 3, 0, 0, 0, 5, 0, 0, 0, 0]),
            Some(Err(3))
        );
        let mapped = [
            0, 129, 0, 0, 0, 0, 0, 5, 0xc3, 0x50, 0xc3, 0x51, 0, 0, 0x1c, 0x20,
        ];
        assert_eq!(natpmp::parse_map(&mapped, 50000), Some(Ok((50001, 7200))));
        assert_eq!(natpmp::parse_map(&mapped, 40000), None);
        let refused = [0, 129, 0, 2, 0, 0, 0, 5, 0xc3, 0x50, 0, 0, 0, 0, 0, 0];
        assert_eq!(natpmp::parse_map(&refused, 50000), Some(Err(2)));
    }

    #[test]
    fn ssdp_location() {
        let response = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\nLocation: http://192.168.1.1:5000/rootDesc.xml\r\n\r\n";
        assert_eq!(
            upnp::location(response),
            Some("http://192.168.1.1:5000/rootDesc.xml")
        );
        assert_eq!(
            upnp::location("NOTIFY * HTTP/1.1\r\nLOCATION: http://x/\r\n\r\n"),
            None
        );
        assert!(
            upnp::search_request(upnp::SEARCH_TARGETS[0])
                .contains("\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n")
        );
    }

    #[test]
    fn device_description_service() {
        let location = reqwest::Url::parse("http://192.168.1.1:5000/rootDesc.xml").unwrap();
        let description = r#"<?xml version="1.0"?><root xmlns="urn:schemas-upnp-org:device-1-0">
<device><serviceList>
<service><serviceType>urn:schemas-upnp-org:service:WANCommonInterfaceConfig:1</serviceType><controlURL>/ctl/CmnIfCfg</controlURL></service>
<service><serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType><controlURL>/ctl/PPP</controlURL></service>
<service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>ctl/IPConn?a=1&amp;b=2</controlURL></service>
</serviceList></device></root>"#;
        let service = upnp::service(description, &location).unwrap();
        assert_eq!(
            service.kind,
            "urn:schemas-upnp-org:service:WANIPConnection:1"
        );
        assert_eq!(
            service.control.as_str(),
            "http://192.168.1.1:5000/ctl/IPConn?a=1&b=2"
        );
        // URLBase is honoured; a control URL on another host is not.
        let based = "<root><URLBase>http://192.168.1.1:5000/igd/</URLBase><service><serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType><controlURL>ppp</controlURL></service>\
<service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:2</serviceType><controlURL>http://10.0.0.9/ctl</controlURL></service></root>";
        let service = upnp::service(based, &location).unwrap();
        assert_eq!(service.control.as_str(), "http://192.168.1.1:5000/igd/ppp");
        assert_eq!(upnp::service("<root></root>", &location), None);
    }

    #[test]
    fn soap_messages() {
        let internal: SocketAddrV4 = "192.168.1.5:50000".parse().unwrap();
        let body = upnp::body(
            "urn:schemas-upnp-org:service:WANIPConnection:1",
            "AddPortMapping",
            &upnp::add_arguments(50000, internal, 7200),
        );
        assert!(body.contains(
            "<u:AddPortMapping xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\">"
        ));
        assert!(body.contains("<NewRemoteHost></NewRemoteHost><NewExternalPort>50000</NewExternalPort><NewProtocol>UDP</NewProtocol><NewInternalPort>50000</NewInternalPort><NewInternalClient>192.168.1.5</NewInternalClient><NewEnabled>1</NewEnabled><NewPortMappingDescription>Farwing</NewPortMappingDescription><NewLeaseDuration>7200</NewLeaseDuration>"));
        let reply = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:GetExternalIPAddressResponse xmlns:u="urn:schemas-upnp-org:service:WANIPConnection:1"><NewExternalIPAddress>203.0.113.7</NewExternalIPAddress></u:GetExternalIPAddressResponse></s:Body></s:Envelope>"#;
        assert_eq!(
            upnp::element(reply, "NewExternalIPAddress"),
            Some("203.0.113.7")
        );
        let fault = r#"<s:Envelope><s:Body><s:Fault><faultcode>s:Client</faultcode><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>718</errorCode><errorDescription>ConflictInMappingEntry</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#;
        assert_eq!(upnp::element(fault, "errorCode"), Some("718"));
        assert_eq!(upnp::element("<a/><b>x</b>", "a"), None);
    }

    #[test]
    fn only_public_external_addresses_are_advertised() {
        assert!(advertisable(&Ipv4Addr::new(203, 0, 113, 7)));
        for ip in [
            "192.168.0.2",
            "10.1.2.3",
            "172.16.0.1",
            "100.64.0.1",
            "100.127.255.254",
            "169.254.1.1",
            "0.0.0.0",
        ] {
            assert!(!advertisable(&ip.parse().unwrap()), "{ip}");
        }
        assert!(advertisable(&Ipv4Addr::new(100, 128, 0, 1)));
    }

    #[tokio::test]
    async fn ignores_unsuitable_candidates() {
        let session = Session::new(true);
        let emit: Emit = Arc::new(|candidate: RTCIceCandidateInit| {
            assert!(candidate.candidate.is_empty(), "nothing to publish")
        });
        for line in [
            "candidate:1 1 udp 2122260223 203.0.113.7 50000 typ host",
            "candidate:1 1 udp 1686052607 203.0.113.7 40000 typ srflx raddr 192.168.1.5 rport 50000",
            "candidate:1 1 tcp 1518280447 192.168.1.5 9 typ host tcptype active",
        ] {
            session.observe(
                &RTCIceCandidateInit {
                    candidate: line.into(),
                    ..Default::default()
                },
                &emit,
            );
        }
        assert!(session.state.lock().unwrap().0.is_empty());
        // The srflx address is remembered against duplicate publication.
        assert_eq!(
            *session.reflexive.lock().unwrap(),
            vec!["203.0.113.7:40000".parse::<SocketAddrV4>().unwrap()]
        );
        let disabled = Session::new(false);
        disabled.observe(
            &RTCIceCandidateInit {
                candidate: "candidate:1 1 udp 2122260223 192.168.1.5 50000 typ host".into(),
                ..Default::default()
            },
            &emit,
        );
        assert!(disabled.state.lock().unwrap().0.is_empty());
        session.close().await;
    }
}
