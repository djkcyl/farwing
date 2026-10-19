//! Device identity keys, request signatures, DTLS identity binding and the
//! assistance PAKE (feature `crypto`).
//!
//! The byte strings that are signed or MACed are defined here, so the client
//! and the server can never disagree about them. See
//! `docs/protocol/v2-design.md` §1–§2.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use subtle::ConstantTimeEq;

/// Errors from decoding or verifying protocol cryptography.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptoError {
    Encoding,
    PublicKey,
    Signature,
    Pake,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Encoding => "invalid base64url encoding",
            Self::PublicKey => "invalid Ed25519 public key",
            Self::Signature => "signature verification failed",
            Self::Pake => "PAKE exchange failed",
        })
    }
}

impl std::error::Error for CryptoError {}

/// base64url without padding, the encoding of every key, signature and PAKE
/// message on the wire.
pub fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn unb64(text: &str) -> Result<Vec<u8>, CryptoError> {
    URL_SAFE_NO_PAD
        .decode(text.trim())
        .map_err(|_| CryptoError::Encoding)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

// ------------------------------------------------------------- device keys

/// An installation's Ed25519 identity key.
pub struct DeviceKey(SigningKey);

impl DeviceKey {
    pub fn generate() -> Self {
        Self(SigningKey::generate(&mut rand_core::OsRng))
    }

    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self(SigningKey::from_bytes(seed))
    }

    /// The 32-byte secret seed, for storage in the platform credential store.
    pub fn seed(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// Public key as base64url.
    pub fn public_key(&self) -> String {
        b64(self.0.verifying_key().as_bytes())
    }

    /// Detached signature as base64url.
    pub fn sign(&self, message: &[u8]) -> String {
        b64(&self.0.sign(message).to_bytes())
    }
}

impl std::fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceKey")
            .field("public_key", &self.public_key())
            .finish_non_exhaustive()
    }
}

/// Whether `public_key` is a well-formed Ed25519 public key.
pub fn valid_public_key(public_key: &str) -> bool {
    parse_public_key(public_key).is_ok()
}

fn parse_public_key(public_key: &str) -> Result<VerifyingKey, CryptoError> {
    let bytes: [u8; 32] = unb64(public_key)?
        .try_into()
        .map_err(|_| CryptoError::PublicKey)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| CryptoError::PublicKey)
}

/// Verify a detached signature made by [`DeviceKey::sign`].
pub fn verify(public_key: &str, message: &[u8], signature: &str) -> Result<(), CryptoError> {
    let key = parse_public_key(public_key)?;
    let bytes: [u8; 64] = unb64(signature)?
        .try_into()
        .map_err(|_| CryptoError::Signature)?;
    key.verify_strict(message, &Signature::from_bytes(&bytes))
        .map_err(|_| CryptoError::Signature)
}

// ----------------------------------------------------------- signed bytes

/// Bytes signed by every device-authenticated HTTPS request (§1.2).
pub fn request_message(method: &str, path_and_query: &str, timestamp_ms: i64, body: &[u8]) -> Vec<u8> {
    format!(
        "FWREQ2\n{}\n{path_and_query}\n{timestamp_ms}\n{}",
        method.to_ascii_uppercase(),
        hex(&Sha256::digest(body))
    )
    .into_bytes()
}

/// Proof of possession sent with `POST /devices/init` (§1.1).
pub fn init_proof_message(public_key: &str, timestamp_ms: i64) -> Vec<u8> {
    format!("FWINIT2\n{public_key}\n{timestamp_ms}").into_bytes()
}

