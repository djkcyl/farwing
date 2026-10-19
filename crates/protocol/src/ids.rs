//! Identifier formats and validation.

/// Prefix of an account session credential.
pub const ACCESS_TOKEN_PREFIX: &str = "fwa_";
/// Prefix of a guest (not logged in) credential.
pub const GUEST_TOKEN_PREFIX: &str = "fwg_";
/// Prefix of a single-use realtime ticket.
pub const TICKET_PREFIX: &str = "fwt_";
/// Prefix of a realtime resume token.
pub const RESUME_TOKEN_PREFIX: &str = "fwr_";

/// Length of a device identifier (derived from the device key,
/// `crypto::local_device_id`).
pub const DEVICE_ID_LEN: usize = 16;
/// Length of an assistance connect identifier.
pub const CONNECT_ID_LEN: usize = 9;

/// 16 characters of lower-case base32 (`[a-z2-7]`).
pub fn is_device_id(value: &str) -> bool {
    value.len() == DEVICE_ID_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
}

/// 9 ASCII digits.
pub fn is_connect_id(value: &str) -> bool {
    value.len() == CONNECT_ID_LEN && value.bytes().all(|b| b.is_ascii_digit())
}

/// A credential with the expected prefix and a 43-character base64url body
/// (32 random bytes without padding).
pub fn is_token(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|body| {
        body.len() == 43
            && body
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

/// Lower-case canonical UUID text (8-4-4-4-12).
pub fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_identifiers() {
        assert!(is_device_id("a2b3c4d5e6f7g2h3"));
        assert!(!is_device_id("A2b3c4d5e6f7g2h3"));
        assert!(!is_device_id("a1b2c3d4e5f6g7h8"), "not base32");
        assert!(is_connect_id("123456789"));
        assert!(!is_connect_id("12345678"));
        assert!(is_token(
            "fwa_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            ACCESS_TOKEN_PREFIX
        ));
        assert!(!is_token("fwa_short", ACCESS_TOKEN_PREFIX));
        assert!(is_uuid("0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0"));
    }
}
