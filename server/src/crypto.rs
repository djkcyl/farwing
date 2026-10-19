//! Identifiers, tokens, hashing and credentials.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Fill `buf` from the operating system CSPRNG.
pub fn fill_random(buf: &mut [u8]) {
    getrandom::fill(buf).expect("operating system random number generator failed");
}

pub fn random_array<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    fill_random(&mut buf);
    buf
}

/// `<prefix>` + base64url (no padding) of 32 random bytes (43 characters).
pub fn new_token(prefix: &str) -> String {
    let bytes: [u8; 32] = random_array();
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes))
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Storage form of every credential: hex SHA-256 of the token text.
pub fn token_hash(token: &str) -> String {
    sha256_hex(token.as_bytes())
}

/// Uniform random value in `0..n` without modulo bias.
fn uniform(n: u32) -> u32 {
    let zone = u32::MAX - (u32::MAX % n);
    loop {
        let v = u32::from_le_bytes(random_array());
        if v < zone {
            return v % n;
        }
    }
}

/// 9 digits, first digit non-zero.
pub fn new_connect_id() -> String {
    let mut s = String::with_capacity(9);
    s.push(char::from(b'1' + uniform(9) as u8));
    for _ in 1..farwing_protocol::ids::CONNECT_ID_LEN {
        s.push(char::from(b'0' + uniform(10) as u8));
    }
    s
}

pub fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn argon2() -> Argon2<'static> {
    // Argon2id, m = 19456 KiB, t = 2, p = 1.
    let params = Params::new(19_456, 2, 1, None).expect("valid argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt =
        SaltString::encode_b64(&random_array::<16>()).map_err(|e| anyhow::anyhow!("salt: {e}"))?;
    let hash = argon2()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("argon2: {e}"))?;
    Ok(hash.to_string())
}

pub fn verify_password(password: &str, encoded: &str) -> bool {
    match PasswordHash::new(encoded) {
        Ok(parsed) => argon2()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// A fixed hash used to spend comparable time when the account does not exist.
pub fn dummy_password_hash() -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| hash_password("farwing-dummy-password").expect("hash"))
}

pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// TURN REST credential: `base64(HMAC-SHA1(secret, username))`.
pub fn turn_password(secret: &str, username: &str) -> String {
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(secret.as_bytes()).expect("any key size");
    mac.update(username.as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}

/// Random secret for `init` (base64url, 32 bytes).
pub fn new_shared_secret() -> String {
    URL_SAFE_NO_PAD.encode(random_array::<32>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use farwing_protocol::ids;

    #[test]
    fn identifiers() {
        for _ in 0..100 {
            let c = new_connect_id();
            assert!(ids::is_connect_id(&c));
            assert_ne!(c.as_bytes()[0], b'0');
            assert!(ids::is_token(
                &new_token(ids::ACCESS_TOKEN_PREFIX),
                ids::ACCESS_TOKEN_PREFIX
            ));
        }
        assert!(ids::is_uuid(&new_uuid()));
    }

    #[test]
    fn passwords() {
        let h = hash_password("correct horse").unwrap();
        assert!(h.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"));
        assert!(verify_password("correct horse", &h));
        assert!(!verify_password("wrong", &h));
    }

    #[test]
    fn turn_rest_credential_matches_coturn() {
        // RFC 2202 test case 2 (HMAC-SHA1), base64 encoded.
        assert_eq!(
            turn_password("Jefe", "what do ya want for nothing?"),
            "7/zfauXrL6LSdBbV8YTfnCWafHk="
        );
        assert_eq!(turn_password("secret", "x").len(), 28);
    }
}
