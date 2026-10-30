//! Server settings (`docs/protocol/deployment-design.md` §2). They live in
//! the database as one JSON document, are edited in the web console or
//! through the administration API, and are applied while the server runs
//! (`crate::runtime`). Only the data directory and an optional fixed
//! listen address come from the command line.

use anyhow::{Context, Result, anyhow, bail};
use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

/// Why a setting was refused: the field (dotted path) and a problem code the
/// web console translates, with an English explanation for logs and curl.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SettingError {
    pub field: String,
    /// `required`, `invalid`, `out_of_range`, `conflict`, `unavailable` or
    /// `excludes_you`.
    pub problem: &'static str,
    pub message: String,
}

impl SettingError {
    pub fn new(field: &str, problem: &'static str, message: impl Into<String>) -> Self {
        Self {
            field: field.to_owned(),
            problem,
            message: message.into(),
        }
    }
    fn required(field: &str, message: impl Into<String>) -> Self {
        Self::new(field, "required", message)
    }
    fn invalid(field: &str, message: impl Into<String>) -> Self {
        Self::new(field, "invalid", message)
    }
    fn range(field: &str, message: impl Into<String>) -> Self {
        Self::new(field, "out_of_range", message)
    }
}

impl std::fmt::Display for SettingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

impl std::error::Error for SettingError {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Database, wallpapers, certificates (`--data-dir`); not a setting.
    #[serde(skip)]
    pub data_dir: PathBuf,
    /// Socket address of the HTTPS/WSS listener.
    pub listen: SocketAddr,
    /// `listen` was fixed by `--listen` / `FARWING_LISTEN` and cannot be
    /// changed online.
    #[serde(skip)]
    pub listen_locked: bool,
    /// Public `https://` base URL used to build realtime endpoints.
    pub public_url: String,
    /// Name shown in `GET /api/v2/server` and the web console.
    pub instance_name: String,
    /// Peers (CIDRs) whose `X-Forwarded-For` header is honored.
    pub trusted_proxies: Vec<IpNet>,
    /// `tracing` filter directive, e.g. `info` or `farwing_server=debug`.
    /// `RUST_LOG` overrides it.
    pub log: String,
    /// Accounts may be registered with an invitation code.
    pub registration: bool,
    pub tls: TlsConfig,
    pub guest: GuestConfig,
    pub turn: TurnConfig,
    pub sfu: SfuSettings,
    pub egress: EgressConfig,
    pub limits: LimitsConfig,
    pub web: WebConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data"),
            listen: SocketAddr::from(([0, 0, 0, 0], 8443)),
            listen_locked: false,
            public_url: "https://localhost:8443".to_owned(),
            instance_name: "farwing".to_owned(),
            trusted_proxies: Vec::new(),
            log: "info".to_owned(),
            registration: false,
            tls: TlsConfig::default(),
            guest: GuestConfig::default(),
            turn: TurnConfig::default(),
            sfu: SfuSettings::default(),
            egress: EgressConfig::default(),
            limits: LimitsConfig::default(),
            web: WebConfig::default(),
        }
    }
}

/// Multi-viewer forwarding (`docs/protocol/deployment-design.md` §5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SfuSettings {
    /// Hosts may send their picture once to a forwarder here (or on an edge
    /// node) instead of once per viewer.
    pub enabled: bool,
    /// Viewers of one host from which it forwards.
    pub threshold: u32,
    /// Hosts may choose compatible mode, in which the forwarder can see the
    /// picture (browsers without encoded transforms need it).
    pub allow_compatible: bool,
    /// UDP port of the forwarder (all its connections share it); edge nodes
    /// use it unless set otherwise.
    pub port: u16,
}

impl Default for SfuSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: 3,
            allow_compatible: true,
            port: 3481,
        }
    }
}

/// What this server may send for sessions, and who may use its relays and
/// forwarding (`docs/protocol/egress-policy.md` §3.1).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EgressConfig {
    /// This server's egress cap (Mbit/s); none: unlimited.
    pub limit_mbps: Option<u32>,
    /// Accounts without their own setting.
    pub users: EgressPolicy,
    /// Sessions with no account on either side, together.
    pub guests: EgressPolicy,
}

/// One payer's policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EgressPolicy {
    /// May use the relays.
    pub relay: bool,
    /// May receive through forwarding rooms.
    pub forward: bool,
    /// Mbit/s over all its relayed and forwarded sessions; none: unlimited.
    pub rate_mbps: Option<u32>,
    /// GB per month; none: unlimited.
    pub quota_gb: Option<u32>,
}

impl Default for EgressPolicy {
    fn default() -> Self {
        Self {
            relay: true,
            forward: true,
            rate_mbps: None,
            quota_gb: None,
        }
    }
}

/// Ranges of the egress settings.
pub const EGRESS_LIMIT_MBPS: std::ops::RangeInclusive<u32> = 1..=100_000;
pub const EGRESS_RATE_MBPS: std::ops::RangeInclusive<u32> = 1..=10_000;
pub const EGRESS_QUOTA_GB: std::ops::RangeInclusive<u32> = 1..=1_000_000;

