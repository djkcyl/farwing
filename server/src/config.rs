//! Server configuration (TOML).

use anyhow::{Context, Result, anyhow, bail};
use ipnet::IpNet;
use serde::{Deserialize, Deserializer};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Socket address of the HTTPS/WSS listener.
    pub listen: SocketAddr,
    /// Public `https://` base URL used to build realtime endpoints.
    pub public_url: String,
    /// Directory for the SQLite database, wallpapers and generated certificates.
    pub data_dir: PathBuf,
    /// Name shown in `GET /api/v2/server`.
    #[serde(default = "default_instance_name")]
    pub instance_name: String,
    /// Peers (CIDRs) whose `X-Forwarded-For` header is honored.
    #[serde(default)]
    pub trusted_proxies: Vec<IpNet>,
    /// Session bitrate cap handed to clients.
    #[serde(default)]
    pub bitrate_limit_mbps: Option<u32>,
    /// `tracing` filter directive, e.g. `info` or `farwing_server=debug`.
    /// `RUST_LOG` overrides it.
    #[serde(default)]
    pub log: Option<String>,
    #[serde(default)]
    pub tls: TlsConfig,
    #[serde(default)]
    pub guest: GuestConfig,
    #[serde(default)]
    pub turn: TurnConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub web: WebConfig,
}

/// The web console (`docs/web-console.md`).
#[derive(Clone, Debug, Deserialize)]
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
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            admin_allow: Vec::new(),
            session_idle_minutes: 30,
            session_max_hours: 12,
            audit_days: 180,
        }
    }
}

fn default_instance_name() -> String {
    "farwing".to_owned()
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// Terminate TLS in the server (certificate files or self-signed).
    #[default]
    Tls,
    /// Plain HTTP behind a TLS-terminating reverse proxy.
    Proxy,
    /// Terminate TLS here with a certificate obtained and renewed
    /// automatically from an ACME CA (`[tls.acme]`).
    Acme,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    pub mode: TlsMode,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub self_signed: bool,
    pub hostnames: Vec<String>,
    pub acme: AcmeConfig,
}

/// How the ACME CA verifies that this server controls the domain.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
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

#[derive(Clone, Debug, Deserialize)]
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

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GuestConfig {
    pub enabled: bool,
}

impl Default for GuestConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnMode {
    /// coturn (or compatible) with `use-auth-secret`.
    External,
    /// Embedded STUN/TURN server.
    Builtin,
    /// STUN only, no relay credentials.
    #[default]
    None,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TurnConfig {
    pub mode: TurnMode,
    /// ICE server URLs handed to clients (`stun:`, `turn:`, `turns:`).
    pub urls: Vec<String>,
    /// TURN REST shared secret (coturn `static-auth-secret`).
    pub shared_secret: String,
    #[serde(deserialize_with = "de_duration")]
    pub credential_ttl: Duration,
    pub force_relay: bool,
    // ---- builtin only
    pub listen_udp: Option<SocketAddr>,
    pub listen_tcp: Option<SocketAddr>,
    pub public_ip: Option<IpAddr>,
    pub realm: String,
    /// Local address relay sockets bind to.
    pub relay_bind: IpAddr,
    /// Address advertised in relayed candidates (defaults to `public_ip`).
    pub relay_ip: Option<IpAddr>,
    pub relay_port_min: Option<u16>,
    pub relay_port_max: Option<u16>,
    /// Extra STUN endpoints for NAT behaviour discovery (`[[turn.stun]]`).
    pub stun: Vec<StunEndpoint>,
}

/// An extra STUN Binding endpoint of the builtin server (RFC 5780).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
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

/// Whether two UDP listen addresses cannot both be bound.
fn listen_conflict(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() != 0
        && a.port() == b.port()
        && a.is_ipv4() == b.is_ipv4()
        && (a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified())
}

/// Checks `[[turn.stun]]` in builtin mode.
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
        Self {
            mode: TurnMode::None,
            urls: Vec::new(),
            shared_secret: String::new(),
            credential_ttl: Duration::from_secs(12 * 3600),
            force_relay: false,
            listen_udp: None,
            listen_tcp: None,
            public_ip: None,
            realm: "farwing".to_owned(),
            relay_bind: IpAddr::from([0, 0, 0, 0]),
            relay_ip: None,
            relay_port_min: None,
            relay_port_max: None,
            stun: Vec::new(),
        }
    }
}

/// Optional overrides of protocol limits, timers and rate limits.
#[derive(Clone, Debug, Deserialize)]
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