/// Bytes signed into `a=x-farwing-identity` (§1.4). `fingerprints` are the
/// SDP `a=fingerprint` values; order does not matter.
pub fn dtls_identity_message(session_id: &str, attempt_id: &str, role: &str, fingerprints: &[String]) -> Vec<u8> {
    let mut sorted = fingerprints
        .iter()
        .map(|f| f.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();
    sorted.sort();
    format!("FWDTLS2\n{session_id}\n{attempt_id}\n{role}\n{}", sorted.join(",")).into_bytes()
}

// ------------------------------------------------------------ assist PAKE

/// SPAKE2 identity string of remote assistance.
pub const ASSIST_PAKE_IDENTITY: &[u8] = b"farwing-assist-v2";
/// Number of PAKE instances a controller always sends: the host may accept a
/// random code and a custom password, and a controller cannot tell which.
pub const ASSIST_PAKE_SLOTS: usize = 2;

/// One side of a SPAKE2 exchange, consumed by [`PakeState::finish`].
pub struct PakeState(Spake2<Ed25519Group>);

/// Start a symmetric SPAKE2 exchange for an access `code`; returns the
/// state and the base64url message for the peer.
pub fn pake_start(code: &str) -> (PakeState, String) {
    let (state, message) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(code.as_bytes()),
        &Identity::new(ASSIST_PAKE_IDENTITY),
    );
    (PakeState(state), b64(&message))
}

impl PakeState {
    /// Derive the shared key from the peer's message. A wrong code still
    /// yields a key; mismatches are detected with [`key_confirm`].
    pub fn finish(self, peer_message: &str) -> Result<[u8; 32], CryptoError> {
        let message = unb64(peer_message)?;
        let key = self.0.finish(&message).map_err(|_| CryptoError::Pake)?;
        key.try_into().map_err(|_| CryptoError::Pake)
    }
}

type HmacSha256 = Hmac<Sha256>;

