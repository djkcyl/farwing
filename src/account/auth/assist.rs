//! Account-scoped assistance codes. No plaintext or third-party client imports.
use super::*;
use crate::account::assist::{SavedDevice, normalize_connect_id, validate_connect_code};
use sha2::{Digest, Sha256};

pub(crate) struct AssistCodeStore {
    service: String,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SavedCode {
    pub publisher_id: String,
    pub code: String,
}

impl AssistCodeStore {
    pub(crate) fn new(user_id: &str) -> Result<Self> {
        if user_id.is_empty() {
            bail!("{}", crate::i18n::t().session.code_account_unknown);
        }
        Ok(Self {
            service: crate::product::keyring_service(&format!(
                "assist.{:x}",
                Sha256::digest(user_id.as_bytes())
            )),
        })
    }

    fn entry(&self, id: &str) -> Result<Entry> {
        let id = normalize_connect_id(id)?;
        Entry::new(&self.service, &id).map_err(|_| anyhow::anyhow!("{}", crate::i18n::t().session.credential_store_unavailable))
    }

    fn read(entry: &Entry) -> Result<Option<SavedCode>> {
        let bytes = match entry.get_secret() {
            Ok(bytes) => bytes,
            Err(KeyringError::NoEntry) => return Ok(None),
            Err(_) => bail!("{}", crate::i18n::t().session.saved_code_unreadable),
        };
        let value: SavedCode = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("{}", crate::i18n::t().session.saved_code_invalid))?;
        crate::account::api::validate_device_id(&value.publisher_id)?;
        validate_connect_code(&value.code)?;
        Ok(Some(value))
    }

    fn delete(entry: &Entry) -> Result<()> {
        match entry.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(_) => bail!("{}", crate::i18n::t().session.saved_code_clear_failed),
        }
    }

    pub(crate) fn load(&self, id: &str, publisher_id: Option<&str>) -> Result<Option<SavedCode>> {
        // An empty `publisher_id` (a favourite not yet connected to) finds
        // only a code saved without a device.
        if let Some(publisher_id) = publisher_id.filter(|id| !id.is_empty()) {
            crate::account::api::validate_device_id(publisher_id)?;
        }
        let value = Self::read(&self.entry(id)?)?;
        Ok(value.filter(|v| publisher_id.is_none_or(|id| v.publisher_id == id)))
    }

    /// `publisher_id` is empty for a favourite not yet connected to: the
    /// first successful connection binds the code to its device.
    pub(crate) fn save(&self, id: &str, publisher_id: &str, code: String) -> Result<()> {
        if !publisher_id.is_empty() {
            crate::account::api::validate_device_id(publisher_id)?;
        }
        validate_connect_code(&code)?;
        let _lock = credential_store_lock("assist-codes.lock")?;
        let entry = self.entry(id)?;
        if code.is_empty() {
            return Self::delete(&entry);
        }
        let bytes = serde_json::to_vec(&SavedCode {
            publisher_id: publisher_id.to_owned(),
            code,
        })?;
        entry
            .set_secret(&bytes)
            .map_err(|_| anyhow::anyhow!("{}", crate::i18n::t().session.code_save_failed))
    }

    pub(crate) fn remove_device(&self, device: &SavedDevice) -> Result<()> {
        device.validate()?;
        let _lock = credential_store_lock("assist-codes.lock")?;
        let entry = self.entry(&device.connect_id)?;
        if Self::read(&entry)?.is_some_and(|v| v.publisher_id == device.publisher_device_id) {
            Self::delete(&entry)?;
        }
        Ok(())
    }

    pub(crate) fn reject_code(&self, id: &str, rejected: &str) -> Result<()> {
        let _lock = credential_store_lock("assist-codes.lock")?;
        let entry = self.entry(id)?;
        if Self::read(&entry)?.is_some_and(|v| v.code == rejected) {
            Self::delete(&entry)?;
        }
        Ok(())
    }
}
