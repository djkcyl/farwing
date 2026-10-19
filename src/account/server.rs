//! The Farwing Server this profile talks to, and how its certificate is trusted.
//!
//! The setting belongs to the current profile and is stored as a small JSON file
//! in the profile's data directory. It contains no secrets: the account and
//! device credentials live in the operating-system credential store.
//!
//! Self-hosted servers often use a private CA or a self-signed certificate. A
//! server is trusted when its chain validates against the system roots (plus an
//! optional extra CA), or when its leaf certificate matches a SHA-256 pin that
//! the user confirmed.

use anyhow::{Context, Result, bail};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use url::Url;

const FILE_NAME: &str = "server.json";
const SCHEMA: u8 = 1;
/// Overrides the saved server for this process (automation and tests).
const URL_ENV: &str = "FARWING_SERVER_URL";
const PIN_ENV: &str = "FARWING_SERVER_PIN";

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Stored {
    schema: u8,
    url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pinned_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ca_pem: Option<String>,
}

/// A validated server setting.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerConfig {
    origin: Url,
    pinned_sha256: Option<[u8; 32]>,
    ca_pem: Option<String>,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("origin", &self.origin.as_str())
            .field("pinned", &self.pinned_sha256.is_some())
            .field("custom_ca", &self.ca_pem.is_some())
            .finish()
    }
}

/// While the background service hosts this profile the setting lives in the
/// service vault: the SYSTEM resident cannot read the user's profile.
fn directory() -> Result<std::path::PathBuf> {
    if managed() {
        crate::platform::windows::host_service::vault::root()
    } else {
        crate::product::local_data_dir()
    }
}

fn managed() -> bool {
    crate::platform::windows::host_service::vault::applies().unwrap_or(false)
}

fn copy_setting(from: &std::path::Path, to: &std::path::Path) -> Result<bool> {
    let source = from.join(FILE_NAME);
    let bytes = match std::fs::read(&source) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("读取服务器设置失败"),
    };
    let stored: Stored = serde_json::from_slice(&bytes).context("服务器设置文件无效")?;
    anyhow::ensure!(stored.schema == SCHEMA, "服务器设置版本不受支持");
    std::fs::create_dir_all(to).context("创建配置目录失败")?;
    let target = to.join(FILE_NAME);
    let temporary = target.with_extension("json.tmp");
    std::fs::write(&temporary, &bytes).context("保存服务器设置失败")?;
    std::fs::rename(&temporary, &target).context("保存服务器设置失败")?;
    Ok(true)
}

impl ServerConfig {
    /// Parse a user-entered address. `host[:port]` implies `https://`.
    pub fn new(address: &str, pinned_sha256: Option<&str>, ca_pem: Option<String>) -> Result<Self> {
        let address = address.trim();
        if address.is_empty() {
            bail!("请输入服务器地址");
        }
        let text = if address.contains("://") {
            address.to_owned()
        } else {
            format!("https://{address}")
        };
        let mut url = Url::parse(&text).context("服务器地址格式不正确")?;
        if url.scheme() != "https" {
            bail!("服务器地址必须使用 https://");
        }
        if url.host_str().is_none_or(str::is_empty) {
            bail!("服务器地址缺少主机名");
        }
        if !url.username().is_empty() || url.password().is_some() {
            bail!("服务器地址不能包含用户名或密码");
        }
        url.set_path("");
        url.set_query(None);
        url.set_fragment(None);
        let pinned_sha256 = pinned_sha256
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(parse_fingerprint)
            .transpose()?;
        if let Some(pem) = &ca_pem {
            parse_ca(pem)?;
        }
        Ok(Self {
            origin: url,
            pinned_sha256,
            ca_pem,
        })
    }

