//! Time-based one-time passwords for administrators (RFC 6238: HMAC-SHA1,
//! 30-second steps, 6 digits) and their recovery codes.

use hmac::{Hmac, Mac};
use sha1::Sha1;

const STEP_S: i64 = 30;
const DIGITS: u32 = 6;
const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 base32 without padding.
pub fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut buffer = 0u32;
    let mut bits = 0;
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0;
    for ch in text.chars().filter(|c| !c.is_whitespace() && *c != '=') {
        let value = BASE32.iter().position(|b| *b as char == ch.to_ascii_uppercase())? as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

/// A new 160-bit secret, base32.
pub fn new_secret() -> String {
    let mut bytes = [0u8; 20];
    getrandom::fill(&mut bytes).expect("system random number generator");
    base32_encode(&bytes)
}

fn code_at(secret: &[u8], counter: u64) -> u32 {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("any key length");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[19] & 0x0f) as usize;
    let value = u32::from_be_bytes([
        digest[offset] & 0x7f,
        digest[offset + 1],
        digest[offset + 2],
        digest[offset + 3],
    ]);
    value % 10u32.pow(DIGITS)
}

/// The code for `secret` at Unix time `now_s`.
pub fn code(secret: &str, now_s: i64) -> Option<String> {
    let key = base32_decode(secret)?;
    Some(format!(
        "{:06}",
        code_at(&key, (now_s / STEP_S) as u64)
    ))
}

/// Whether `typed` is the code of the current step or one step either side.
pub fn verify(secret: &str, typed: &str, now_s: i64) -> bool {
    let typed: String = typed.chars().filter(|c| !c.is_whitespace()).collect();
    let Some(key) = base32_decode(secret) else {
        return false;
    };
    if typed.len() != DIGITS as usize || !typed.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let step = now_s / STEP_S;
    (-1..=1).any(|delta| {
        let expected = format!("{:06}", code_at(&key, (step + delta) as u64));
        subtle::ConstantTimeEq::ct_eq(expected.as_bytes(), typed.as_bytes()).into()
    })
}

/// `otpauth://` URI for authenticator apps.
pub fn uri(secret: &str, issuer: &str, account: &str) -> String {
    let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    format!(
        "otpauth://totp/{}:{}?secret={secret}&issuer={}&algorithm=SHA1&digits=6&period=30",
        enc(issuer),
        enc(account),
        enc(issuer)
    )
}

/// Ten recovery codes like `ab12-cd34`.
pub fn new_recovery_codes() -> Vec<String> {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    (0..10)
        .map(|_| {
            let mut bytes = [0u8; 8];
            getrandom::fill(&mut bytes).expect("system random number generator");
            let chars: String = bytes
                .iter()
                .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
                .collect();
            format!("{}-{}", &chars[..4], &chars[4..])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_vectors() {
        // RFC 6238 appendix B (SHA-1, 8 digits) truncated to 6 digits.
        let secret = base32_encode(b"12345678901234567890");
        assert_eq!(code(&secret, 59).unwrap(), "287082");
        assert_eq!(code(&secret, 1111111109).unwrap(), "081804");
        assert_eq!(code(&secret, 1234567890).unwrap(), "005924");
        assert!(verify(&secret, "287082", 59));
        assert!(verify(&secret, "287 082", 89), "one step later");
        assert!(!verify(&secret, "287082", 200));
        assert!(!verify(&secret, "28708", 59));
    }

    #[test]
    fn base32_round_trips() {
        for bytes in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            assert_eq!(base32_decode(&base32_encode(bytes)).unwrap(), bytes);
        }
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
        assert_eq!(new_recovery_codes().len(), 10);
    }
}
