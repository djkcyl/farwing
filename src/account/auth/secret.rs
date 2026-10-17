//! Shared account records in portable and explicitly installed modes.
use crate::platform::host_service::vault;
use keyring::{Entry, Error};

pub(crate) struct SecretEntry {
    local: Entry,
    key: String,
}
fn failure(error: anyhow::Error) -> Error {
    Error::PlatformFailure(Box::new(std::io::Error::other(error.to_string())))
}
impl SecretEntry {
    pub fn new(service: &str, account: &str) -> Result<Self, Error> {
        Ok(Self {
            local: Entry::new(service, account)?,
            key: vault::key(service, account),
        })
    }
    pub fn get_secret(&self) -> Result<Vec<u8>, Error> {
        if vault::applies().map_err(failure)? {
            vault::read(&self.key)
                .map_err(failure)?
                .ok_or(Error::NoEntry)
        } else {
            self.local.get_secret()
        }
    }
    pub fn set_secret(&self, bytes: &[u8]) -> Result<(), Error> {
        if vault::applies().map_err(failure)? {
            vault::write(&self.key, Some(bytes)).map_err(failure)
        } else {
            self.local.set_secret(bytes)
        }
    }
    pub fn delete_credential(&self) -> Result<(), Error> {
        if vault::applies().map_err(failure)? {
            vault::write(&self.key, None).map_err(failure)
        } else {
            self.local.delete_credential()
        }
    }
    pub(crate) fn enroll(&self) -> anyhow::Result<()> {
        match self.local.get_secret() {
            Ok(bytes) => vault::write(&self.key, Some(&bytes)),
            Err(Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    pub(crate) fn restore_portable(&self) -> anyhow::Result<()> {
        match vault::read(&self.key)? {
            Some(bytes) => self.local.set_secret(&bytes)?,
            None => match self.local.delete_credential() {
                Ok(()) | Err(Error::NoEntry) => (),
                Err(e) => return Err(e.into()),
            },
        }
        Ok(())
    }
    pub(crate) fn clear_portable(&self) -> anyhow::Result<()> {
        match self.local.delete_credential() {
            Ok(()) | Err(Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