/// Accepts integer seconds or a string such as `"12h"`, `"30m"`, `"90s"`, `"1d"`.
fn de_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Secs(u64),
        Text(String),
    }
    match Raw::deserialize(d)? {
        Raw::Secs(s) => Ok(Duration::from_secs(s)),
        Raw::Text(t) => parse_duration(&t).map_err(serde::de::Error::custom),
    }
}

pub fn parse_duration(text: &str) -> Result<Duration> {
    let t = text.trim();
    if t.is_empty() {
        bail!("empty duration");
    }
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: u64 = num
        .parse()
        .with_context(|| format!("invalid duration {t:?}"))?;
    let mult = match unit.trim() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        other => bail!("invalid duration unit {other:?} (use s, m, h or d)"),
    };
    Ok(Duration::from_secs(n * mult))
}

impl Config {
    /// Load and validate a config file. Relative paths inside the file are
    /// resolved against the file's directory.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        let base = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        cfg.resolve_paths(&base);
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn from_toml_str(text: &str, base: &Path) -> Result<Self> {
        let mut cfg: Config = toml::from_str(text)?;
        cfg.resolve_paths(base);
        cfg.validate()?;
        Ok(cfg)
    }

    fn resolve_paths(&mut self, base: &Path) {
        let fix = |p: &mut PathBuf| {
            if p.is_relative() {
                *p = base.join(&*p);
            }
        };
        fix(&mut self.data_dir);
        if let Some(p) = self.tls.cert.as_mut() {
            fix(p);
        }
        if let Some(p) = self.tls.key.as_mut() {
            fix(p);
        }
        if let Some(p) = self.tls.acme.ca_root.as_mut() {
            fix(p);
        }
    }