/// Viewers from which a host forwards.
pub const SFU_THRESHOLD: std::ops::RangeInclusive<u32> = 2..=16;

/// The web console (`docs/web-console.md`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    /// Serve the web console at `/` (the API is always available).
    pub enabled: bool,
    /// Networks (CIDRs) allowed to use the administration API; empty = any.
    pub admin_allow: Vec<IpNet>,
    /// A browser login expires after this many idle minutes ...
    pub session_idle_minutes: u32,
    /// ... and at the latest after this many hours.
    pub session_max_hours: u32,
    /// Days the audit log is kept.
    pub audit_days: u32,
    /// What browser controllers may do at most: a subset of `caps::WEB`
    /// with `view`; `["view", "audio"]` makes the web view-only.
    pub caps: Vec<String>,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            admin_allow: Vec::new(),
            // A week unused, a month at most: the token works only with this
            // browser's device key.
            session_idle_minutes: 7 * 24 * 60,
            session_max_hours: 30 * 24,
            audit_days: 180,
            caps: farwing_protocol::caps::WEB
                .iter()
                .map(|c| (*c).to_owned())
                .collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// A certificate generated by the server (clients pin its fingerprint).
    #[default]
    SelfSigned,
    /// A certificate and key uploaded by the administrator
    /// (`<data>/tls/provided-*.pem`).
    Provided,
    /// A certificate obtained and renewed automatically from an ACME CA.
    Acme,
    /// Plain HTTP behind a TLS-terminating reverse proxy.
    Proxy,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    pub mode: TlsMode,
    /// Extra names on the self-signed certificate (the host of
    /// `public_url` is always included).
    pub hostnames: Vec<String>,
    pub acme: AcmeConfig,
}

/// How the ACME CA verifies that this server controls the domain.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum AcmeChallenge {
    /// TLS-ALPN-01 when `public_url` uses port 443, else HTTP-01 when
    /// `http_listen` is set.
    #[default]
    #[serde(rename = "auto")]
    Auto,
    /// The CA connects to port 443 of the domain (served by `listen`).
    #[serde(rename = "tls-alpn-01")]
    TlsAlpn01,
    /// The CA fetches `http://<domain>/.well-known/acme-challenge/…` on port
    /// 80 (served by `http_listen`).
    #[serde(rename = "http-01")]
    Http01,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AcmeConfig {
    /// Names on the certificate; empty = the host of `public_url`.
    pub domains: Vec<String>,
    /// Contact e-mail for the CA (expiry and policy notices); optional.
    pub contact: String,
    /// `letsencrypt`, `letsencrypt-staging` or the directory URL of another
    /// ACME CA.
    pub directory: String,
    pub challenge: AcmeChallenge,
    /// Plain-HTTP listener (usually `0.0.0.0:80`): answers HTTP-01
    /// challenges and redirects everything else to `public_url`.
    pub http_listen: Option<SocketAddr>,
    /// Extra root certificate (PEM) trusted for the CA's API, for a private
    /// or test CA such as Pebble.
    pub ca_root: Option<PathBuf>,
    /// Agreement to the CA's terms of service (required).
    pub agree_tos: bool,
}

impl Default for AcmeConfig {
    fn default() -> Self {
        Self {
            domains: Vec::new(),
            contact: String::new(),
            directory: "letsencrypt".to_owned(),
            challenge: AcmeChallenge::Auto,
            http_listen: None,
            ca_root: None,
            agree_tos: false,
        }
    }
}

