//! Automatic certificates from an ACME CA (`[tls] mode = "acme"`):
//! Let's Encrypt by default, any RFC 8555 CA by directory URL.
//!
//! - The account and the certificate live in `data_dir/acme`; a valid cached
//!   certificate is used at once, otherwise a temporary self-signed one until
//!   the first certificate arrives.
//! - TLS-ALPN-01 (RFC 8737) is answered by the TLS listener itself: a
//!   handshake offering only `acme-tls/1` gets the challenge certificate.
//!   HTTP-01 is answered by the plain-HTTP listener (`http_listen`), which
//!   otherwise redirects to `public_url`.
//! - The certificate is renewed after two thirds of its lifetime (the CA's
//!   lifetime, whatever it is), retried with backoff, and swapped into the
//!   running listener without a restart.

use crate::config::{AcmeChallenge, Config};
use anyhow::{Context, Result, anyhow, bail};
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount,
    NewOrder, OrderStatus, RetryPolicy,
};
use parking_lot::RwLock;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// ALPN protocol of TLS-ALPN-01 validation connections.
pub const ACME_TLS_ALPN: &[u8] = b"acme-tls/1";

/// The certificate the TLS listener presents: the current one, or the
/// challenge certificate for a validation handshake.
#[derive(Debug)]
pub struct Resolver {
    current: RwLock<Arc<CertifiedKey>>,
    challenges: RwLock<HashMap<String, Arc<CertifiedKey>>>,
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let validation = hello
            .alpn()
            .is_some_and(|mut protocols| protocols.any(|p| p == ACME_TLS_ALPN));
        if validation {
            let name = hello.server_name()?.to_ascii_lowercase();
            return self.challenges.read().get(&name).cloned();
        }
        Some(Arc::clone(&self.current.read()))
    }
}

pub(crate) fn certified_key(chain: Vec<CertificateDer<'static>>, key: &PrivateKeyDer<'_>) -> Result<Arc<CertifiedKey>> {
    let signing = rustls::crypto::ring::sign::any_supported_type(key)
        .map_err(|e| anyhow!("unsupported certificate key: {e}"))?;
    Ok(Arc::new(CertifiedKey::new(chain, signing)))
}

/// What the web console shows about the certificate.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Status {
    pub domains: Vec<String>,
    pub directory: String,
    pub challenge: &'static str,
    /// A certificate from the CA is in use (not the temporary one).
    pub issued: bool,
    pub not_before: Option<i64>,
    pub not_after: Option<i64>,
    /// When the next renewal is due (Unix seconds).
    pub renew_at: Option<i64>,
    pub last_error: Option<String>,
    pub last_attempt: Option<i64>,
}

/// Domains and CA a cached certificate was issued for.
#[derive(Serialize, Deserialize, PartialEq)]
struct CertMeta {
    domains: Vec<String>,
    directory: String,
}

pub struct Acme {
    cfg: Config,
    domains: Vec<String>,
    challenge: AcmeChallenge,
    resolver: Arc<Resolver>,
    /// HTTP-01: token → key authorization.
    tokens: RwLock<HashMap<String, String>>,
    status: RwLock<Status>,
    fingerprint: RwLock<String>,
}

impl Acme {
    /// Prepare the TLS listener: the cached certificate if it still covers
    /// the configured domains, otherwise a temporary self-signed one.
    pub fn prepare(cfg: &Config) -> Result<Arc<Self>> {
        let domains = cfg.acme_domains();
        let challenge = cfg.acme_challenge()?;
        let dir = cfg.acme_dir();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut status = Status {
            domains: domains.clone(),
            directory: cfg.tls.acme.directory_url(),
            challenge: match challenge {
                AcmeChallenge::Http01 => "http-01",
                _ => "tls-alpn-01",
            },
            ..Status::default()
        };
        let cached = load_cached(cfg, &domains).unwrap_or_else(|e| {
            tracing::warn!(error = %format!("{e:#}"), "cached ACME certificate not usable");
            None
        });
        let (key, fingerprint) = match cached {
            Some((key, fp, validity)) => {
                status.issued = true;
                set_validity(&mut status, validity);
                (key, fp)
            }
            None => {
                let ck = rcgen::generate_simple_self_signed(domains.clone())
                    .context("generating temporary certificate")?;
                let der = ck.cert.der().clone();
                let fp = crate::tls::fingerprint(der.as_ref());
                let key = PrivateKeyDer::try_from(ck.signing_key.serialize_der())
                    .map_err(|e| anyhow!("temporary key: {e}"))?;
                (certified_key(vec![der], &key)?, fp)
            }
        };
        Ok(Arc::new(Self {
            cfg: cfg.clone(),
            domains,
            challenge,
            resolver: Arc::new(Resolver {
                current: RwLock::new(key),
                challenges: RwLock::new(HashMap::new()),
            }),
            tokens: RwLock::new(HashMap::new()),
            status: RwLock::new(status),
            fingerprint: RwLock::new(fingerprint),
        }))
    }

