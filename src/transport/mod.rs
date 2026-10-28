//! Transport ownership and module boundaries.

pub(crate) mod bulk;
pub(crate) mod direct;
pub(crate) mod happy_eyeballs;
pub(crate) mod nat;
pub mod video_receiver;
pub(crate) mod rsfec;
pub mod rtc;
pub(crate) mod rtcp_timing;
pub mod signal;
pub(crate) mod timing;
pub(crate) mod transfer;
pub(crate) mod fct;
pub(crate) mod fec_policy;

/// HTTP can start before signaling (for example, the release check).
/// Install the same TLS provider before reqwest constructs its configuration.
pub(crate) fn http_client() -> reqwest::ClientBuilder {
    init_tls();
    reqwest::Client::builder()
}

pub(crate) fn init_tls() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}
