//! Binding of a session's DTLS fingerprints to the peers' device identities
//! (`docs/protocol/v2-design.md` §1.4) and, for code-verified assistance, to
//! the PAKE key (§2).
//!
//! Each side adds session-level attributes to the SDP it sends:
//!
//! * `a=x-farwing-identity:<sig>`: Ed25519 signature of the device key over
//!   [`farwing_protocol::crypto::dtls_identity_message`];
//! * `a=x-farwing-pake:<mac>`: HMAC with the PAKE key over the same
//!   fingerprints (assistance with a code only).
//!
//! The receiver checks both against the peer key the server stated, after a
//! trust-on-first-use check of that key.

use anyhow::{Context, Result, bail, ensure};
use farwing_protocol::crypto;

use crate::account::api::ClientIdentity;

const IDENTITY_ATTRIBUTE: &str = "x-farwing-identity";
const PAKE_ATTRIBUTE: &str = "x-farwing-pake";

/// Role names covered by the signatures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Controller,
    Host,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Self::Controller => "controller",
            Self::Host => "host",
        }
    }
}

/// This side of the binding: the device key and an optional PAKE key.
#[derive(Clone)]
pub(crate) struct Local {
    pub identity: ClientIdentity,
    pub pake: Option<[u8; 32]>,
}

impl Local {
    pub(crate) fn new(identity: ClientIdentity) -> Self {
        Self {
            identity,
            pake: None,
        }
    }
}

/// Both sides of one session attempt.
#[derive(Clone)]
pub(crate) struct Binding {
    pub local: Local,
    pub role: Role,
    /// The peer's device ID and key as stated by the server.
    pub peer_device: String,
    pub peer_key: String,
    pub session_id: String,
    pub attempt_id: String,
}

/// A peer's key does not belong to the device ID it claims: the ID is
/// derived from the key, so this is an impersonation attempt.
#[derive(Debug)]
pub(crate) struct IdentityMismatch {
    pub device_id: String,
}

impl std::fmt::Display for IdentityMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "对端出示的密钥与设备 ID {} 不符，可能有人冒充该设备，已拒绝连接",
            crate::account::api::device_id_text(&self.device_id)
        )
    }
}

impl std::error::Error for IdentityMismatch {}

/// Check that `public_key` is the key of `device_id` (the ID is derived
/// from the key, `docs/access-model.md` §2.1).
pub(crate) fn check_key(device_id: &str, public_key: &str) -> Result<()> {
    ensure!(
        crypto::valid_public_key(public_key),
        "对端设备没有有效的身份公钥"
    );
    if crypto::local_device_id(public_key).as_deref() != Some(device_id) {
        return Err(IdentityMismatch {
            device_id: device_id.to_owned(),
        }
        .into());
    }
    Ok(())
}

impl Binding {
    /// Check the peer key the server stated against the peer's device ID.
    pub(crate) fn check_trust(&self) -> Result<()> {
        check_key(&self.peer_device, &self.peer_key)
    }

    /// Add this side's attributes to a local SDP.
    pub(crate) fn sign(&self, sdp: String) -> Result<String> {
        let fingerprints = fingerprints(&sdp);
        ensure!(!fingerprints.is_empty(), "本地 SDP 缺少 DTLS 指纹");
        let role = self.role.as_str();
        let signature = self.local.identity.sign(&crypto::dtls_identity_message(
            &self.session_id,
            &self.attempt_id,
            role,
            &fingerprints,
        ));
        let mut lines = vec![format!("a={IDENTITY_ATTRIBUTE}:{signature}")];
        if let Some(key) = &self.local.pake {
            lines.push(format!(
                "a={PAKE_ATTRIBUTE}:{}",
                crypto::pake_fingerprint_binding(key, &self.session_id, role, &fingerprints)
            ));
        }
        Ok(insert_session_attributes(&sdp, &lines))
    }

