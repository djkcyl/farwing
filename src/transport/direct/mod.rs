//! Direct connections without the server (`docs/protocol/v2-design.md` §12).
//!
//! A host runs a small signalling endpoint ([`host`]): TLS over TCP, the
//! realtime [`Envelope`]s length-prefixed, and a handshake that binds both
//! device keys to the TLS channel. A controller connects to it with
//! [`client`]; the session that follows is negotiated exactly as through the
//! server. [`lan`] finds hosts on the local network; [`store`] keeps paired
//! devices, the account's device list and the pairing code.

pub(crate) mod client;
pub(crate) mod host;
pub(crate) mod lan;
pub(crate) mod managed;
pub(crate) mod store;

use crate::account::api::ClientIdentity;
use anyhow::{Context, Result, bail, ensure};
use farwing_protocol::direct::{self as wire, DirectHello};
use farwing_protocol::realtime::Envelope;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(crate) const ROLE_HOST: &str = "host";
pub(crate) const ROLE_CONTROLLER: &str = "controller";

// ------------------------------------------------------------ local device

/// This installation as a direct peer.
#[derive(Clone)]
pub(crate) struct LocalDevice {
    /// Device key and the ID derived from it: the same with or without a
    /// server (`docs/access-model.md` §2.1).
    pub identity: ClientIdentity,
    pub name: String,
}

impl LocalDevice {
    pub(crate) fn from_identity(identity: ClientIdentity, name: String) -> Result<Self> {
        let id = farwing_protocol::crypto::local_device_id(&identity.public_key())
            .context("本机设备密钥无效")?;
        Ok(Self {
            identity: identity.with_device_id(id),
            name,
        })
    }
}

/// This profile's device ID (derived from its key), read once per process.
/// For per-frame use in the GUI; the key of a profile does not change.
pub(crate) fn local_device_id() -> Option<String> {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(id) = ID.get() {
        return Some(id.clone());
    }
    let id = local_device().ok()?.identity.device_id;
    Some(ID.get_or_init(|| id).clone())
}

/// This profile's device: its saved key, registered or not.
pub(crate) fn local_device() -> Result<LocalDevice> {
    let native = crate::account::auth::KeyringIdentityStore::new()?.load_or_create()?;
    let name = device_name(&native);
    LocalDevice::from_identity(native.client_identity()?, name)
}

/// The name this device shows to direct peers.
pub(crate) fn device_name(native: &crate::account::auth::NativeIdentity) -> String {
    native
        .device_name()
        .map(str::to_owned)
        .or_else(|| crate::platform::windows::device_profile::computer_name().ok())
        .unwrap_or_else(|| native.suggested_name())
}

/// A peer-supplied name, safe to show.
pub(crate) fn sanitize_name(text: &str) -> String {
    let name: String = text
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect::<String>()
        .trim()
        .to_owned();
    if name.is_empty() { "未命名设备".into() } else { name }
}

// ------------------------------------------------------------------ target

/// A host address typed by the user: `name-or-ip[:port]`, IPv6 in brackets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub host: String,
    pub port: u16,
}

impl Target {
    pub(crate) fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        ensure!(!text.is_empty(), "请输入被控端的地址");
        ensure!(
            !text.contains("://") && !text.contains('/'),
            "地址格式应为 主机名或 IP[:端口]"
        );
        let default = farwing_protocol::direct::DEFAULT_PORT;
        if let Some(rest) = text.strip_prefix('[') {
            let (host, tail) = rest.split_once(']').context("IPv6 地址缺少 ]")?;
            host.parse::<std::net::Ipv6Addr>()
                .context("IPv6 地址无效")?;
            let port = match tail.strip_prefix(':') {
                Some(port) => port.parse().context("端口无效")?,
                None if tail.is_empty() => default,
                None => bail!("地址格式应为 [IPv6]:端口"),
            };
            return Ok(Self {
                host: host.to_owned(),
                port,
            });
        }
        if text.parse::<std::net::Ipv6Addr>().is_ok() {
            return Ok(Self {
                host: text.to_owned(),
                port: default,
            });
        }
        let (host, port) = match text.rsplit_once(':') {
            Some((host, port)) => (host, port.parse().context("端口无效")?),
            None => (text, default),
        };
        ensure!(port != 0, "端口无效");
        ensure!(
            !host.is_empty()
                && host.len() <= 253
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_'),
            "主机名无效"
        );
        Ok(Self {
            host: host.to_owned(),
            port,
        })
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

// ----------------------------------------------------------------- framing

/// Read one frame; `None` at a clean end of stream.
pub(crate) async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Envelope>> {
    let mut length = [0u8; 4];
    match reader.read_exact(&mut length).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error).context("读取直连数据失败"),
    }
    let length = u32::from_be_bytes(length) as usize;
    ensure!(
        (1..=wire::MAX_FRAME).contains(&length),
        "直连数据帧长度无效（{length}）"
    );
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .await
        .context("读取直连数据失败")?;
    let text = std::str::from_utf8(&body).context("直连数据不是 UTF-8")?;
    let envelope = Envelope::decode(text).context("直连数据格式无效")?;
    ensure!(
        envelope.v == farwing_protocol::PROTOCOL_VERSION,
        "对端协议版本 {} 不受支持",
        envelope.v
    );
    Ok(Some(envelope))
}

pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, envelope: &Envelope) -> Result<()> {
    let text = envelope.encode();
    ensure!(text.len() <= wire::MAX_FRAME, "直连数据帧过大");
    let mut frame = Vec::with_capacity(4 + text.len());
    frame.extend_from_slice(&(text.len() as u32).to_be_bytes());
    frame.extend_from_slice(text.as_bytes());
    writer.write_all(&frame).await.context("发送直连数据失败")?;
    writer.flush().await.context("发送直连数据失败")
}

// --------------------------------------------------------------------- TLS

/// Server configuration with a self-signed certificate, created once per
/// process. The certificate is not trusted by anyone: the handshake proofs
/// authenticate the devices.
pub(crate) fn server_tls() -> Result<Arc<rustls::ServerConfig>> {
    static CONFIG: std::sync::OnceLock<Arc<rustls::ServerConfig>> = std::sync::OnceLock::new();
    if let Some(config) = CONFIG.get() {
        return Ok(Arc::clone(config));
    }
    crate::transport::init_tls();
    let certified = rcgen::generate_simple_self_signed(vec!["farwing.direct".to_owned()])
        .context("生成直连证书失败")?;
    let certificate = certified.cert.der().clone();
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()),
    );
    let mut config = rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .context("配置直连 TLS 失败")?;
    config.alpn_protocols = vec![b"farwing-direct/2".to_vec()];
    Ok(Arc::clone(CONFIG.get_or_init(|| Arc::new(config))))
}

/// Client configuration accepting any certificate (see [`server_tls`]).
pub(crate) fn client_tls() -> Result<Arc<rustls::ClientConfig>> {
    crate::transport::init_tls();
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .context("TLS 加密提供程序未初始化")?;
    let mut config = rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"farwing-direct/2".to_vec()];
    Ok(Arc::new(config))
}