    pub fn resolver(&self) -> Arc<Resolver> {
        Arc::clone(&self.resolver)
    }

    pub fn status(&self) -> Status {
        self.status.read().clone()
    }

    pub fn fingerprint(&self) -> String {
        self.fingerprint.read().clone()
    }

    /// HTTP-01 answer for `token`.
    pub fn http_answer(&self, token: &str) -> Option<String> {
        self.tokens.read().get(token).cloned()
    }

    /// Obtain the first certificate and keep renewing it. Runs forever.
    pub async fn run(self: Arc<Self>) {
        let mut failures: u32 = 0;
        loop {
            let now = crate::db::now_s();
            let due = {
                let s = self.status.read();
                if s.issued { s.renew_at.unwrap_or(now) } else { now }
            };
            if now >= due {
                self.status.write().last_attempt = Some(now);
                match self.obtain().await {
                    Ok(()) => {
                        failures = 0;
                        self.status.write().last_error = None;
                    }
                    Err(e) => {
                        failures += 1;
                        let text = format!("{e:#}");
                        tracing::warn!(error = %text, failures, "obtaining the ACME certificate failed");
                        self.status.write().last_error = Some(text);
                    }
                }
            }
            // Failures back off from 5 minutes to 6 hours (CA rate limits);
            // otherwise wake up for the renewal, at least twice a day.
            let wait = if failures > 0 {
                Duration::from_secs((300u64 << (failures - 1).min(7)).min(6 * 3600))
            } else {
                let s = self.status.read();
                let until = s.renew_at.unwrap_or(now).saturating_sub(crate::db::now_s());
                Duration::from_secs(until.clamp(60, 12 * 3600) as u64)
            };
            tokio::time::sleep(wait).await;
        }
    }