impl AcmeConfig {
    /// The directory URL of `directory`.
    pub fn directory_url(&self) -> String {
        match self.directory.trim() {
            "" | "letsencrypt" => "https://acme-v02.api.letsencrypt.org/directory".to_owned(),
            "letsencrypt-staging" => {
                "https://acme-staging-v02.api.letsencrypt.org/directory".to_owned()
            }
            url => url.to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GuestConfig {
    /// Devices without an account may host and connect (guests, visitors).
    pub enabled: bool,
}

impl Default for GuestConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnMode {
    /// Embedded STUN/TURN server.
    #[default]
    Builtin,
    /// coturn (or compatible) with `use-auth-secret`.
    External,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TurnConfig {
    pub mode: TurnMode,
    /// ICE server URLs handed to clients (`stun:`, `turn:`, `turns:`);
    /// external mode, or to override the builtin server's.
    pub urls: Vec<String>,
    /// TURN REST shared secret (coturn `static-auth-secret`); generated at
    /// setup, never shown.
    pub shared_secret: String,
    #[serde(with = "secs")]
    pub credential_ttl: Duration,
    pub force_relay: bool,
    // ---- builtin only
    /// Relay through this server; off, it offers STUN only (the NAT
    /// discovery endpoints).
    pub relay: bool,
    pub listen_udp: Option<SocketAddr>,
    pub listen_tcp: Option<SocketAddr>,
    /// TURN over TLS, with the web certificate (not behind a reverse proxy).
    pub listen_tls: Option<SocketAddr>,
    pub public_ip: Option<IpAddr>,
    pub realm: String,
    /// Local address relay sockets bind to.
    pub relay_bind: IpAddr,
    /// Address advertised in relayed candidates (defaults to `public_ip`).
    pub relay_ip: Option<IpAddr>,
    pub relay_port_min: Option<u16>,
    pub relay_port_max: Option<u16>,
    /// Extra STUN endpoints for NAT behaviour discovery.
    pub stun: Vec<StunEndpoint>,
}

/// An extra STUN Binding endpoint of the builtin server (RFC 5780).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StunEndpoint {
    /// Local UDP address to bind.
    pub listen: SocketAddr,
    /// Address clients see; defaults to `public_ip` and the listen port.
    #[serde(default)]
    pub public: Option<SocketAddr>,
}

impl TurnConfig {
    /// Public IP of the builtin server (`public_ip`, else `relay_ip`).
    pub fn builtin_public_ip(&self) -> Option<IpAddr> {
        self.public_ip.or(self.relay_ip)
    }

    /// Address clients reach `ep` at; `bound_port` stands in for a listen
    /// port of 0.
    pub fn stun_public(&self, ep: &StunEndpoint, bound_port: u16) -> Option<SocketAddr> {
        ep.public.or_else(|| {
            let port = match ep.listen.port() {
                0 => bound_port,
                p => p,
            };
            self.builtin_public_ip().map(|ip| SocketAddr::new(ip, port))
        })
    }
}

/// Durations stored as whole seconds.
mod secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;
    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_secs(u64::deserialize(d)?))
    }
}

/// Whether two UDP listen addresses cannot both be bound.
fn listen_conflict(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() != 0
        && a.port() == b.port()
        && a.is_ipv4() == b.is_ipv4()
        && (a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified())
}

/// Checks the extra STUN endpoints in builtin mode.
fn validate_stun(turn: &TurnConfig) -> Result<()> {
    let primary = turn.listen_udp.context("[turn] listen_udp missing")?;
    // Known public addresses (port 0 is only resolved when binding).
    let mut publics: Vec<(SocketAddr, String)> = Vec::new();
    if let Some(ip) = turn.builtin_public_ip() {
        if primary.port() != 0 {
            publics.push((SocketAddr::new(ip, primary.port()), "listen_udp".to_owned()));
        }
    }
    for (n, ep) in turn.stun.iter().enumerate() {
        let name = format!("[[turn.stun]] listen = \"{}\"", ep.listen);
        if let Some(p) = ep.public {
            if p.port() == 0 || p.ip().is_unspecified() || p.ip().is_multicast() {
                bail!("{name}: public = \"{p}\" is not a usable address");
            }
        }
        let public = turn
            .stun_public(ep, 0)
            .with_context(|| format!("{name} needs public (or [turn] public_ip)"))?;
        if public.is_ipv4() != ep.listen.is_ipv4() {
            bail!(
                "{name}: listen and public ({public}) must be the same address family; \
                 set public"
            );
        }
        if listen_conflict(ep.listen, primary) {
            bail!("{name} conflicts with listen_udp = \"{primary}\"");
        }
        if let Some(other) = turn.stun[..n]
            .iter()
            .find(|o| listen_conflict(ep.listen, o.listen))
        {
            bail!("{name} conflicts with [[turn.stun]] listen = \"{}\"", other.listen);
        }
        if public.port() != 0 {
            if let Some((_, other)) = publics.iter().find(|(a, _)| *a == public) {
                bail!("{name}: public address {public} is already used by {other}");
            }
            publics.push((public, name));
        }
    }
    // With several public IPs, answers (and CHANGE-REQUEST answers from
    // other sockets) must leave from the right local IP.
    for ep in &turn.stun {
        let Some(me) = turn.stun_public(ep, 0) else {
            continue;
        };
        let several_ips = turn
            .stun
            .iter()
            .filter_map(|o| turn.stun_public(o, 0))
            .any(|a| a.is_ipv4() == me.is_ipv4() && a.ip() != me.ip());
        if several_ips && ep.listen.ip().is_unspecified() {
            bail!(
                "[[turn.stun]] listen = \"{}\": endpoints on several public IPs must listen on a \
                 specific local IP, not {}",
                ep.listen,
                ep.listen.ip()
            );
        }
    }
    Ok(())
}