    pub fn validate(&self) -> Result<()> {
        let url = url::Url::parse(&self.public_url)
            .with_context(|| format!("public_url {:?} is not a URL", self.public_url))?;
        if url.scheme() != "https" {
            bail!("public_url must use https:// (also in tls.mode = \"proxy\")");
        }
        if url.host_str().is_none() {
            bail!("public_url has no host");
        }
        match self.tls.mode {
            TlsMode::Proxy => {}
            TlsMode::Tls => {
                if !self.tls.self_signed && (self.tls.cert.is_none() || self.tls.key.is_none()) {
                    bail!("[tls] needs cert and key, or self_signed = true, or mode = \"proxy\"");
                }
            }
            TlsMode::Acme => {
                let acme = &self.tls.acme;
                if !acme.agree_tos {
                    bail!(
                        "[tls.acme] agree_tos = true is required: it confirms that you accept the \
                         terms of service of the CA ({})",
                        acme.directory_url()
                    );
                }
                let domains = self.acme_domains();
                if domains.is_empty() {
                    bail!("[tls.acme] needs domains (or a host name in public_url)");
                }
                for d in &domains {
                    if d.parse::<IpAddr>().is_ok() || !d.contains('.') {
                        bail!("[tls.acme] {d:?} is not a public domain name");
                    }
                }
                url::Url::parse(&acme.directory_url())
                    .with_context(|| format!("[tls.acme] directory {:?}", acme.directory))?;
                self.acme_challenge()?;
            }
        }
        let turn = &self.turn;
        match turn.mode {
            TurnMode::External => {
                if turn.shared_secret.is_empty() {
                    bail!("[turn] mode = \"external\" needs shared_secret");
                }
                if turn.urls.is_empty() {
                    bail!("[turn] mode = \"external\" needs urls");
                }
            }
            TurnMode::Builtin => {
                if turn.shared_secret.is_empty() {
                    bail!("[turn] mode = \"builtin\" needs shared_secret");
                }
                if turn.listen_udp.is_none() {
                    bail!("[turn] mode = \"builtin\" needs listen_udp");
                }
                if turn.public_ip.is_none() && turn.relay_ip.is_none() {
                    bail!("[turn] mode = \"builtin\" needs public_ip");
                }
                match (turn.relay_port_min, turn.relay_port_max) {
                    (Some(a), Some(b)) if a == 0 || b < a => {
                        bail!("[turn] relay_port_min/relay_port_max are invalid")
                    }
                    (Some(_), None) | (None, Some(_)) => {
                        bail!("[turn] set both relay_port_min and relay_port_max")
                    }
                    _ => {}
                }
                validate_stun(turn)?;
            }
            TurnMode::None => {
                if turn
                    .urls
                    .iter()
                    .any(|u| u.starts_with("turn:") || u.starts_with("turns:"))
                {
                    bail!("[turn] mode = \"none\" only allows stun: urls");
                }
            }
        }
        if turn.credential_ttl.is_zero() {
            bail!("[turn] credential_ttl must be positive");
        }
        let l = &self.limits;
        if l.heartbeat_ms == 0 || l.idle_timeout_ms == 0 || l.ws_rate_per_s == 0 {
            bail!("[limits] timers and rates must be positive");
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

/// Map `windows`/`win` etc. to the canonical platform name.
pub fn normalize_platform(p: &str) -> Option<&'static str> {
    farwing_protocol::Platform::parse(p).map(farwing_protocol::Platform::as_str)
}

/// The commented configuration written by `farwing-server init`.
pub const CONFIG_TEMPLATE: &str = r#"# Farwing Server configuration.
# Relative paths are resolved against the directory of this file.

# Address of the HTTPS + WSS listener.
listen = "0.0.0.0:8443"

# Public https:// URL clients use to reach this server. The realtime endpoint
# is derived from it: wss://<host>[:port]/api/v2/ws. It must be https even when
# a reverse proxy terminates TLS (tls.mode = "proxy").
public_url = "{public_url}"

# Data directory: SQLite database (farwing.db), wallpapers/ and tls/.
data_dir = "{data_dir}"

# Name shown to clients in GET /api/v2/server.
instance_name = "farwing"

# Reverse proxies (CIDRs) whose X-Forwarded-For header is trusted for the
# client IP (rate limits, Wake-on-LAN network matching).
trusted_proxies = []

# Optional session bitrate cap (Mbit/s) handed to clients.
# bitrate_limit_mbps = 50

# Log filter (RUST_LOG overrides it). Secrets, SDP and ICE candidates are never logged.
log = "info"

[tls]
# mode = "tls"   : terminate TLS here (default)
# mode = "proxy" : plain HTTP on `listen`, behind a TLS-terminating reverse proxy
# mode = "acme"  : terminate TLS here with a certificate obtained and renewed
#                  automatically (Let's Encrypt or another ACME CA, see [tls.acme])
mode = "tls"
# Either a certificate chain and private key in PEM format ...
# cert = "/etc/farwing/fullchain.pem"
# key = "/etc/farwing/privkey.pem"
# ... or a self-signed certificate generated on first start into data_dir/tls.
# Its SHA-256 fingerprint is logged at startup so clients can pin it.
self_signed = true
hostnames = ["{host}"]

# [tls.acme]  (mode = "acme"; browsers and clients then trust the certificate
# without pinning). The CA must reach this server under the domain name:
# tls-alpn-01 needs port 443 (public_url on 443), http-01 needs port 80.
# Names on the certificate (default: the host of public_url):
# domains = ["{host}"]
# contact = "admin@example.com" # optional, for expiry notices
# directory = "letsencrypt"     # or "letsencrypt-staging", or a directory URL
# challenge = "auto"            # "tls-alpn-01", "http-01" or "auto"
# http_listen = "0.0.0.0:80"    # http-01 and redirect from http:// to public_url
# agree_tos = true              # required: accept the CA's terms of service

[guest]
# Allow devices without an account (guest hosts and visitors). Can also be
# changed in the web console (Settings).
enabled = true

[web]
# Serve the web console at / (user center, administration, browser viewing).
enabled = true
# Networks allowed to use the administration pages, e.g. ["192.168.0.0/16"];
# empty = any network (administrators can enable TOTP).
admin_allow = []
# Browser logins expire after this many idle minutes, and after this many hours.
session_idle_minutes = 30
session_max_hours = 12
# Days the audit log is kept.
audit_days = 180

[turn]
# mode = "external": coturn (or compatible) with use-auth-secret / static-auth-secret
# mode = "builtin" : embedded STUN/TURN server (listen_udp, public_ip, ...)
# mode = "none"    : STUN only, no relay
mode = "external"
urls = [
  "stun:{host}:3478",
  "turn:{host}:3478?transport=udp",
  "turn:{host}:3478?transport=tcp",
]
# Shared secret for TURN REST credentials (coturn static-auth-secret).
shared_secret = "{turn_secret}"
# Lifetime of issued TURN credentials ("12h", "30m", or seconds).
credential_ttl = "12h"
# Ask clients to use relayed candidates only.
force_relay = false
# ---- builtin only:
# listen_udp = "0.0.0.0:3478"
# listen_tcp = "0.0.0.0:3478"
# public_ip = "203.0.113.10"
# realm = "farwing"
# relay_bind = "0.0.0.0"
# relay_port_min = 49160
# relay_port_max = 49200
# With builtin mode and empty `urls`, stun:/turn: URLs are derived from
# public_ip and the listen ports.
# Extra STUN endpoints for NAT behaviour discovery (RFC 5780, builtin only).
# Each answers Binding requests with RESPONSE-ORIGIN and honors CHANGE-REQUEST;
# clients get them as additional stun: URLs. Two ports on one IP allow the
# "change port" tests; the same two ports on a second public IP enable
# OTHER-ADDRESS and all tests. Keep these entries at the end of [turn].
# public defaults to public_ip and the listen port.
# [[turn.stun]]
# listen = "203.0.113.10:3479"
# [[turn.stun]]
# listen = "203.0.113.10:3480"
# [[turn.stun]]
# listen = "198.51.100.20:3479"
# public = "198.51.100.20:3479"
# [[turn.stun]]
# listen = "198.51.100.20:3480"
# public = "198.51.100.20:3480"

[limits]
# Optional overrides (defaults shown).
# heartbeat_ms = 20000
# idle_timeout_ms = 60000
# resume_ttl_s = 300
# ticket_ttl_s = 60
# max_frame_bytes = 1048576
# max_sdp_bytes = 1048576
# max_candidates = 256
# max_candidate_bytes = 2048
# ws_rate_per_s = 200
# ws_rate_burst = 400
# device_init_per_ip_hour = 30
# login_failures_per_ip = 20
# login_failures_per_user = 10
# assist_failures_per_minute = 10
# assist_host_reply_ms = 10000
# assist_decision_wait_ms = 65000
# guest_token_ttl_s = 86400
"#;

/// Render [`CONFIG_TEMPLATE`]. The TLS hostname and TURN host are derived
/// from `public_url`.
pub fn render_template(public_url: &str, data_dir: &str, turn_secret: &str) -> String {
    let host = url::Url::parse(public_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_matches(['[', ']']).to_owned()))
        .unwrap_or_else(|| "localhost".to_owned());
    CONFIG_TEMPLATE
        .replace("{host}", &toml_escape(&host))
        .replace("{public_url}", &toml_escape(public_url))
        .replace("{data_dir}", &toml_escape(data_dir))
        .replace("{turn_secret}", &toml_escape(turn_secret))
}

fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Values used for the checked-in `farwing-server.example.toml`.
pub const EXAMPLE_PUBLIC_URL: &str = "https://farwing.example.com:8443";
pub const EXAMPLE_DATA_DIR: &str = "./data";
pub const EXAMPLE_TURN_SECRET: &str = "CHANGE-ME-run-farwing-server-init-to-generate-a-secret";

pub fn parse_listen(text: &str) -> Result<SocketAddr> {
    text.parse()
        .map_err(|_| anyhow!("invalid socket address {text:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_file_matches_template_and_parses() {
        let rendered = render_template(EXAMPLE_PUBLIC_URL, EXAMPLE_DATA_DIR, EXAMPLE_TURN_SECRET);
        let file = include_str!("../farwing-server.example.toml");
        assert_eq!(
            rendered.replace("\r\n", "\n"),
            file.replace("\r\n", "\n"),
            "farwing-server.example.toml is out of date with CONFIG_TEMPLATE"
        );
        let cfg = Config::from_toml_str(&rendered, Path::new(".")).unwrap();
        assert!(cfg.warnings().is_empty());
        assert_eq!(cfg.turn.credential_ttl, Duration::from_secs(43200));
        assert_eq!(
            cfg.ws_endpoint(),
            "wss://farwing.example.com:8443/api/v2/ws"
        );
    }

    #[test]
    fn platforms_and_prefix() {
        assert_eq!(normalize_platform("Win"), Some("windows"));
        assert_eq!(normalize_platform("linux"), Some("linux"));
        assert_eq!(normalize_platform("tv"), None);
        let mut cfg = Config::from_toml_str(
            &render_template(EXAMPLE_PUBLIC_URL, EXAMPLE_DATA_DIR, EXAMPLE_TURN_SECRET),
            Path::new("."),
        )
        .unwrap();
        assert_eq!(cfg.public_path_prefix(), "");
        cfg.public_url = "https://example.com/farwing/".into();
        assert_eq!(cfg.public_path_prefix(), "/farwing");
    }

    /// A builtin-mode configuration with `extra` appended to `[turn]`.
    fn builtin_with(extra: &str) -> Result<Config> {
        let text = format!(
            r#"
listen = "0.0.0.0:8443"
public_url = "https://farwing.example.com:8443"
data_dir = "./data"
[tls]
self_signed = true
[turn]
mode = "builtin"
shared_secret = "s"
listen_udp = "0.0.0.0:3478"
public_ip = "203.0.113.10"
{extra}
"#
        );
        Config::from_toml_str(&text, Path::new("."))
    }

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn stun_endpoints() {
        let cfg = builtin_with(
            r#"
[[turn.stun]]
listen = "0.0.0.0:3479"
[[turn.stun]]
listen = "0.0.0.0:3480"
public = "203.0.113.10:4480"
"#,
        )
        .unwrap();
        let t = &cfg.turn;
        assert_eq!(t.stun.len(), 2);
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
        let square = r#"
[[turn.stun]]
listen = "10.0.0.5:3479"
[[turn.stun]]
listen = "10.0.0.5:3480"
[[turn.stun]]
listen = "10.0.0.6:3479"
public = "198.51.100.20:3479"
[[turn.stun]]
listen = "10.0.0.6:3480"
public = "198.51.100.20:3480"
"#;
        let cfg = builtin_with(square).unwrap();
        assert!(cfg.warnings().is_empty(), "{:?}", cfg.warnings());
        // Without the last corner the others cannot send OTHER-ADDRESS.
        let three = &square[..square.rfind("[[turn.stun]]").unwrap()];
        let w = builtin_with(three).unwrap().warnings();
        assert_eq!(w.len(), 3, "{w:?}");
        assert!(w[0].contains("no OTHER-ADDRESS"), "{w:?}");
        // A global listen IP without public is probably a second public IP.
        let w = builtin_with("[[turn.stun]]\nlisten = \"198.51.100.20:3479\"")
            .unwrap()
            .warnings();
        assert!(w.len() == 1 && w[0].contains("set public"), "{w:?}");
        // Ignored (with a warning) in other modes.
        let mut cfg = Config::from_toml_str(
            &render_template(EXAMPLE_PUBLIC_URL, EXAMPLE_DATA_DIR, EXAMPLE_TURN_SECRET),
            Path::new("."),
        )
        .unwrap();
        cfg.turn.stun.push(ep);
        assert!(cfg.warnings().iter().any(|w| w.contains("only served")));
    }

    #[test]
    fn invalid_stun_endpoints() {
        let err = |extra: &str| match builtin_with(extra) {
            Ok(_) => panic!("accepted: {extra}"),
            Err(e) => format!("{e:#}"),
        };
        let one = |listen: &str, public: &str| {
            let public = if public.is_empty() {
                String::new()
            } else {
                format!("public = \"{public}\"")
            };
            format!("[[turn.stun]]\nlisten = \"{listen}\"\n{public}\n")
        };
        let cases = [
            (one("0.0.0.0:3478", ""), "conflicts with listen_udp"),
            (one("10.0.0.5:3478", ""), "conflicts with listen_udp"),
            (
                one("10.0.0.5:3479", "") + &one("0.0.0.0:3479", "198.51.100.20:3479"),
                "conflicts with [[turn.stun]]",
            ),
            (one("0.0.0.0:3479", "203.0.113.10:0"), "not a usable address"),
            (one("0.0.0.0:3479", "0.0.0.0:3479"), "not a usable address"),
            (one("[::]:3479", ""), "same address family"),
            (one("0.0.0.0:3479", "[2001:db8::1]:3479"), "same address family"),
            (one("0.0.0.0:3479", "203.0.113.10:3478"), "already used by listen_udp"),
            (
                one("10.0.0.5:3479", "") + &one("10.0.0.6:3479", ""),
                "already used by [[turn.stun]] listen = \"10.0.0.5:3479\"",
            ),
            (
                one("0.0.0.0:3479", "") + &one("10.0.0.6:3480", "198.51.100.20:3480"),
                "must listen on a specific local IP",
            ),
            ("[[turn.stun]]\nlisten = \"0.0.0.0:3479\"\nport = 1\n".to_owned(), "unknown field"),
        ];
        for (extra, expected) in cases {
            let e = err(&extra);
            assert!(e.contains(expected), "{extra}\n=> {e}");
        }
        // Distinct families and ports, and port 0 (bound at start), are fine.
        let ok = one("0.0.0.0:3479", "")
            + &one("[::]:3479", "[2001:db8::1]:3479")
            + &one("127.0.0.1:0", "")
            + &one("127.0.0.1:0", "");
        builtin_with(&ok).unwrap();
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("12h").unwrap(), Duration::from_secs(43200));
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert!(parse_duration("1y").is_err());
    }
}