    /// Verify the peer's attributes on a remote SDP.
    pub(crate) fn verify(&self, sdp: &str) -> Result<()> {
        let fingerprints = fingerprints(sdp);
        ensure!(!fingerprints.is_empty(), "对端 SDP 缺少 DTLS 指纹");
        let role = match self.role {
            Role::Controller => Role::Host,
            Role::Host => Role::Controller,
        }
        .as_str();
        let signature = attribute(sdp, IDENTITY_ATTRIBUTE).context("对端未签名其 DTLS 指纹")?;
        crypto::verify(
            &self.peer_key,
            &crypto::dtls_identity_message(&self.session_id, &self.attempt_id, role, &fingerprints),
            signature,
        )
        .map_err(|_| anyhow::anyhow!("对端 DTLS 指纹签名无效"))?;
        if let Some(key) = &self.local.pake {
            let binding = attribute(sdp, PAKE_ATTRIBUTE).context("对端未用访问码确认 DTLS 指纹")?;
            if !crypto::pake_fingerprint_matches(key, &self.session_id, role, &fingerprints, binding) {
                bail!("对端 DTLS 指纹与访问码协商结果不符");
            }
        }
        Ok(())
    }
}

/// `a=fingerprint` values of an SDP (session and media level).
fn fingerprints(sdp: &str) -> Vec<String> {
    let mut values: Vec<String> = sdp
        .lines()
        .filter_map(|line| line.trim_end().strip_prefix("a=fingerprint:"))
        .map(|value| value.trim().to_ascii_lowercase())
        .collect();
    values.sort();
    values.dedup();
    values
}

fn attribute<'a>(sdp: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("a={name}:");
    sdp.lines()
        .find_map(|line| line.trim_end().strip_prefix(prefix.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Insert session-level attribute lines before the first media section.
fn insert_session_attributes(sdp: &str, lines: &[String]) -> String {
    let newline = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let block: String = lines.iter().map(|line| format!("{line}{newline}")).collect();
    match sdp.find(&format!("{newline}m=")) {
        Some(index) => {
            let at = index + newline.len();
            format!("{}{block}{}", &sdp[..at], &sdp[at..])
        }
        None => {
            let mut out = sdp.to_owned();
            if !out.ends_with(newline) {
                out.push_str(newline);
            }
            out.push_str(&block);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sdp() -> String {
        "v=0\r\no=- 1 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=fingerprint:sha-256 AB:CD\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=fingerprint:sha-256 AB:CD\r\n".into()
    }

    fn binding(local: Local, role: Role, peer_key: String) -> Binding {
        Binding {
            local,
            role,
            peer_device: "a1b2c3d4e5f6g7h8".into(),
            peer_key,
            session_id: "s".into(),
            attempt_id: "a".into(),
        }
    }

    #[test]
    fn signed_sdp_verifies_and_rejects_other_keys() {
        let controller =
            ClientIdentity::new("c", "a1b2c3d4e5f6g7h8", &crypto::DeviceKey::generate().seed())
                .unwrap();
        let host = ClientIdentity::new("h", "h1b2c3d4e5f6g7h8", &crypto::DeviceKey::generate().seed())
            .unwrap();
        let key = [7u8; 32];
        let sender = binding(
            Local {
                identity: controller.clone(),
                pake: Some(key),
            },
            Role::Controller,
            host.public_key(),
        );
        let signed = sender.sign(sdp()).unwrap();
        assert!(signed.find("a=x-farwing-identity").unwrap() < signed.find("m=").unwrap());
        let receiver = binding(
            Local {
                identity: host.clone(),
                pake: Some(key),
            },
            Role::Host,
            controller.public_key(),
        );
        receiver.verify(&signed).unwrap();
        let tampered = signed.replace("AB:CD", "AB:CE");
        assert!(receiver.verify(&tampered).is_err());
        let wrong_pake = binding(
            Local {
                identity: host.clone(),
                pake: Some([8u8; 32]),
            },
            Role::Host,
            controller.public_key(),
        );
        assert!(wrong_pake.verify(&signed).is_err());
        let wrong_key = binding(Local::new(host), Role::Host, sender.peer_key.clone());
        assert!(wrong_key.verify(&signed).is_err());
    }
}
