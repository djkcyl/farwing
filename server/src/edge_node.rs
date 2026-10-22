//! `farwing-server edge`: an edge relay node (`docs/protocol/deployment-design.md`
//! §4). It joins the control node once with the link from the web console,
//! then keeps one signed WebSocket to it, runs the relay and NAT discovery
//! endpoints it is told to, and reports its health every ten seconds. The
//! relay keeps running while the connection is down.

use crate::config::TurnConfig;
use crate::edge::{FromNode, Health, ToNode};
use crate::http::nodes::{JOIN_HEADER, KEY_HEADER, NODE_HEADER, auth_message};
use crate::{stun_server, turn_server};
use anyhow::{Context, Result, anyhow, bail};
use farwing_protocol::crypto::DeviceKey;
use farwing_protocol::headers;
use futures_util::{SinkExt, StreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const HEALTH_INTERVAL: Duration = Duration::from_secs(10);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// What a node keeps in its data directory (`edge.json`, private).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Identity {
    /// The control node's base URL (its public URL).
    control: String,
    /// The control certificate's SHA-256 fingerprint; empty: trust the
    /// system's roots.
    fingerprint: String,
    /// Empty until joined.
    node_id: String,
    /// Ed25519 seed, base64.
    key: String,
    /// The join token while the join is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    join: Option<String>,
}

fn identity_path(data_dir: &Path) -> PathBuf {
    data_dir.join("edge.json")
}

impl Identity {
    fn load(data_dir: &Path) -> Result<Self> {
        let path = identity_path(data_dir);
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "reading {}: this node has not joined; run it with --join and the link from the web console",
                path.display()
            )
        })?;
        serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))
    }

    fn save(&self, data_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("creating {}", data_dir.display()))?;
        crate::tls::write_private(
            &identity_path(data_dir),
            serde_json::to_string_pretty(self)?.as_bytes(),
        )
    }

    fn key(&self) -> Result<DeviceKey> {
        let seed: [u8; 32] = farwing_protocol::crypto::unb64(&self.key)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| anyhow!("the node key in edge.json is invalid"))?;
        Ok(DeviceKey::from_seed(&seed))
    }
}

/// `<base>/edge/<token>[#<fingerprint>]`.
fn parse_join(link: &str) -> Result<(String, String, String)> {
    let (link, fingerprint) = link.split_once('#').unwrap_or((link, ""));
    let (base, token) = link
        .rsplit_once("/edge/")
        .ok_or_else(|| anyhow!("not a join link: {link}"))?;
    let url = url::Url::parse(base).with_context(|| format!("not a join link: {link}"))?;
    if url.scheme() != "https" && url.scheme() != "http" {
        bail!("not a join link: {link}");
    }
    let token = token.trim_matches('/');
    if token.is_empty() {
        bail!("the join link has no token");
    }
    Ok((
        base.trim_end_matches('/').to_owned(),
        token.to_owned(),
        fingerprint.trim().to_uppercase(),
    ))
}