/// Warnings about `[[turn.stun]]`.
fn stun_warnings(turn: &TurnConfig, out: &mut Vec<String>) {
    if turn.stun.is_empty() {
        return;
    }
    if turn.mode != TurnMode::Builtin {
        out.push("[[turn.stun]] is only served with [turn] mode = \"builtin\"; ignored".to_owned());
        return;
    }
    let base = turn.builtin_public_ip();
    for ep in &turn.stun {
        let ip = ep.listen.ip();
        let local_only = match ip {
            IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
            IpAddr::V6(v6) => {
                v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local()
            }
        };
        if ep.public.is_none() && !ip.is_unspecified() && !local_only && Some(ip) != base {
            out.push(format!(
                "[[turn.stun]] listen = \"{}\" has no public and is announced as {}; set public \
                 if clients reach it at {ip}",
                ep.listen,
                turn.stun_public(ep, 0).map_or_else(String::new, |a| a.to_string()),
            ));
        }
    }
    let publics: Vec<SocketAddr> = turn
        .stun
        .iter()
        .filter_map(|ep| turn.stun_public(ep, 0))
        .filter(|a| a.port() != 0)
        .collect();
    let routes = crate::stun_server::plan(&publics);
    for (me, r) in publics.iter().zip(&routes) {
        let several_ips = publics
            .iter()
            .any(|a| a.is_ipv4() == me.is_ipv4() && a.ip() != me.ip());
        if several_ips && r.other.is_none() {
            out.push(format!(
                "[[turn.stun]] {me} sends no OTHER-ADDRESS: RFC 5780 tests need the same two \
                 ports on two public IPs (e.g. A:3479, A:3480, B:3479, B:3480)"
            ));
        }
    }
}

impl Default for TurnConfig {
    fn default() -> Self {
        let any = |port| Some(SocketAddr::from(([0, 0, 0, 0], port)));
        Self {
            mode: TurnMode::Builtin,
            urls: Vec::new(),
            shared_secret: String::new(),
            credential_ttl: Duration::from_secs(12 * 3600),
            force_relay: false,
            relay: true,
            listen_udp: any(3478),
            listen_tcp: any(3478),
            listen_tls: any(5349),
            public_ip: None,
            realm: "farwing".to_owned(),
            relay_bind: IpAddr::from([0, 0, 0, 0]),
            relay_ip: None,
            relay_port_min: Some(49160),
            relay_port_max: Some(49200),
            stun: [3479, 3480]
                .into_iter()
                .map(|port| StunEndpoint {
                    listen: SocketAddr::from(([0, 0, 0, 0], port)),
                    public: None,
                })
                .collect(),
        }
    }
}

const INSTANCE_NAME_MAX: usize = 64;

impl Config {
    /// The settings of a server that has not been set up yet: listen on
    /// `listen`, a self-signed certificate for this machine's names, the
    /// builtin relay waiting for its public address.
    pub fn initial(data_dir: PathBuf, listen: SocketAddr) -> Self {
        let local = local_ipv4();
        let host = local.map_or_else(|| "localhost".to_owned(), |ip| ip.to_string());
        let mut hostnames = vec!["localhost".to_owned(), "127.0.0.1".to_owned()];
        if let Some(ip) = local {
            hostnames.push(ip.to_string());
        }
        if let Some(name) = machine_name() {
            hostnames.push(name);
        }
        Self {
            data_dir,
            listen,
            public_url: format!("https://{host}:{}", listen.port()),
            tls: TlsConfig {
                hostnames,
                ..TlsConfig::default()
            },
            turn: TurnConfig {
                shared_secret: crate::crypto::new_shared_secret(),
                ..TurnConfig::default()
            },
            ..Self::default()
        }
    }