    /// The configured server, if any. The environment override wins.
    pub fn load() -> Result<Option<Self>> {
        if let Some(url) = std::env::var_os(URL_ENV) {
            let url = url.to_string_lossy().into_owned();
            let pin = std::env::var(PIN_ENV).ok();
            return Self::new(&url, pin.as_deref(), None).map(Some);
        }
        let path = directory()?.join(FILE_NAME);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("读取服务器设置失败"),
        };
        let stored: Stored = serde_json::from_slice(&bytes).context("服务器设置文件无效")?;
        anyhow::ensure!(stored.schema == SCHEMA, "服务器设置版本不受支持");
        Self::new(&stored.url, stored.pinned_sha256.as_deref(), stored.ca_pem).map(Some)
    }

    /// The configured server, or an error telling the user to configure one.
    pub fn require() -> Result<Self> {
        Self::load()?.ok_or_else(|| anyhow::Error::new(NoServer))
    }

    pub fn save(&self) -> Result<()> {
        let directory = directory()?;
        std::fs::create_dir_all(&directory).context("创建配置目录失败")?;
        let stored = Stored {
            schema: SCHEMA,
            url: self.origin().to_owned(),
            pinned_sha256: self.pinned_sha256.map(hex),
            ca_pem: self.ca_pem.clone(),
        };
        let path = directory.join(FILE_NAME);
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(&stored)?)
            .context("保存服务器设置失败")?;
        std::fs::rename(&temporary, &path).context("保存服务器设置失败")?;
        Ok(())
    }

    pub fn clear() -> Result<()> {
        let path = directory()?.join(FILE_NAME);
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("删除服务器设置失败"),
        }
    }

    /// Copy the profile's setting into the service vault (installation).
    /// Returns whether a setting was copied.
    pub(crate) fn enroll() -> Result<bool> {
        copy_setting(
            &crate::product::local_data_dir()?,
            &crate::platform::windows::host_service::vault::root()?,
        )
    }

    /// Copy the vault's setting back to the profile (uninstallation).
    pub(crate) fn restore_portable() -> Result<bool> {
        copy_setting(
            &crate::platform::windows::host_service::vault::root()?,
            &crate::product::local_data_dir()?,
        )
    }

    /// `https://host[:port]` without a trailing slash.
    pub fn origin(&self) -> &str {
        self.origin.as_str().trim_end_matches('/')
    }

    /// Stable key that binds credentials to this server.
    pub fn key(&self) -> String {
        self.origin().to_ascii_lowercase()
    }

    pub fn host(&self) -> &str {
        self.origin.host_str().unwrap_or_default()
    }

    pub fn pinned_fingerprint(&self) -> Option<String> {
        self.pinned_sha256.map(hex)
    }

    pub fn has_custom_ca(&self) -> bool {
        self.ca_pem.is_some()
    }

    /// Absolute URL of a server path.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin())
    }

    /// Whether a server-supplied realtime endpoint belongs to this server.
    /// Endpoints may use another port of the same host, never another host.
    pub fn accepts_endpoint(&self, endpoint: &Url) -> bool {
        endpoint.scheme() == "wss"
            && endpoint
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case(self.host()))
    }

    /// TLS configuration shared by HTTPS and WSS. `alpn` selects HTTP/1.1 for
    /// the HTTP client; the WebSocket handshake uses no ALPN.
    pub fn tls(&self, alpn_http1: bool) -> Result<Arc<rustls::ClientConfig>> {
        crate::transport::init_tls();
        let mut roots = rustls::RootCertStore::empty();
        let native = rustls_native_certs::load_native_certs();
        roots.add_parsable_certificates(native.certs);
        if let Some(pem) = &self.ca_pem {
            for certificate in parse_ca(pem)? {
                roots.add(certificate).context("自定义 CA 证书无效")?;
            }
        }
        if roots.is_empty() && self.pinned_sha256.is_none() {
            bail!("没有可用的受信任根证书，请导入 CA 或固定服务器证书指纹");
        }
        let webpki = if roots.is_empty() {
            None
        } else {
            Some(
                WebPkiServerVerifier::builder(Arc::new(roots))
                    .build()
                    .context("创建证书校验器失败")?,
            )
        };
        let provider = rustls::crypto::CryptoProvider::get_default()
            .cloned()
            .context("TLS 加密提供程序未初始化")?;
        let verifier = Arc::new(Verifier {
            webpki,
            pinned: self.pinned_sha256,
            provider,
        });
        let mut config = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        if alpn_http1 {
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
        }
        Ok(Arc::new(config))
    }
}

/// No server has been configured for this profile.
#[derive(Debug)]
pub struct NoServer;

impl std::fmt::Display for NoServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("尚未设置 Farwing 服务器，请先在登录页填写服务器地址（或运行 `Farwing server set <地址>`）")
    }
}

impl std::error::Error for NoServer {}

#[derive(Debug)]
struct Verifier {
    webpki: Option<Arc<WebPkiServerVerifier>>,
    pinned: Option<[u8; 32]>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        leaf: &CertificateDer<'_>,
        chain: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        // A pin is an explicit user decision about this exact certificate.
        // It replaces chain and name validation for that certificate only.
        if let Some(pinned) = self.pinned {
            let actual: [u8; 32] = Sha256::digest(leaf.as_ref()).into();
            if constant_time_eq(&actual, &pinned) {
                return Ok(ServerCertVerified::assertion());
            }
        }
        match &self.webpki {
            Some(verifier) => verifier.verify_server_cert(leaf, chain, name, ocsp, now),
            None => Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            )),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            signed,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            signed,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Result of inspecting a server certificate before trusting it.