/// Accepts the pinned certificate, or one the system trusts for the name.
#[derive(Debug)]
struct ControlVerifier {
    pinned: String,
    platform: rustls_platform_verifier::Verifier,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for ControlVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if !self.pinned.is_empty() && crate::tls::fingerprint(end_entity.as_ref()) == self.pinned {
            return Ok(ServerCertVerified::assertion());
        }
        self.platform
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn client_tls(pinned: &str) -> Result<Arc<rustls::ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = ControlVerifier {
        pinned: pinned.to_owned(),
        platform: rustls_platform_verifier::Verifier::new(Arc::clone(&provider))
            .context("system certificate verifier")?,
        provider: Arc::clone(&provider),
    };
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS versions")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

type Socket = tokio_tungstenite::WebSocketStream<
    tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
>;

async fn connect(identity: &Identity, key: &DeviceKey) -> Result<Socket> {
    let base = url::Url::parse(&identity.control).context("control URL")?;
    if base.scheme() != "https" {
        bail!("the control node must be reached over https");
    }
    let host = base.host_str().ok_or_else(|| anyhow!("control URL has no host"))?;
    let port = base.port_or_known_default().unwrap_or(443);
    let mut ws_url = base.clone();
    ws_url.set_scheme("wss").map_err(|_| anyhow!("control URL"))?;
    ws_url.set_path(&format!("{}/api/v2/edge/ws", base.path().trim_end_matches('/')));
    let mut request = ws_url.as_str().into_client_request()?;
    let timestamp = crate::db::now_ms();
    let h = request.headers_mut();
    h.insert(headers::PROTOCOL, "2".parse()?);
    h.insert(headers::TIMESTAMP, timestamp.to_string().parse()?);
    match &identity.join {
        Some(token) => {
            let public_key = key.public_key();
            h.insert(JOIN_HEADER, token.parse()?);
            h.insert(KEY_HEADER, public_key.parse()?);
            h.insert(
                headers::SIGNATURE,
                key.sign(&auth_message(&public_key, timestamp)).parse()?,
            );
        }
        None => {
            h.insert(NODE_HEADER, identity.node_id.parse()?);
            h.insert(
                headers::SIGNATURE,
                key.sign(&auth_message(&identity.node_id, timestamp)).parse()?,
            );
        }
    }
    let tcp = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::TcpStream::connect((host.trim_matches(['[', ']']), port)),
    )
    .await
    .context("connecting to the control node timed out")?
    .with_context(|| format!("connecting to {host}:{port}"))?;
    tcp.set_nodelay(true).ok();
    let name = ServerName::try_from(host.trim_matches(['[', ']']).to_owned())
        .context("control host name")?;
    let tls = tokio_rustls::TlsConnector::from(client_tls(&identity.fingerprint)?)
        .connect(name, tcp)
        .await
        .context("TLS with the control node (is the fingerprint in the join link current?)")?;
    let (socket, _) = tokio_tungstenite::client_async(request, tls)
        .await
        .context("the control node refused this node")?;
    Ok(socket)
}

/// The relay this node runs now.
struct Relay {
    cfg: TurnConfig,
    turn: Option<turn_server::BuiltinTurn>,
    stun: Option<stun_server::StunEndpoints>,
    error: Option<String>,
    last_bytes: u64,
}

impl Relay {
    async fn stop(&mut self) {
        if let Some(turn) = self.turn.take() {
            turn.close().await;
        }
        if let Some(stun) = self.stun.take() {
            stun.close().await;
        }
    }

    async fn start(data_dir: &Path, node_id: &str, cfg: TurnConfig) -> Self {
        let mut relay = Relay {
            cfg: cfg.clone(),
            turn: None,
            stun: None,
            error: None,
            last_bytes: 0,
        };
        let names: Vec<String> = cfg
            .builtin_public_ip()
            .map(|ip| vec![ip.to_string()])
            .unwrap_or_else(|| vec!["localhost".to_owned()]);
        let tls = match crate::tls::self_signed_turns(&data_dir.join("tls"), &names) {
            Ok(tls) => Some(tls),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "no TURN over TLS on this node");
                None
            }
        };
        let auth = turn_server::RestAuthHandler::for_node(cfg.shared_secret.clone(), node_id);
        match turn_server::start_with(&cfg, tls, auth).await {
            Ok(turn) => relay.turn = Some(turn),
            Err(e) => relay.error = Some(format!("{e:#}")),
        }
        if !cfg.stun.is_empty() {
            match stun_server::start(&cfg).await {
                Ok(stun) => relay.stun = Some(stun),
                Err(e) => {
                    relay.error.get_or_insert(format!("{e:#}"));
                }
            }
        }
        if let Some(error) = &relay.error {
            tracing::error!(%error, "relay did not start");
        }
        relay
    }

    async fn health(&mut self, interval: Duration) -> Health {
        let mut health = Health {
            error: self.error.clone(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            ..Health::default()
        };
        if let Some(turn) = &self.turn {
            let stats = turn.stats().await;
            health.turn_udp = Some(turn.udp_addr());
            health.turn_tcp = turn.tcp_addr();
            health.turn_tls = turn.tls_addr();
            health.allocations = stats.allocations;
            health.sessions = stats.sessions;
            health.relayed_bps =
                stats.relayed_bytes.saturating_sub(self.last_bytes) / interval.as_secs().max(1);
            self.last_bytes = stats.relayed_bytes;
        }
        if let Some(stun) = &self.stun {
            health.stun = stun.local_addrs().to_vec();
        }
        health
    }
}