    /// Check the settings as a whole; the first problem found.
    pub fn validate(&self) -> Result<(), SettingError> {
        let url = url::Url::parse(&self.public_url)
            .map_err(|_| SettingError::invalid("public_url", "public_url is not a URL"))?;
        if url.scheme() != "https" {
            return Err(SettingError::invalid(
                "public_url",
                "public_url must use https:// (also behind a reverse proxy)",
            ));
        }
        if url.host_str().is_none_or(str::is_empty) {
            return Err(SettingError::invalid("public_url", "public_url has no host"));
        }
        let name = self.instance_name.trim();
        if name.is_empty() || name.chars().count() > INSTANCE_NAME_MAX {
            return Err(SettingError::range(
                "instance_name",
                format!("instance_name must be 1-{INSTANCE_NAME_MAX} characters"),
            ));
        }
        let egress = &self.egress;
        if egress.limit_mbps.is_some_and(|n| !EGRESS_LIMIT_MBPS.contains(&n)) {
            return Err(SettingError::range(
                "egress.limit_mbps",
                "egress.limit_mbps must be 1-100000",
            ));
        }
        for (who, policy) in [("users", &egress.users), ("guests", &egress.guests)] {
            if policy.rate_mbps.is_some_and(|n| !EGRESS_RATE_MBPS.contains(&n)) {
                return Err(SettingError::range(
                    &format!("egress.{who}.rate_mbps"),
                    format!("egress.{who}.rate_mbps must be 1-10000"),
                ));
            }
            if policy.quota_gb.is_some_and(|n| !EGRESS_QUOTA_GB.contains(&n)) {
                return Err(SettingError::range(
                    &format!("egress.{who}.quota_gb"),
                    format!("egress.{who}.quota_gb must be 1-1000000"),
                ));
            }
        }
        if tracing_subscriber::EnvFilter::try_new(&self.log).is_err() {
            return Err(SettingError::invalid("log", "log is not a valid filter"));
        }
        let caps = &self.web.caps;
        if !caps.iter().any(|c| c == farwing_protocol::caps::VIEW)
            || caps
                .iter()
                .any(|c| !farwing_protocol::caps::WEB.contains(&c.as_str()))
        {
            return Err(SettingError::invalid(
                "web.caps",
                "web.caps must include view and only browser capabilities",
            ));
        }
        let web = &self.web;
        for (field, value) in [
            ("web.session_idle_minutes", web.session_idle_minutes),
            ("web.session_max_hours", web.session_max_hours),
            ("web.audit_days", web.audit_days),
        ] {
            if value == 0 {
                return Err(SettingError::range(field, format!("{field} must be positive")));
            }
        }
        match self.tls.mode {
            TlsMode::SelfSigned | TlsMode::Provided | TlsMode::Proxy => {}
            TlsMode::Acme => {
                let acme = &self.tls.acme;
                if !acme.agree_tos {
                    return Err(SettingError::required(
                        "tls.acme.agree_tos",
                        format!(
                            "the terms of service of the CA ({}) must be accepted",
                            acme.directory_url()
                        ),
                    ));
                }
                let domains = self.acme_domains();
                if domains.is_empty() {
                    return Err(SettingError::required(
                        "tls.acme.domains",
                        "ACME needs a domain name (or one in public_url)",
                    ));
                }
                if let Some(d) = domains
                    .iter()
                    .find(|d| d.parse::<IpAddr>().is_ok() || !d.contains('.'))
                {
                    return Err(SettingError::invalid(
                        "tls.acme.domains",
                        format!("{d:?} is not a public domain name"),
                    ));
                }
                if url::Url::parse(&acme.directory_url()).is_err() {
                    return Err(SettingError::invalid(
                        "tls.acme.directory",
                        "the ACME directory is not a URL",
                    ));
                }
                self.acme_challenge().map_err(|e| {
                    SettingError::new("tls.acme.challenge", "invalid", format!("{e:#}"))
                })?;
            }
        }
        let turn = &self.turn;
        match turn.mode {
            TurnMode::External => {
                if turn.shared_secret.is_empty() {
                    return Err(SettingError::required(
                        "turn.shared_secret",
                        "the external relay needs its shared secret",
                    ));
                }
                if turn.urls.is_empty() {
                    return Err(SettingError::required(
                        "turn.urls",
                        "the external relay needs its URLs",
                    ));
                }
            }
            TurnMode::Builtin => {
                if turn.shared_secret.is_empty() {
                    return Err(SettingError::required(
                        "turn.shared_secret",
                        "the relay secret is missing",
                    ));
                }
                if turn.relay && turn.listen_udp.is_none() {
                    return Err(SettingError::required(
                        "turn.listen_udp",
                        "the relay needs a UDP port",
                    ));
                }
                if turn.builtin_public_ip().is_none() {
                    return Err(SettingError::required(
                        "turn.public_ip",
                        "the relay needs this server's public IP address",
                    ));
                }
                match (turn.relay_port_min, turn.relay_port_max) {
                    (Some(a), Some(b)) if a == 0 || b < a => {
                        return Err(SettingError::invalid(
                            "turn.relay_port_min",
                            "the relay port range is invalid",
                        ));
                    }
                    (Some(_), None) | (None, Some(_)) => {
                        return Err(SettingError::required(
                            "turn.relay_port_max",
                            "set both ends of the relay port range",
                        ));
                    }
                    _ => {}
                }
                // TCP ports must differ from each other and from the web port.
                let tcp_ports = [
                    ("turn.listen_tls", turn.listen_tls),
                    ("turn.listen_tcp", turn.listen_tcp),
                ];
                for (field, addr) in tcp_ports {
                    let Some(addr) = addr else { continue };
                    let others = std::iter::once(self.listen).chain(
                        tcp_ports
                            .iter()
                            .filter(|(other, _)| *other != field)
                            .filter_map(|(_, a)| *a),
                    );
                    for other in others {
                        if addr.port() != 0
                            && addr.port() == other.port()
                            && (addr.ip() == other.ip()
                                || addr.ip().is_unspecified()
                                || other.ip().is_unspecified())
                        {
                            return Err(SettingError::new(
                                field,
                                "conflict",
                                format!("{addr} is used by another listener"),
                            ));
                        }
                    }
                }
                validate_stun(turn)
                    .map_err(|e| SettingError::new("turn.stun", "invalid", format!("{e:#}")))?;
            }
        }
        let sfu = &self.sfu;
        if !SFU_THRESHOLD.contains(&sfu.threshold) {
            return Err(SettingError::range(
                "sfu.threshold",
                format!(
                    "sfu.threshold must be {}-{}",
                    SFU_THRESHOLD.start(),
                    SFU_THRESHOLD.end()
                ),
            ));
        }
        if sfu.port == 0 {
            return Err(SettingError::range("sfu.port", "sfu.port must be 1-65535"));
        }
        if turn.mode == TurnMode::Builtin {
            let udp = turn
                .listen_udp
                .into_iter()
                .chain(turn.stun.iter().map(|ep| ep.listen))
                .any(|a| a.port() == sfu.port);
            let relayed = matches!(
                (turn.relay_port_min, turn.relay_port_max),
                (Some(a), Some(b)) if (a..=b).contains(&sfu.port)
            );
            if udp || relayed {
                return Err(SettingError::new(
                    "sfu.port",
                    "conflict",
                    format!("UDP port {} is used by the relay", sfu.port),
                ));
            }
        }
        if turn.credential_ttl.is_zero() {
            return Err(SettingError::range(
                "turn.credential_ttl",
                "credential_ttl must be positive",
            ));
        }
        let l = &self.limits;
        if l.heartbeat_ms == 0 || l.idle_timeout_ms == 0 || l.ws_rate_per_s == 0 {
            return Err(SettingError::range(
                "limits",
                "timers and rates must be positive",
            ));
        }
        Ok(())
    }