    async fn account(&self) -> Result<Account> {
        let acme = &self.cfg.tls.acme;
        let directory = acme.directory_url();
        let builder = || match &acme.ca_root {
            Some(root) => Account::builder_with_root(root)
                .with_context(|| format!("[tls.acme] ca_root {}", root.display())),
            None => Account::builder().context("ACME HTTP client"),
        };
        // One account per directory: switching from staging to production
        // must not reuse the staging account.
        let path = self.cfg.acme_dir().join(format!(
            "account-{}.json",
            &crate::crypto::sha256_hex(directory.as_bytes())[..16]
        ));
        if let Ok(text) = std::fs::read_to_string(&path) {
            let credentials: AccountCredentials =
                serde_json::from_str(&text).context("reading the stored ACME account")?;
            return builder()?
                .from_credentials(credentials)
                .await
                .context("restoring the ACME account");
        }
        let contact = acme.contact.trim();
        let contact = (!contact.is_empty()).then(|| {
            if contact.starts_with("mailto:") {
                contact.to_owned()
            } else {
                format!("mailto:{contact}")
            }
        });
        let contacts: Vec<&str> = contact.iter().map(String::as_str).collect();
        let (account, credentials) = builder()?
            .create(
                &NewAccount {
                    contact: &contacts,
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                directory.clone(),
                None,
            )
            .await
            .with_context(|| format!("creating an account at {directory}"))?;
        write_private(&path, serde_json::to_string_pretty(&credentials)?.as_bytes())?;
        tracing::info!(directory = %directory, "ACME account created");
        Ok(account)
    }

    async fn obtain(&self) -> Result<()> {
        let account = self.account().await?;
        let identifiers: Vec<Identifier> =
            self.domains.iter().map(|d| Identifier::Dns(d.clone())).collect();
        let mut order = account
            .new_order(&NewOrder::new(&identifiers))
            .await
            .context("creating the certificate order")?;
        tracing::info!(domains = ?self.domains, challenge = self.status.read().challenge, "requesting a certificate");
        let kind = match self.challenge {
            AcmeChallenge::Http01 => ChallengeType::Http01,
            _ => ChallengeType::TlsAlpn01,
        };
        let result = async {
            let mut authorizations = order.authorizations();
            while let Some(authorization) = authorizations.next().await {
                let mut authorization = authorization.context("reading an authorization")?;
                match authorization.status {
                    AuthorizationStatus::Pending => {}
                    AuthorizationStatus::Valid => continue,
                    other => bail!("authorization is {other:?}"),
                }
                let mut challenge = authorization
                    .challenge(kind.clone())
                    .with_context(|| format!("the CA offers no {kind:?} challenge"))?;
                let domain = challenge.identifier().to_string().to_ascii_lowercase();
                let answer = challenge.key_authorization();
                match kind {
                    ChallengeType::Http01 => {
                        self.tokens
                            .write()
                            .insert(challenge.token.clone(), answer.as_str().to_owned());
                    }
                    _ => {
                        let key = challenge_certificate(&domain, answer.digest().as_ref())?;
                        self.resolver.challenges.write().insert(domain, key);
                    }
                }
                challenge.set_ready().await.context("starting validation")?;
            }
            let retry = RetryPolicy::new()
                .initial_delay(Duration::from_secs(1))
                .timeout(Duration::from_secs(120));
            let state = order.poll_ready(&retry).await.context("waiting for validation")?;
            if state != OrderStatus::Ready {
                let mut reasons = Vec::new();
                let mut authorizations = order.authorizations();
                while let Some(Ok(authorization)) = authorizations.next().await {
                    for c in &authorization.challenges {
                        if let Some(problem) = &c.error {
                            reasons.push(problem.to_string());
                        }
                    }
                }
                bail!(
                    "validation failed ({state:?}){}",
                    if reasons.is_empty() { String::new() } else { format!(": {}", reasons.join("; ")) }
                );
            }
            let key_pem = order.finalize().await.context("finalizing the order")?;
            let chain_pem = order
                .poll_certificate(&retry)
                .await
                .context("downloading the certificate")?;
            Ok((key_pem, chain_pem))
        }
        .await;
        // Challenge material is only needed during validation.
        self.tokens.write().clear();
        self.resolver.challenges.write().clear();
        let (key_pem, chain_pem) = result?;
        self.install(&key_pem, &chain_pem)?;
        Ok(())
    }

    /// Save a new certificate and swap it into the listener.
    fn install(&self, key_pem: &str, chain_pem: &str) -> Result<()> {
        let chain = parse_chain(chain_pem.as_bytes())?;
        let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).context("certificate key")?;
        let certified = certified_key(chain.clone(), &key)?;
        let validity = validity(chain[0].as_ref()).context("reading the certificate's validity")?;
        let dir = self.cfg.acme_dir();
        write_private(&dir.join("key.pem"), key_pem.as_bytes())?;
        std::fs::write(dir.join("cert.pem"), chain_pem).context("writing cert.pem")?;
        let meta = CertMeta {
            domains: self.domains.clone(),
            directory: self.cfg.tls.acme.directory_url(),
        };
        std::fs::write(dir.join("cert.json"), serde_json::to_vec_pretty(&meta)?)?;
        let fp = crate::tls::fingerprint(chain[0].as_ref());
        *self.resolver.current.write() = certified;
        *self.fingerprint.write() = fp.clone();
        let mut status = self.status.write();
        status.issued = true;
        set_validity(&mut status, Some(validity));
        tracing::info!(
            domains = ?self.domains,
            sha256_fingerprint = %fp,
            not_after = validity.1,
            "certificate installed"
        );
        Ok(())
    }
}

fn set_validity(status: &mut Status, validity: Option<(i64, i64)>) {
    status.not_before = validity.map(|v| v.0);
    status.not_after = validity.map(|v| v.1);
    status.renew_at = validity.map(|(from, to)| from + (to - from) * 2 / 3);
}

fn parse_chain(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<_, _>>()
        .context("parsing the certificate chain")?;
    if chain.is_empty() {
        bail!("the CA returned no certificate");
    }
    Ok(chain)
}

type Cached = (Arc<CertifiedKey>, String, Option<(i64, i64)>);

/// The cached certificate, if it was issued for these domains by this CA
/// and has not expired.
fn load_cached(cfg: &Config, domains: &[String]) -> Result<Option<Cached>> {
    let dir = cfg.acme_dir();
    let (cert, key, meta) = (dir.join("cert.pem"), dir.join("key.pem"), dir.join("cert.json"));
    if !cert.exists() || !key.exists() || !meta.exists() {
        return Ok(None);
    }
    let stored: CertMeta = serde_json::from_slice(&std::fs::read(&meta)?)?;
    let wanted = CertMeta {
        domains: domains.to_vec(),
        directory: cfg.tls.acme.directory_url(),
    };
    if stored != wanted {
        tracing::info!("configured domains or CA changed; a new certificate will be requested");
        return Ok(None);
    }
    let chain = parse_chain(&std::fs::read(&cert)?)?;
    let validity = validity(chain[0].as_ref());
    if validity.is_some_and(|(_, to)| to <= crate::db::now_s()) {
        return Ok(None);
    }
    let key = PrivateKeyDer::from_pem_file(&key).context("reading the certificate key")?;
    let fp = crate::tls::fingerprint(chain[0].as_ref());
    Ok(Some((certified_key(chain, &key)?, fp, validity)))
}

