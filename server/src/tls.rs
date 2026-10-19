//! TLS configuration: certificate files, a persistent self-signed
//! certificate, or an automatically renewed ACME certificate (`acme.rs`).

use crate::config::{Config, TlsMode};
use anyhow::{Context, Result, bail};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Install the `ring` provider as the process default (idempotent).
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub struct TlsMaterial {
    pub config: Arc<rustls::ServerConfig>,
    /// SHA-256 fingerprint of the leaf certificate (`AA:BB:...`).
    pub fingerprint: String,
    pub cert_path: PathBuf,
    /// The ACME manager (`mode = "acme"`), to be run once listening.
    pub acme: Option<Arc<crate::acme::Acme>>,
}

/// The certificate the server presents, for display: fixed, or renewed by
/// ACME.
#[derive(Clone)]
pub enum Presented {
    Fixed(String),
    Acme(Arc<crate::acme::Acme>),
}

impl Presented {
    pub fn fingerprint(&self) -> String {
        match self {
            Self::Fixed(fp) => fp.clone(),
            Self::Acme(acme) => acme.fingerprint(),
        }
    }
}

/// Upper-case colon-separated SHA-256 of a DER certificate.
pub fn fingerprint(der: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Load (or generate) the certificate. `None` in proxy mode.
pub fn load(cfg: &Config) -> Result<Option<TlsMaterial>> {
    if cfg.tls.mode == TlsMode::Proxy {
        return Ok(None);
    }
    if cfg.tls.mode == TlsMode::Acme {
        let acme = crate::acme::Acme::prepare(cfg)?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
            .context("TLS versions")?
            .with_no_client_auth()
            .with_cert_resolver(acme.resolver());
        config.alpn_protocols = vec![b"http/1.1".to_vec(), crate::acme::ACME_TLS_ALPN.to_vec()];
        return Ok(Some(TlsMaterial {
            config: Arc::new(config),
            fingerprint: acme.fingerprint(),
            cert_path: cfg.acme_dir().join("cert.pem"),
            acme: Some(acme),
        }));
    }
    let (cert_path, key_path) = if cfg.tls.self_signed {
        ensure_self_signed(&cfg.tls_dir(), &cfg.tls.hostnames, &cfg.public_url)?
    } else {
        (
            cfg.tls.cert.clone().context("[tls] cert missing")?,
            cfg.tls.key.clone().context("[tls] key missing")?,
        )
    };
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&cert_path)
        .with_context(|| format!("reading {}", cert_path.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("parsing {}", cert_path.display()))?;
    if certs.is_empty() {
        bail!("{} contains no certificate", cert_path.display());
    }
    let key = PrivateKeyDer::from_pem_file(&key_path)
        .with_context(|| format!("reading private key {}", key_path.display()))?;
    let fp = fingerprint(certs[0].as_ref());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .context("TLS versions")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("certificate and key do not match")?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Some(TlsMaterial {
        config: Arc::new(config),
        fingerprint: fp,
        cert_path,
        acme: None,
    }))
}

/// Generate `cert.pem`/`key.pem` in `dir` on first use; reuse them afterwards.
pub fn ensure_self_signed(
    dir: &Path,
    hostnames: &[String],
    public_url: &str,
) -> Result<(PathBuf, PathBuf)> {
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    if cert_path.exists() && key_path.exists() {
        return Ok((cert_path, key_path));
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut names: Vec<String> = hostnames
        .iter()
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .collect();
    if let Some(host) = url::Url::parse(public_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_matches(['[', ']']).to_owned()))
        && !names.contains(&host)
    {
        names.push(host);
    }
    if names.is_empty() {
        names.push("localhost".to_owned());
    }
    let ck = rcgen::generate_simple_self_signed(names).context("generating certificate")?;
    write_private(&key_path, ck.signing_key.serialize_pem().as_bytes())?;
    std::fs::write(&cert_path, ck.cert.pem())
        .with_context(|| format!("writing {}", cert_path.display()))?;
    tracing::info!(path = %cert_path.display(), "generated self-signed certificate");
    Ok((cert_path, key_path))
}

/// Write a private file (mode 0600 on Unix).
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("writing {}", path.display()))?;
        f.write_all(data)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, data).with_context(|| format!("writing {}", path.display()))
    }
}