/// Run as an edge node: join with `join` first when given, then serve.
pub async fn run(data_dir: PathBuf, join: Option<String>) -> Result<()> {
    crate::tls::install_crypto_provider();
    let mut identity = match join {
        Some(link) => {
            let (control, token, fingerprint) = parse_join(&link)?;
            let identity = Identity {
                control,
                fingerprint,
                node_id: String::new(),
                key: farwing_protocol::crypto::b64(&DeviceKey::generate().seed()),
                join: Some(token),
            };
            identity.save(&data_dir)?;
            identity
        }
        None => Identity::load(&data_dir)?,
    };
    let key = identity.key()?;
    let mut relay: Option<Relay> = None;
    let mut backoff = Duration::from_secs(1);
    tracing::info!(control = %identity.control, "edge node starting");
    loop {
        let socket = tokio::select! {
            socket = connect(&identity, &key) => socket,
            _ = crate::server::shutdown_signal() => break,
        };
        let socket = match socket {
            Ok(socket) => {
                backoff = Duration::from_secs(1);
                socket
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), retry_in = ?backoff, "control node unreachable");
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = crate::server::shutdown_signal() => break,
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        tracing::info!(node = %identity.node_id, "connected to the control node");
        let (mut sink, mut stream) = socket.split();
        let mut ticks = tokio::time::interval(HEALTH_INTERVAL);
        let stopped = loop {
            tokio::select! {
                _ = crate::server::shutdown_signal() => break true,
                _ = ticks.tick() => {
                    let Some(relay) = relay.as_mut() else { continue };
                    let health = relay.health(HEALTH_INTERVAL).await;
                    let text = serde_json::to_string(&FromNode::Health(health))?;
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break false;
                    }
                }
                incoming = stream.next() => {
                    let text = match incoming {
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(Message::Close(frame))) => {
                            tracing::warn!(?frame, "the control node closed the connection");
                            break false;
                        }
                        Some(Ok(_)) => continue,
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "connection to the control node lost");
                            break false;
                        }
                        None => break false,
                    };
                    match serde_json::from_str::<ToNode>(&text) {
                        Ok(ToNode::Joined { node_id }) => {
                            identity.node_id = node_id;
                            identity.join = None;
                            identity.save(&data_dir)?;
                            tracing::info!(node = %identity.node_id, "joined the control node");
                        }
                        Ok(ToNode::Config { turn }) => {
                            if relay.as_ref().is_some_and(|r| r.cfg == *turn) {
                                continue;
                            }
                            if let Some(mut old) = relay.take() {
                                old.stop().await;
                            }
                            tracing::info!(public_ip = ?turn.public_ip, "relay settings received");
                            relay = Some(Relay::start(&data_dir, &identity.node_id, *turn).await);
                            ticks.reset_immediately();
                        }
                        Ok(ToNode::ControlFingerprint { fingerprint }) => {
                            identity.fingerprint = fingerprint;
                            identity.save(&data_dir)?;
                            tracing::info!("the control node's certificate changed");
                        }
                        Err(e) => tracing::debug!(error = %e, "unreadable message from the control node"),
                    }
                }
            }
        };
        if stopped {
            let _ = sink.send(Message::Close(None)).await;
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = crate::server::shutdown_signal() => break,
        }
    }
    if let Some(mut relay) = relay {
        relay.stop().await;
    }
    tracing::info!("edge node stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_links() {
        let (base, token, fp) =
            parse_join("https://farwing.example.com:8443/edge/fwj_abc#aa:bb").unwrap();
        assert_eq!(base, "https://farwing.example.com:8443");
        assert_eq!(token, "fwj_abc");
        assert_eq!(fp, "AA:BB");
        let (base, _, fp) = parse_join("https://example.com/farwing/edge/fwj_x").unwrap();
        assert_eq!(base, "https://example.com/farwing");
        assert!(fp.is_empty());
        assert!(parse_join("https://example.com/").is_err());
        assert!(parse_join("ftp://example.com/edge/x").is_err());
    }
}