fn mac(key: &[u8; 32], message: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

/// Key confirmation for one side: `label` is `"host"` or `"controller"`.
pub fn key_confirm(key: &[u8; 32], label: &str, request_id: &str) -> String {
    b64(&mac(key, format!("FWAK2\n{label}\n{request_id}").as_bytes()))
}

/// Constant-time comparison of a received key confirmation.
pub fn key_confirm_matches(key: &[u8; 32], label: &str, request_id: &str, received: &str) -> bool {
    let expected = mac(key, format!("FWAK2\n{label}\n{request_id}").as_bytes());
    unb64(received).is_ok_and(|bytes| bool::from(bytes.ct_eq(&expected)))
}

// ---------------------------------------------------------- direct (§12)

/// Bytes a peer signs on a direct connection: its role and the TLS exported
/// keying material, so the proof cannot be relayed into another connection.
pub fn direct_proof_message(role: &str, ekm: &[u8]) -> Vec<u8> {
    format!("FWDIRECT2\n{role}\n{}", hex(ekm)).into_bytes()
}

/// Key confirmation of a PAKE on a direct connection (pairing and
/// assistance): `label` is `"host"` or `"controller"`.
pub fn direct_confirm(key: &[u8; 32], label: &str, ekm: &[u8]) -> String {
    b64(&mac(key, format!("FWPAIR2\n{label}\n{}", hex(ekm)).as_bytes()))
}

/// Constant-time check of [`direct_confirm`].
pub fn direct_confirm_matches(key: &[u8; 32], label: &str, ekm: &[u8], received: &str) -> bool {
    let expected = mac(key, format!("FWPAIR2\n{label}\n{}", hex(ekm)).as_bytes());
    unb64(received).is_ok_and(|bytes| bool::from(bytes.ct_eq(&expected)))
}

/// Device ID of a device that has no server registration: the first 16
/// characters of the lower-case base32 SHA-256 of its public key.
pub fn local_device_id(public_key: &str) -> Option<String> {
    let bytes = unb64(public_key).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let digest = Sha256::digest(&bytes);
    let mut out = String::with_capacity(crate::ids::DEVICE_ID_LEN);
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in digest.iter() {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 && out.len() < crate::ids::DEVICE_ID_LEN {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
        if out.len() == crate::ids::DEVICE_ID_LEN {
            break;
        }
    }
    Some(out)
}

/// Short human-checkable fingerprint of a public key: 4 groups of 4 hex
/// digits of its SHA-256.
pub fn key_fingerprint(public_key: &str) -> String {
    let bytes = unb64(public_key).unwrap_or_default();
    let digest = hex(&Sha256::digest(&bytes)[..8]);
    digest
        .as_bytes()
        .chunks(4)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

/// Value of `a=x-farwing-pake` binding the DTLS fingerprints of an assistance
/// session to the PAKE key (§2 step 7).
pub fn pake_fingerprint_binding(key: &[u8; 32], session_id: &str, role: &str, fingerprints: &[String]) -> String {
    let message = dtls_identity_message(session_id, "", role, fingerprints);
    b64(&mac(key, &message))
}

/// Constant-time check of a received `a=x-farwing-pake` value.
pub fn pake_fingerprint_matches(
    key: &[u8; 32],
    session_id: &str,
    role: &str,
    fingerprints: &[String],
    received: &str,
) -> bool {
    let expected = mac(key, &dtls_identity_message(session_id, "", role, fingerprints));
    unb64(received).is_ok_and(|bytes| bool::from(bytes.ct_eq(&expected)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_round_trip() {
        let key = DeviceKey::generate();
        let message = request_message("post", "/api/v2/devices?x=1", 123, b"{}");
        let signature = key.sign(&message);
        assert!(verify(&key.public_key(), &message, &signature).is_ok());
        let other = request_message("post", "/api/v2/devices?x=2", 123, b"{}");
        assert_eq!(verify(&key.public_key(), &other, &signature), Err(CryptoError::Signature));
        let restored = DeviceKey::from_seed(&key.seed());
        assert_eq!(restored.public_key(), key.public_key());
    }

    #[test]
    fn dtls_message_ignores_fingerprint_order() {
        let a = dtls_identity_message("s", "a", "host", &["B".into(), "a".into()]);
        let b = dtls_identity_message("s", "a", "host", &["a".into(), "b".into()]);
        assert_eq!(a, b);
    }

    #[test]
    fn local_device_ids_are_valid_and_stable() {
        let key = DeviceKey::generate();
        let id = local_device_id(&key.public_key()).unwrap();
        assert!(crate::ids::is_device_id(&id), "{id}");
        assert_eq!(Some(id), local_device_id(&key.public_key()));
        assert_eq!(local_device_id("short"), None);
        assert_eq!(key_fingerprint(&key.public_key()).len(), 19);
    }

    #[test]
    fn direct_confirmation_binds_role_and_channel() {
        let (a, ma) = pake_start("12345678");
        let (b, mb) = pake_start("12345678");
        let ka = a.finish(&mb).unwrap();
        let kb = b.finish(&ma).unwrap();
        let ekm = [3u8; 32];
        let confirm = direct_confirm(&ka, "host", &ekm);
        assert!(direct_confirm_matches(&kb, "host", &ekm, &confirm));
        assert!(!direct_confirm_matches(&kb, "controller", &ekm, &confirm));
        assert!(!direct_confirm_matches(&kb, "host", &[4u8; 32], &confirm));
    }

    #[test]
    fn pake_detects_wrong_code() {
        let (controller, message_c) = pake_start("Fw2026test");
        let (host, message_h) = pake_start("Fw2026test");
        let k_c = controller.finish(&message_h).unwrap();
        let k_h = host.finish(&message_c).unwrap();
        assert_eq!(k_c, k_h);
        let confirm = key_confirm(&k_h, "host", "r1");
        assert!(key_confirm_matches(&k_c, "host", "r1", &confirm));

        let (controller, message_c) = pake_start("wrong-code9");
        let (host, message_h) = pake_start("Fw2026test");
        let k_c = controller.finish(&message_h).unwrap();
        let k_h = host.finish(&message_c).unwrap();
        assert!(!key_confirm_matches(&k_c, "host", "r1", &key_confirm(&k_h, "host", "r1")));
    }
}