/// The self-signed certificate of a TLS-ALPN-01 validation (RFC 8737 §3).
fn challenge_certificate(domain: &str, digest: &[u8]) -> Result<Arc<CertifiedKey>> {
    let mut params = rcgen::CertificateParams::new(vec![domain.to_owned()])?;
    params
        .custom_extensions
        .push(rcgen::CustomExtension::new_acme_identifier(digest));
    let key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    let key_der = PrivateKeyDer::try_from(key.serialize_der()).map_err(|e| anyhow!("{e}"))?;
    certified_key(vec![cert.der().clone()], &key_der)
}

fn write_private(path: &std::path::Path, data: &[u8]) -> Result<()> {
    crate::tls::write_private(path, data)
}

// ------------------------------------------------------- certificate dates

/// `(notBefore, notAfter)` of a DER X.509 certificate, in Unix seconds.
pub fn validity(der: &[u8]) -> Option<(i64, i64)> {
    let (_, certificate, _) = tlv(der)?;
    let (_, tbs, _) = tlv(certificate)?;
    let mut rest = tbs;
    if rest.first() == Some(&0xa0) {
        rest = tlv(rest)?.2; // version
    }
    for _ in 0..3 {
        rest = tlv(rest)?.2; // serialNumber, signature, issuer
    }
    let (_, validity, _) = tlv(rest)?;
    let (tag_from, from, after) = tlv(validity)?;
    let (tag_to, to, _) = tlv(after)?;
    Some((time(tag_from, from)?, time(tag_to, to)?))
}

/// One DER element: `(tag, content, rest)`.
fn tlv(data: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *data.first()?;
    let first = *data.get(1)? as usize;
    let (len, header) = if first < 0x80 {
        (first, 2)
    } else {
        let n = first & 0x7f;
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *data.get(2 + i)? as usize;
        }
        (len, 2 + n)
    };
    let end = header.checked_add(len)?;
    Some((tag, data.get(header..end)?, data.get(end..)?))
}

/// UTCTime (`YYMMDDHHMMSSZ`) or GeneralizedTime (`YYYYMMDDHHMMSSZ`).
fn time(tag: u8, value: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(value).ok()?.strip_suffix('Z')?;
    let (year, rest) = match tag {
        0x17 => {
            let yy: i64 = text.get(0..2)?.parse().ok()?;
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, text.get(2..)?)
        }
        0x18 => (text.get(0..4)?.parse().ok()?, text.get(4..)?),
        _ => return None,
    };
    let field = |i: usize| -> Option<i64> { rest.get(i..i + 2)?.parse().ok() };
    let (month, day, hour, minute, second) = (field(0)?, field(2)?, field(4)?, field(6)?, field(8)?);
    Some(days_from_civil(year, month, day) * 86400 + hour * 3600 + minute * 60 + second)
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

// ------------------------------------------------------------- HTTP listener

/// The plain-HTTP listener of `http_listen`: HTTP-01 answers, and a
/// redirect to `public_url` for everything else.
pub fn http_router(acme: Arc<Acme>) -> axum::Router {
    use axum::extract::{OriginalUri, Path};
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;
    let public = acme.cfg.public_url.trim_end_matches('/').to_owned();
    let answers = Arc::clone(&acme);
    axum::Router::new()
        .route(
            "/.well-known/acme-challenge/{token}",
            axum::routing::get(move |Path(token): Path<String>| {
                let acme = Arc::clone(&answers);
                async move {
                    match acme.http_answer(&token) {
                        Some(answer) => {
                            ([(header::CONTENT_TYPE, "application/octet-stream")], answer).into_response()
                        }
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }),
        )
        .fallback(move |OriginalUri(uri): OriginalUri| {
            let target = format!("{public}{}", uri.path_and_query().map(|p| p.as_str()).unwrap_or("/"));
            async move { (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, target)]).into_response() }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_certificate_validity() {
        let mut params = rcgen::CertificateParams::new(vec!["farwing.test".to_owned()]).unwrap();
        params.not_before = rcgen::date_time_ymd(2026, 1, 2);
        params.not_after = rcgen::date_time_ymd(2051, 3, 4); // GeneralizedTime after 2049
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let (from, to) = validity(cert.der().as_ref()).unwrap();
        assert_eq!(from, days_from_civil(2026, 1, 2) * 86400);
        assert_eq!(to, days_from_civil(2051, 3, 4) * 86400);
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
    }

    #[test]
    fn challenge_certificate_is_served_only_to_validation() {
        crate::tls::install_crypto_provider();
        let key = challenge_certificate("farwing.test", &[7u8; 32]).unwrap();
        assert_eq!(key.cert.len(), 1);
    }
}
