//! ICE server lists and TURN REST credentials handed to session peers.

use crate::config::{TurnConfig, TurnMode};
use farwing_protocol::realtime::IceServer;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct IceProvider {
    stun_urls: Vec<String>,
    turn_urls: Vec<String>,
    secret: String,
    ttl: Duration,
}

fn host_text(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    }
}

/// URLs advertised for the builtin server when none are configured: its
/// listeners in `cfg` (the ports actually bound), TURN over TLS under
/// `tls_host` (a name on the certificate) or else the public IP.
pub fn builtin_urls(cfg: &TurnConfig, tls_host: Option<&str>) -> Vec<String> {
    let Some(ip) = cfg.public_ip.or(cfg.relay_ip) else {
        return Vec::new();
    };
    let host = host_text(ip);
    let mut out = Vec::new();
    if let Some(udp) = cfg.listen_udp {
        out.push(format!("stun:{host}:{}", udp.port()));
        out.push(format!("turn:{host}:{}?transport=udp", udp.port()));
    }
    if let Some(tcp) = cfg.listen_tcp {
        out.push(format!("turn:{host}:{}?transport=tcp", tcp.port()));
    }
    if let Some(tls) = cfg.listen_tls {
        let host = tls_host.map_or(host.clone(), str::to_owned);
        out.push(format!("turns:{host}:{}?transport=tcp", tls.port()));
    }
    out
}

impl IceProvider {
    /// The servers for `cfg`; `tls_host` as in [`builtin_urls`].
    pub fn new(cfg: &TurnConfig, tls_host: Option<&str>) -> Self {
        let urls = if cfg.mode == TurnMode::Builtin && cfg.urls.is_empty() {
            builtin_urls(cfg, tls_host)
        } else {
            cfg.urls.clone()
        };
        let (turn_urls, stun_urls): (Vec<_>, Vec<_>) = urls
            .into_iter()
            .partition(|u| u.starts_with("turn:") || u.starts_with("turns:"));
        let relay = cfg.mode == TurnMode::External || cfg.relay;
        let (turn_urls, secret) = if relay {
            (turn_urls, cfg.shared_secret.clone())
        } else {
            (Vec::new(), String::new())
        };
        Self {
            stun_urls,
            turn_urls,
            secret,
            ttl: cfg.credential_ttl,
        }
    }

    /// Also hand out the extra STUN endpoints (`[[turn.stun]]`, public
    /// addresses) as `stun:` URLs.
    pub fn with_stun_endpoints(mut self, endpoints: &[SocketAddr]) -> Self {
        for addr in endpoints {
            let url = format!("stun:{}:{}", host_text(addr.ip()), addr.port());
            if !self.stun_urls.contains(&url) {
                self.stun_urls.push(url);
            }
        }
        self
    }

    /// Whether relay (TURN) credentials are issued.
    pub fn has_turn(&self) -> bool {
        !self.turn_urls.is_empty() && !self.secret.is_empty()
    }

    /// `username = "<expiry unix s>:<session_id>"`,
    /// `credential = base64(HMAC-SHA1(secret, username))`.
    pub fn credentials(&self, session_id: &str) -> (String, String) {
        let expiry = crate::db::now_s() + self.ttl.as_secs() as i64;
        let username = format!("{expiry}:{session_id}");
        let password = crate::crypto::turn_password(&self.secret, &username);
        (username, password)
    }

    pub fn servers(&self, session_id: &str) -> Vec<IceServer> {
        let mut out = Vec::new();
        if !self.stun_urls.is_empty() {
            out.push(IceServer {
                urls: self.stun_urls.clone(),
                username: String::new(),
                credential: String::new(),
            });
        }
        if self.has_turn() {
            let (username, credential) = self.credentials(session_id);
            out.push(IceServer {
                urls: self.turn_urls.clone(),
                username,
                credential,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_format() {
        let cfg = TurnConfig {
            mode: TurnMode::External,
            urls: vec![
                "stun:t.example:3478".into(),
                "turn:t.example:3478?transport=udp".into(),
            ],
            shared_secret: "s3cret".into(),
            ..TurnConfig::default()
        };
        let p = IceProvider::new(&cfg, None);
        let servers = p.servers("sid-1");
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].urls, vec!["stun:t.example:3478".to_string()]);
        assert!(servers[0].username.is_empty());
        let (expiry, sid) = servers[1].username.split_once(':').unwrap();
        assert_eq!(sid, "sid-1");
        let expiry: i64 = expiry.parse().unwrap();
        assert!((expiry - crate::db::now_s() - 43200).abs() <= 2);
        assert_eq!(
            servers[1].credential,
            crate::crypto::turn_password("s3cret", &servers[1].username)
        );
    }

    #[test]
    fn builtin_without_relay_is_stun_only() {
        let cfg = TurnConfig {
            relay: false,
            shared_secret: "s".into(),
            listen_udp: None,
            listen_tcp: None,
            listen_tls: None,
            public_ip: Some("203.0.113.10".parse().unwrap()),
            ..TurnConfig::default()
        };
        let p = IceProvider::new(&cfg, None)
            .with_stun_endpoints(&["203.0.113.10:3479".parse().unwrap()]);
        assert!(!p.has_turn());
        let servers = p.servers("x");
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].urls, ["stun:203.0.113.10:3479"]);
        assert!(servers[0].credential.is_empty());
    }

    #[test]
    fn turn_over_tls_uses_the_certificate_name() {
        let cfg = TurnConfig {
            shared_secret: "s".into(),
            public_ip: Some("203.0.113.10".parse().unwrap()),
            ..TurnConfig::default()
        };
        let urls = |host| IceProvider::new(&cfg, host).servers("x")[1].urls.clone();
        assert_eq!(
            urls(None),
            [
                "turn:203.0.113.10:3478?transport=udp",
                "turn:203.0.113.10:3478?transport=tcp",
                "turns:203.0.113.10:5349?transport=tcp"
            ]
        );
        assert_eq!(urls(Some("farwing.example.com"))[2], "turns:farwing.example.com:5349?transport=tcp");
    }

    #[test]
    fn extra_stun_endpoints() {
        let cfg = TurnConfig {
            mode: TurnMode::Builtin,
            shared_secret: "s".into(),
            listen_udp: Some("0.0.0.0:3478".parse().unwrap()),
            listen_tcp: Some("0.0.0.0:3478".parse().unwrap()),
            listen_tls: None,
            public_ip: Some("203.0.113.10".parse().unwrap()),
            ..TurnConfig::default()
        };
        // The last one duplicates the primary STUN URL.
        let extra: Vec<SocketAddr> = [
            "203.0.113.10:3479",
            "[2001:db8::1]:3479",
            "203.0.113.10:3478",
        ]
        .iter()
        .map(|a| a.parse().unwrap())
        .collect();
        let servers = IceProvider::new(&cfg, None)
            .with_stun_endpoints(&extra)
            .servers("x");
        assert_eq!(servers.len(), 2);
        assert_eq!(
            servers[0].urls,
            [
                "stun:203.0.113.10:3478",
                "stun:203.0.113.10:3479",
                "stun:[2001:db8::1]:3479"
            ]
        );
        assert_eq!(
            servers[1].urls,
            [
                "turn:203.0.113.10:3478?transport=udp",
                "turn:203.0.113.10:3478?transport=tcp"
            ]
        );
        assert!(!servers[1].credential.is_empty());
    }
}