    /// Warnings about values that are accepted but probably unintended.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        stun_warnings(&self.turn, &mut out);
        out
    }

    /// `wss://<host>[:port]<path>/api/v2/ws`.
    pub fn ws_endpoint(&self) -> String {
        let url = url::Url::parse(&self.public_url).expect("validated");
        let host = url.host_str().unwrap_or("localhost");
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_owned()
        };
        let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
        let prefix = url.path().trim_end_matches('/');
        format!("wss://{host}{port}{prefix}{}", farwing_protocol::WS_PATH)
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("farwing.db")
    }

    pub fn wallpaper_dir(&self) -> PathBuf {
        self.data_dir.join("wallpapers")
    }

    pub fn tls_dir(&self) -> PathBuf {
        self.data_dir.join("tls")
    }

    pub fn acme_dir(&self) -> PathBuf {
        self.data_dir.join("acme")
    }

    /// Names for the ACME certificate: `[tls.acme] domains`, else the host
    /// of `public_url`. Lower case, without duplicates.
    pub fn acme_domains(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tls
            .acme
            .domains
            .iter()
            .map(|d| d.trim().trim_end_matches('.').to_ascii_lowercase())
            .filter(|d| !d.is_empty())
            .collect();
        if names.is_empty()
            && let Some(host) = url::Url::parse(&self.public_url)
                .ok()
                .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        {
            names.push(host);
        }
        names.dedup();
        names
    }

    /// The challenge the ACME CA is asked to use (`auto` resolved).
    pub fn acme_challenge(&self) -> Result<AcmeChallenge> {
        let acme = &self.tls.acme;
        let public_port = url::Url::parse(&self.public_url)
            .ok()
            .and_then(|u| u.port_or_known_default())
            .unwrap_or(443);
        match acme.challenge {
            AcmeChallenge::Http01 if acme.http_listen.is_none() => {
                bail!("[tls.acme] challenge = \"http-01\" needs http_listen (usually \"0.0.0.0:80\")")
            }
            AcmeChallenge::Auto if public_port == 443 => Ok(AcmeChallenge::TlsAlpn01),
            AcmeChallenge::Auto if acme.http_listen.is_some() => Ok(AcmeChallenge::Http01),
            AcmeChallenge::Auto => bail!(
                "[tls.acme] cannot prove the domain: public_url is not on port 443 (needed for \
                 tls-alpn-01) and http_listen is not set (needed for http-01, usually \"0.0.0.0:80\")"
            ),
            other => Ok(other),
        }
    }

    /// Path of `public_url` without the trailing slash (empty at the root).
    pub fn public_path_prefix(&self) -> String {
        url::Url::parse(&self.public_url)
            .map(|u| u.path().trim_end_matches('/').to_owned())
            .unwrap_or_default()
    }
}


/// Optional overrides of protocol limits, timers and rate limits.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    pub heartbeat_ms: u32,
    pub idle_timeout_ms: u32,
    pub resume_ttl_s: u64,
    pub ticket_ttl_s: u64,
    pub max_frame_bytes: usize,
    pub max_sdp_bytes: usize,
    pub max_candidates: usize,
    pub max_candidate_bytes: usize,
    pub ws_rate_per_s: u32,
    pub ws_rate_burst: u32,
    /// New anonymous/bound devices per source IP and hour.
    pub device_init_per_ip_hour: u32,
    /// Failed logins per source IP per 15 minutes.
    pub login_failures_per_ip: u32,
    /// Failed logins per username per 15 minutes.
    pub login_failures_per_user: u32,
    /// Assistance attempts per (account, connect_id) and minute that do not
    /// end in a session.
    pub assist_failures_per_minute: u32,
    /// Wait for a host to answer the PAKE of `assist/join`.
    pub assist_host_reply_ms: u64,
    /// Long poll of `assist/requests/{id}/accept` (key check and confirmation).
    pub assist_decision_wait_ms: u64,
    pub guest_token_ttl_s: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        use farwing_protocol::{limits, timing};
        Self {
            heartbeat_ms: timing::HEARTBEAT_MS,
            idle_timeout_ms: timing::IDLE_TIMEOUT_MS,
            resume_ttl_s: limits::RESUME_TTL_SECONDS,
            ticket_ttl_s: limits::TICKET_TTL_SECONDS,
            max_frame_bytes: limits::MAX_WS_FRAME,
            max_sdp_bytes: limits::MAX_SDP_BYTES,
            max_candidates: limits::MAX_CANDIDATES,
            max_candidate_bytes: limits::MAX_CANDIDATE_BYTES,
            ws_rate_per_s: limits::WS_RATE_PER_SECOND,
            ws_rate_burst: limits::WS_RATE_BURST,
            device_init_per_ip_hour: 30,
            login_failures_per_ip: 20,
            login_failures_per_user: 10,
            assist_failures_per_minute: 10,
            assist_host_reply_ms: limits::ASSIST_HOST_REPLY_WAIT_MS,
            assist_decision_wait_ms: limits::ASSIST_DECISION_WAIT_MS,
            guest_token_ttl_s: 24 * 3600,
        }
    }
}

