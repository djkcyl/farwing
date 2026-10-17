//! Registration/auth format boundary: discard prior records, never migrate them.
use super::{IDENTITY_ACCOUNT, SERVICE, SESSION_ACCOUNT, SecretEntry, credential_store_lock};
use anyhow::{Context, Result};
use keyring::{Entry, Error};
use std::sync::{LazyLock, Mutex};

pub(super) const ACCOUNT: &str = "registration-generation";
const CURRENT: &[u8] = b"2";
static CHECKED: LazyLock<Mutex<[bool; 2]>> = LazyLock::new(|| Mutex::new([false; 2]));

fn is_current(value: Result<Vec<u8>, Error>) -> Result<bool> {
    match value {
        Ok(bytes) if bytes == CURRENT => Ok(true),
        Err(Error::NoEntry) => Ok(false),
        Ok(_) => anyhow::bail!("本机鉴权存储格式与此版本不符，未覆盖记录"),
        Err(error) => Err(error).context("无法读取本机鉴权存储版本"),
    }
}
fn deleted(value: Result<(), Error>) -> Result<()> {
    match value {
        Ok(()) | Err(Error::NoEntry) => Ok(()),
        Err(error) => Err(error).context("无法清除旧鉴权记录"),
    }
}

pub(super) fn ensure() -> Result<()> {
    use crate::platform::host_service::vault;
    let managed = vault::applies()?;
    let mut checked = CHECKED.lock().unwrap_or_else(|e| e.into_inner());
    if checked[usize::from(managed)] {
        return Ok(());
    }
    let _guard = credential_store_lock("registration-generation.lock")?;
    let marker = SecretEntry::new(SERVICE, ACCOUNT)?;
    if !is_current(marker.get_secret())? {
        for name in [IDENTITY_ACCOUNT, SESSION_ACCOUNT] {
            deleted(SecretEntry::new(SERVICE, name)?.delete_credential())?;
        }
        marker.set_secret(CURRENT)?;
        tracing::info!("local registration and authentication reset for real-device registration");
    }
    if managed && vault::sid(std::process::id())? != "S-1-5-18" {
        // SYSTEM cannot clean the enrolled user's Credential Manager. The
        // user's GUI discards those inactive portable copies once as well.
        let local = Entry::new(SERVICE, ACCOUNT)?;
        if !is_current(local.get_secret())? {
            for name in [IDENTITY_ACCOUNT, SESSION_ACCOUNT] {
                deleted(Entry::new(SERVICE, name)?.delete_credential())?;
            }
            local.set_secret(CURRENT)?;
        }
    }
    checked[usize::from(managed)] = true;
    Ok(())
}