#[derive(Debug)]
struct AnyCertificate(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _leaf: &rustls::pki_types::CertificateDer<'_>,
        _chain: &[rustls::pki_types::CertificateDer<'_>],
        _name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        // Authenticity comes from the device-key proofs over the exported
        // keying material of this very TLS session.
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        signed: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            signed,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        signed: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            signed,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Keying material exported from an established TLS session.
pub(crate) fn exported_key<Data>(
    connection: &rustls::ConnectionCommon<Data>,
) -> Result<[u8; wire::EXPORTER_LEN]> {
    connection
        .export_keying_material([0u8; wire::EXPORTER_LEN], wire::EXPORTER_LABEL, None)
        .context("导出直连 TLS 密钥材料失败")
}

// ------------------------------------------------------------------- hello

pub(crate) fn hello(
    local: &LocalDevice,
    role: &str,
    ekm: &[u8],
    methods: Vec<String>,
    udp_port: u16,
) -> DirectHello {
    DirectHello {
        device_id: local.identity.device_id.clone(),
        public_key: local.identity.public_key(),
        name: local.name.clone(),
        platform: farwing_protocol::Platform::Windows,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        proof: local
            .identity
            .sign(&farwing_protocol::crypto::direct_proof_message(role, ekm)),
        methods,
        udp_port,
    }
}

/// Check a peer's `direct.hello` of `role` on this TLS session.
pub(crate) fn verify_hello(hello: &DirectHello, role: &str, ekm: &[u8]) -> Result<()> {
    crate::account::api::validate_device_id(&hello.device_id).context("对端设备 ID 无效")?;
    ensure!(
        farwing_protocol::crypto::valid_public_key(&hello.public_key),
        "对端设备公钥无效"
    );
    // The device ID is derived from the key: a peer cannot claim another
    // device's ID (`docs/access-model.md` §2.1).
    if farwing_protocol::crypto::local_device_id(&hello.public_key).as_deref()
        != Some(hello.device_id.as_str())
    {
        return Err(crate::transport::signal::identity::IdentityMismatch {
            device_id: hello.device_id.clone(),
        }
        .into());
    }
    farwing_protocol::crypto::verify(
        &hello.public_key,
        &farwing_protocol::crypto::direct_proof_message(role, ekm),
        &hello.proof,
    )
    .map_err(|_| anyhow::anyhow!("对端设备身份证明无效"))?;
    Ok(())
}

/// The error body of a refused handshake step.
pub(crate) fn refusal(code: farwing_protocol::ErrorCode, message: &str) -> farwing_protocol::ErrorBody {
    farwing_protocol::ErrorBody::new(code, message)
}

// ------------------------------------------------------- per-session state

tokio::task_local! {
    /// The UDP port range of the direct session being prepared on this task.
    static SESSION_PORTS: (u16, u16);
}

/// Run `future` (a host session's preparation) with the UDP ports of a
/// direct session, so its ICE agent binds there (§12.3).
pub(crate) async fn with_session_ports<F: std::future::Future>(ports: Option<(u16, u16)>, future: F) -> F::Output {
    match ports {
        Some(ports) => SESSION_PORTS.scope(ports, future).await,
        None => future.await,
    }
}

/// Apply the direct-session UDP ports of the current task, if any.
pub(crate) fn apply_session_ports(settings: &mut webrtc::api::setting_engine::SettingEngine) {
    let Ok((first, last)) = SESSION_PORTS.try_with(|ports| *ports) else {
        return;
    };
    // Every direct session shares the one UDP port (`UDP_PORTS`).
    let _ = last;
    match webrtc::ice::udp_network::EphemeralUDP::shared(first) {
        Ok(range) => {
            settings.set_udp_network(webrtc::ice::udp_network::UDPNetwork::Ephemeral(range));
            // ICE-TCP would take the same range and could shadow the endpoint
            // port on an interface address; a direct session is UDP only.
            settings.set_network_types(vec![
                webrtc::ice::network_type::NetworkType::Udp4,
                webrtc::ice::network_type::NetworkType::Udp6,
            ]);
            tracing::debug!(first, last, "direct session ICE ports");
        }
        Err(error) => tracing::warn!(%error, "direct session ICE ports rejected"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_parse() {
        let port = farwing_protocol::direct::DEFAULT_PORT;
        assert_eq!(
            Target::parse("192.168.1.5").unwrap(),
            Target { host: "192.168.1.5".into(), port }
        );
        assert_eq!(
            Target::parse("pc.example.com:3000").unwrap(),
            Target { host: "pc.example.com".into(), port: 3000 }
        );
        assert_eq!(
            Target::parse("[fe80::1]:40000").unwrap(),
            Target { host: "fe80::1".into(), port: 40000 }
        );
        assert_eq!(Target::parse("::1").unwrap().port, port);
        assert_eq!(Target::parse("[::1]").unwrap().to_string(), format!("[::1]:{port}"));
        assert!(Target::parse("http://x").is_err());
        assert!(Target::parse("a b").is_err());
        assert!(Target::parse("host:0").is_err());
    }

    #[test]
    fn hello_proofs_bind_role_and_channel() {
        let key = farwing_protocol::crypto::DeviceKey::generate();
        let local = LocalDevice::from_identity(
            ClientIdentity::new("c", "", &key.seed()).unwrap(),
            "PC".into(),
        )
        .unwrap();
        assert!(farwing_protocol::ids::is_device_id(&local.identity.device_id));
        let ekm = [9u8; 32];
        let message = hello(&local, ROLE_HOST, &ekm, Vec::new(), 0);
        verify_hello(&message, ROLE_HOST, &ekm).unwrap();
        assert!(verify_hello(&message, ROLE_CONTROLLER, &ekm).is_err());
        assert!(verify_hello(&message, ROLE_HOST, &[1u8; 32]).is_err());
    }

    #[tokio::test]
    async fn frames_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        let envelope = Envelope::new(farwing_protocol::realtime::types::PING, serde_json::json!({"t": 1}));
        write_frame(&mut a, &envelope).await.unwrap();
        drop(a);
        let read = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(read.kind, farwing_protocol::realtime::types::PING);
        assert!(read_frame(&mut b).await.unwrap().is_none());
    }
}