/// Map `windows`/`win` etc. to the canonical platform name.
pub fn normalize_platform(p: &str) -> Option<&'static str> {
    farwing_protocol::Platform::parse(p).map(farwing_protocol::Platform::as_str)
}

/// The IPv4 address of the default route (no packet is sent).
pub fn local_ipv4() -> Option<IpAddr> {
    // A proxy's virtual adapter may hold the default route with an address
    // from 198.18.0.0/15 (reserved for benchmarks): the LAN adapter is then
    // found through the route to a private network.
    ["192.0.2.1", "192.168.255.254", "10.255.255.254", "172.31.255.254"]
        .into_iter()
        .filter_map(|target| {
            let socket = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
            socket.connect((target, 9)).ok()?;
            socket.local_addr().ok().map(|a| a.ip())
        })
        .find(|ip| match ip {
            IpAddr::V4(v4) => {
                !v4.is_unspecified() && !v4.is_loopback() && (v4.octets()[0], v4.octets()[1] & 0xfe) != (198, 18)
            }
            IpAddr::V6(_) => false,
        })
}

/// This machine's host name, if the environment says.
fn machine_name() -> Option<String> {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .map(|n| n.trim().to_ascii_lowercase())
        .find(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

pub fn parse_listen(text: &str) -> Result<SocketAddr> {
    text.parse()
        .map_err(|_| anyhow!("invalid socket address {text:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin() -> Config {
        Config {
            public_url: "https://farwing.example.com:8443".into(),
            turn: TurnConfig {
                shared_secret: "s".into(),
                public_ip: Some("203.0.113.10".parse().unwrap()),
                stun: Vec::new(),
                ..TurnConfig::default()
            },
            ..Config::default()
        }
    }

    fn with_stun(endpoints: &[(&str, Option<&str>)]) -> Config {
        let mut cfg = builtin();
        cfg.turn.stun = endpoints
            .iter()
            .map(|(listen, public)| StunEndpoint {
                listen: sa(listen),
                public: public.map(sa),
            })
            .collect();
        cfg
    }

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn defaults_and_round_trip() {
        let cfg = Config::initial(PathBuf::from("d"), sa("0.0.0.0:8443"));
        assert_eq!(cfg.turn.mode, TurnMode::Builtin);
        assert!(!cfg.turn.shared_secret.is_empty());
        // Not usable until the public address is known.
        assert_eq!(cfg.validate().unwrap_err().field, "turn.public_ip");
        let mut cfg = cfg;
        cfg.turn.public_ip = Some("203.0.113.10".parse().unwrap());
        cfg.validate().unwrap();
        assert!(cfg.warnings().is_empty(), "{:?}", cfg.warnings());
        // Stored as JSON without the data directory.
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("data_dir"));
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.turn, cfg.turn);
        assert_eq!(back.turn.credential_ttl, Duration::from_secs(43200));
        assert!(serde_json::from_str::<Config>(r#"{"unknown": 1}"#).is_err());
    }

    #[test]
    fn field_errors() {
        let field = |f: &dyn Fn(&mut Config)| {
            let mut cfg = builtin();
            f(&mut cfg);
            cfg.validate().map_err(|e| (e.field, e.problem))
        };
        assert_eq!(field(&|_| {}), Ok(()));
        assert_eq!(
            field(&|c| c.public_url = "http://x".into()),
            Err(("public_url".into(), "invalid"))
        );
        assert_eq!(
            field(&|c| c.instance_name = " ".into()),
            Err(("instance_name".into(), "out_of_range"))
        );
        assert_eq!(
            field(&|c| c.egress.limit_mbps = Some(0)),
            Err(("egress.limit_mbps".into(), "out_of_range"))
        );
        assert_eq!(
            field(&|c| c.web.caps = vec!["audio".into()]),
            Err(("web.caps".into(), "invalid"))
        );
        assert_eq!(
            field(&|c| c.turn.listen_tcp = Some(sa("0.0.0.0:8443"))),
            Err(("turn.listen_tcp".into(), "conflict"))
        );
        assert_eq!(
            field(&|c| c.tls.mode = TlsMode::Acme),
            Err(("tls.acme.agree_tos".into(), "required"))
        );
        assert_eq!(
            field(&|c| c.log = "[".into()),
            Err(("log".into(), "invalid"))
        );
    }

    #[test]
    fn platforms_and_prefix() {
        assert_eq!(normalize_platform("Win"), Some("windows"));
        assert_eq!(normalize_platform("linux"), Some("linux"));
        assert_eq!(normalize_platform("tv"), None);
        let mut cfg = builtin();
        assert_eq!(cfg.public_path_prefix(), "");
        assert_eq!(cfg.ws_endpoint(), "wss://farwing.example.com:8443/api/v2/ws");
        cfg.public_url = "https://example.com/farwing/".into();
        assert_eq!(cfg.public_path_prefix(), "/farwing");
    }

    #[test]
    fn stun_endpoints() {
        let cfg = with_stun(&[("0.0.0.0:3479", None), ("0.0.0.0:3480", Some("203.0.113.10:4480"))]);
        cfg.validate().unwrap();
        let t = &cfg.turn;
        assert_eq!(t.stun_public(&t.stun[0], 0), Some(sa("203.0.113.10:3479")));
        assert_eq!(t.stun_public(&t.stun[1], 0), Some(sa("203.0.113.10:4480")));
        assert!(cfg.warnings().is_empty(), "{:?}", cfg.warnings());
        // Listen port 0 takes the bound port.
        let ep = StunEndpoint {
            listen: sa("0.0.0.0:0"),
            public: None,
        };
        assert_eq!(t.stun_public(&ep, 5000), Some(sa("203.0.113.10:5000")));

        // A complete set on two public IPs behind 1:1 NAT.
        let square = [
            ("10.0.0.5:3479", None),
            ("10.0.0.5:3480", None),
            ("10.0.0.6:3479", Some("198.51.100.20:3479")),
            ("10.0.0.6:3480", Some("198.51.100.20:3480")),
        ];
        let cfg = with_stun(&square);
        cfg.validate().unwrap();
        assert!(cfg.warnings().is_empty(), "{:?}", cfg.warnings());
        // Without the last corner the others cannot send OTHER-ADDRESS.
        let w = with_stun(&square[..3]).warnings();
        assert_eq!(w.len(), 3, "{w:?}");
        assert!(w[0].contains("no OTHER-ADDRESS"), "{w:?}");
        // A global listen IP without public is probably a second public IP.
        let w = with_stun(&[("198.51.100.20:3479", None)]).warnings();
        assert!(w.len() == 1 && w[0].contains("set public"), "{w:?}");
        // Ignored (with a warning) in other modes.
        let mut cfg = with_stun(&[("0.0.0.0:3479", None)]);
        cfg.turn.mode = TurnMode::External;
        cfg.turn.urls = vec!["turn:t.example:3478".into()];
        assert!(cfg.warnings().iter().any(|w| w.contains("only served")));
    }

    #[test]
    fn invalid_stun_endpoints() {
        let err = |eps: &[(&str, Option<&str>)]| match with_stun(eps).validate() {
            Ok(()) => panic!("accepted: {eps:?}"),
            Err(e) => {
                assert_eq!(e.field, "turn.stun");
                e.message
            }
        };
        let cases: [(&[(&str, Option<&str>)], &str); 10] = [
            (&[("0.0.0.0:3478", None)], "conflicts with listen_udp"),
            (&[("10.0.0.5:3478", None)], "conflicts with listen_udp"),
            (
                &[("10.0.0.5:3479", None), ("0.0.0.0:3479", Some("198.51.100.20:3479"))],
                "conflicts with [[turn.stun]]",
            ),
            (&[("0.0.0.0:3479", Some("203.0.113.10:0"))], "not a usable address"),
            (&[("0.0.0.0:3479", Some("0.0.0.0:3479"))], "not a usable address"),
            (&[("[::]:3479", None)], "same address family"),
            (&[("0.0.0.0:3479", Some("[2001:db8::1]:3479"))], "same address family"),
            (&[("0.0.0.0:3479", Some("203.0.113.10:3478"))], "already used by listen_udp"),
            (
                &[("10.0.0.5:3479", None), ("10.0.0.6:3479", None)],
                "already used by [[turn.stun]] listen = \"10.0.0.5:3479\"",
            ),
            (
                &[("0.0.0.0:3479", None), ("10.0.0.6:3480", Some("198.51.100.20:3480"))],
                "must listen on a specific local IP",
            ),
        ];
        for (eps, expected) in cases {
            let e = err(eps);
            assert!(e.contains(expected), "{eps:?}\n=> {e}");
        }
        // Distinct families and ports, and port 0 (bound at start), are fine.
        with_stun(&[
            ("0.0.0.0:3479", None),
            ("[::]:3479", Some("[2001:db8::1]:3479")),
            ("127.0.0.1:0", None),
            ("127.0.0.1:0", None),
        ])
        .validate()
        .unwrap();
    }
}
