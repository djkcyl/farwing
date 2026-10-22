//! First start and recovery (`docs/protocol/deployment-design.md` §2.2): a
//! one-time code, printed on the server's console, unlocks the setup page
//! while no administrator exists, and the recovery of an administrator
//! account afterwards (`farwing-server setup-code`).

use crate::db::{R, now_s};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

const KEY: &str = "setup_code";
/// A code is valid this long.
pub const CODE_TTL_S: i64 = 24 * 3600;
/// Letters and digits that cannot be mistaken for one another.
const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTVWXYZ";

#[derive(Serialize, Deserialize)]
struct Stored {
    hash: String,
    expires_at: i64,
}

/// Uppercase, without separators.
fn normalize(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// A new code (`XXXX-XXXX-XXXX`), replacing any earlier one.
pub fn new_code(c: &Connection) -> R<String> {
    let bytes: [u8; 12] = crate::crypto::random_array();
    let raw: String = bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect();
    let stored = Stored {
        hash: crate::crypto::token_hash(&raw),
        expires_at: now_s() + CODE_TTL_S,
    };
    crate::webdb::set_setting(
        c,
        KEY,
        &serde_json::to_string(&stored).expect("serializable"),
    )?;
    Ok(format!("{}-{}-{}", &raw[..4], &raw[4..8], &raw[8..]))
}

fn stored(c: &Connection) -> R<Option<Stored>> {
    Ok(crate::webdb::settings(c)?
        .remove(KEY)
        .and_then(|v| serde_json::from_str::<Stored>(&v).ok())
        .filter(|s| s.expires_at > now_s()))
}

/// A code that has not expired exists.
pub fn code_pending(c: &Connection) -> R<bool> {
    Ok(stored(c)?.is_some())
}

pub fn code_valid(c: &Connection, code: &str) -> R<bool> {
    let code = normalize(code);
    Ok(!code.is_empty()
        && stored(c)?.is_some_and(|s| {
            crate::crypto::ct_eq(
                s.hash.as_bytes(),
                crate::crypto::token_hash(&code).as_bytes(),
            )
        }))
}

pub fn clear_code(c: &Connection) -> R<()> {
    crate::webdb::delete_setting(c, KEY)
}

/// No administrator exists yet.
pub fn needs_setup(c: &Connection) -> R<bool> {
    Ok(crate::webdb::admin_count(c)? == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL)",
        )
        .unwrap();
        assert!(!code_pending(&c).unwrap());
        let code = new_code(&c).unwrap();
        assert_eq!(code.len(), 14);
        assert!(code_pending(&c).unwrap());
        assert!(code_valid(&c, &code).unwrap());
        assert!(code_valid(&c, &code.to_ascii_lowercase().replace('-', " ")).unwrap());
        assert!(!code_valid(&c, "AAAA-AAAA-AAAA").unwrap());
        assert!(!code_valid(&c, "").unwrap());
        // A new code replaces the old one.
        let second = new_code(&c).unwrap();
        assert!(!code_valid(&c, &code).unwrap() || code == second);
        clear_code(&c).unwrap();
        assert!(!code_valid(&c, &second).unwrap());
    }
}