#[derive(Clone, Debug)]
pub struct CertificateProbe {
    /// SHA-256 of the leaf certificate, lower-case hex.
    pub sha256: String,
    /// Whether the chain already validates against the system roots.
    pub trusted_by_system: bool,
}

/// Connect once and report the server certificate, so the user can compare
/// the fingerprint with the one printed by `farwing-server` and pin it.
pub async fn probe_certificate(address: &str) -> Result<CertificateProbe> {
    use tokio_rustls::TlsConnector;

    let config = ServerConfig::new(address, None, None)?;
    crate::transport::init_tls();
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .context("TLS 加密提供程序未初始化")?;
    let seen = Arc::new(Mutex::new(None::<Vec<u8>>));
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    let webpki = (!roots.is_empty())
        .then(|| WebPkiServerVerifier::builder(Arc::new(roots)).build())
        .transpose()
        .context("创建证书校验器失败")?;
    let recorder = Arc::new(Recorder {
        seen: Arc::clone(&seen),
        webpki,
        trusted: Mutex::new(false),
        provider,
    });
    let tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(recorder.clone())
        .with_no_client_auth();
    let host = config.host().trim_matches(['[', ']']).to_owned();
    let port = config.origin.port_or_known_default().unwrap_or(443);
    let stream = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let addresses = tokio::net::lookup_host((host.as_str(), port))
            .await?
            .collect::<Vec<_>>();
        crate::transport::happy_eyeballs::connect(&addresses).await
    })
    .await
    .context("连接服务器超时")?
    .context("无法连接服务器")?;
    let name = ServerName::try_from(host).context("服务器主机名无效")?;
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        TlsConnector::from(Arc::new(tls)).connect(name, stream),
    )
    .await
    .context("TLS 握手超时")?
    .context("TLS 握手失败")?;
    let leaf = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .context("服务器没有提供证书")?;
    let trusted_by_system = *recorder
        .trusted
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok(CertificateProbe {
        sha256: hex(Sha256::digest(&leaf).into()),
        trusted_by_system,
    })
}

#[derive(Debug)]
struct Recorder {
    seen: Arc<Mutex<Option<Vec<u8>>>>,
    webpki: Option<Arc<WebPkiServerVerifier>>,
    trusted: Mutex<bool>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for Recorder {
    fn verify_server_cert(
        &self,
        leaf: &CertificateDer<'_>,
        chain: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        *self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(leaf.to_vec());
        let trusted = self
            .webpki
            .as_ref()
            .is_some_and(|v| v.verify_server_cert(leaf, chain, name, ocsp, now).is_ok());
        *self
            .trusted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = trusted;
        // Inspection only: the probe connection is closed without sending data.
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            signed,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            signed,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn parse_fingerprint(value: &str) -> Result<[u8; 32]> {
    let digits: String = value
        .chars()
        .filter(|c| !matches!(c, ':' | ' ' | '-'))
        .collect();
    anyhow::ensure!(
        digits.len() == 64 && digits.bytes().all(|b| b.is_ascii_hexdigit()),
        "证书指纹应为 64 位十六进制 SHA-256"
    );
    let mut output = [0u8; 32];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digits[index * 2..index * 2 + 2], 16)?;
    }
    Ok(output)
}

fn parse_ca(pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    use rustls::pki_types::pem::PemObject;
    let certificates = CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("CA 证书不是有效的 PEM")?;
    anyhow::ensure!(!certificates.is_empty(), "CA 文件中没有证书");
    Ok(certificates)
}

fn hex(bytes: [u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_addresses() {
        let config = ServerConfig::new("farwing.example.com:8443/path?x=1", None, None).unwrap();
        assert_eq!(config.origin(), "https://farwing.example.com:8443");
        assert!(ServerConfig::new("http://example.com", None, None).is_err());
        let pin = "AB:".repeat(31) + "AB";
        let config = ServerConfig::new("example.com", Some(&pin), None).unwrap();
        assert_eq!(config.pinned_fingerprint().unwrap(), "ab".repeat(32));
        let endpoint = Url::parse("wss://EXAMPLE.com:9443/api/v2/ws").unwrap();
        assert!(config.accepts_endpoint(&endpoint));
        assert!(!config.accepts_endpoint(&Url::parse("wss://other.com/api/v2/ws").unwrap()));
    }
}
