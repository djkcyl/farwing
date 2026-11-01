//! The local cache of chat (`docs/protocol/chat-design.md` §12.1): each
//! channel's latest messages, shown at once when it opens and replaced by
//! the server's answer. Files under the profile, encrypted for this Windows
//! user (DPAPI); removed when the account signs out.
use farwing_protocol::chat::{Id, Message};
use std::path::{Path, PathBuf};

/// Messages kept per channel.
pub(crate) const PER_CHANNEL: usize = 200;

#[derive(Clone)]
pub(crate) struct Cache {
    dir: PathBuf,
}

impl Cache {
    /// The cache of `user` on `server` under `profile_dir`.
    pub(crate) fn new(profile_dir: &Path, server: &str, user: &str) -> Self {
        use sha2::{Digest, Sha256};
        let key: String = Sha256::digest(format!("{server}\n{user}"))[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Self {
            dir: profile_dir.join("cache").join("chat").join(key),
        }
    }

    fn file(&self, channel: Id) -> PathBuf {
        self.dir.join(format!("{channel}.bin"))
    }

    pub(crate) fn load(&self, channel: Id) -> Option<Vec<Message>> {
        let bytes = std::fs::read(self.file(channel)).ok()?;
        let plain = protect(&bytes, false).ok()?;
        serde_json::from_slice(&plain).ok()
    }

    pub(crate) fn store(&self, channel: Id, messages: &[Message]) {
        let start = messages.len().saturating_sub(PER_CHANNEL);
        let Ok(json) = serde_json::to_vec(&messages[start..]) else { return };
        let Ok(sealed) = protect(&json, true) else { return };
        if std::fs::create_dir_all(&self.dir).is_ok() {
            let tmp = self.dir.join(format!("{channel}.tmp"));
            if std::fs::write(&tmp, sealed).is_ok() {
                let _ = std::fs::rename(&tmp, self.file(channel));
            }
        }
    }

    /// Remove every account's chat cache under `profile_dir`.
    pub(crate) fn clear_all(profile_dir: &Path) {
        let _ = std::fs::remove_dir_all(profile_dir.join("cache").join("chat"));
    }
}

#[cfg(windows)]
fn protect(bytes: &[u8], encrypt: bool) -> anyhow::Result<Vec<u8>> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `input` points at `bytes` for the call; `output` is allocated
    // by the system and freed below after copying.
    unsafe {
        if encrypt {
            CryptProtectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output)?;
        } else {
            CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut output)?;
        }
        let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        std::ptr::write_bytes(output.pbData, 0, output.cbData as usize);
        LocalFree(Some(HLOCAL(output.pbData.cast())));
        Ok(result)
    }
}

#[cfg(not(windows))]
fn protect(_: &[u8], _: bool) -> anyhow::Result<Vec<u8>> {
    anyhow::bail!("no local protection on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_survive_a_round_trip_and_are_not_plain() {
        let dir = std::env::temp_dir().join(format!("farwing-chat-cache-{}", std::process::id()));
        let cache = Cache::new(&dir, "https://example", "u");
        let messages: Vec<Message> = (1..=250)
            .map(|i| Message {
                id: Id(i),
                content: format!("secret {i}"),
                ..Default::default()
            })
            .collect();
        cache.store(Id(7), &messages);
        let back = cache.load(Id(7)).unwrap();
        assert_eq!(back.len(), PER_CHANNEL);
        assert_eq!(back[0].id, Id(51));
        let raw = std::fs::read(cache.file(Id(7))).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("secret"));
        Cache::clear_all(&dir);
        assert!(cache.load(Id(7)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
