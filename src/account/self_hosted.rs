use anyhow::{Context, Result, ensure};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use url::Url;

use super::auth::{KeyringSessionStore, LoginSession, SecretEntry, SessionStore, credential_store_lock};

const SERVICE: &str = "com.openuuyc.self-hosted";
const ORIGIN: &str = "server-origin";
const INSTALLATION_KEY: &str = "installation-key";

#[derive(Clone)]
pub(crate) struct SelfHosted {
    pub origin: Url,
    pub device_id: String,
    pub token: String,
}

pub(crate) fn load_origin() -> Result<Option<Url>> {
    let entry = SecretEntry::new(SERVICE, ORIGIN)?;
    match entry.get_secret() {
        Ok(value) => parse_origin(std::str::from_utf8(&value)?).map(Some),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(error).context("无法读取自建服务器地址"),
    }
}

pub(crate) fn parse_origin(value: &str) -> Result<Url> {
    crate::session::local_host::api_base(value.trim())
        .context("请输入服务器地址，例如 https://remote.example.com")
}

pub(crate) fn save_origin(origin: Option<&Url>) -> Result<()> {
    use keyring::Error::NoEntry;
    let entry = SecretEntry::new(SERVICE, ORIGIN)?;
    match origin {
        Some(origin) => entry.set_secret(origin.as_str().as_bytes())?,
        None => match entry.delete_credential() {
            Ok(()) | Err(NoEntry) => (),
            Err(error) => return Err(error.into()),
        },
    }
    Ok(())
}

impl SelfHosted {
    pub(crate) fn load(origin: Url) -> Result<Self> {
        let _lock = credential_store_lock("self-hosted-key.lock")?;
        let entry = SecretEntry::new(SERVICE, INSTALLATION_KEY)?;
        let secret = match entry.get_secret() {
            Ok(value) => value,
            Err(keyring::Error::NoEntry) => {
                let secret = rand::random::<[u8; 32]>();
                entry.set_secret(&secret)?;
                secret.to_vec()
            }
            Err(error) => return Err(error.into()),
        };
        ensure!(secret.len() == 32, "自建服务凭据格式无效");
        let mut mac = Hmac::<Sha256>::new_from_slice(&secret)?;
        mac.update(origin.as_str().as_bytes());
        let token = format!("{:x}", mac.finalize().into_bytes());
        let device_id = format!("{:x}", Sha256::digest(token.as_bytes()))[..16].to_owned();
        Ok(Self { origin, device_id, token })
    }

    fn session_entry(&self) -> Result<SecretEntry> {
        SecretEntry::new(SERVICE, &format!("account-{}", self.device_id))
            .context("无法打开自建账号凭据存储")
    }

    pub(crate) fn session_store(&self) -> Result<KeyringSessionStore> {
        Ok(KeyringSessionStore::with_entry(self.session_entry()?))
    }

    pub(crate) fn session(&self) -> Result<LoginSession> {
        if let Some(session) = self.session_store()?.load()? {
            return Ok(session);
        }
        self.installation_session()
    }

    pub(crate) fn installation_session(&self) -> Result<LoginSession> {
        LoginSession::new(&self.token, self.host_settings_account(), "自建服务器")
    }

    pub(crate) fn host_settings_account(&self) -> String {
        format!("self-hosted-{:x}", Sha256::digest(self.origin.as_str().as_bytes()))
    }
}

pub(crate) fn transfer_credentials(to_resident: bool) -> Result<()> {
    let scoped = load_origin()?.map(SelfHosted::load).transpose()?;
    for name in [ORIGIN, INSTALLATION_KEY] {
        let entry = SecretEntry::new(SERVICE, name)?;
        if to_resident {
            entry.enroll()?;
        } else {
            entry.restore_portable()?;
        }
    }
    if let Some(scoped) = scoped {
        let session = scoped.session_entry()?;
        if to_resident {
            session.enroll()?;
        } else {
            session.restore_portable()?;
        }
        super::auth::KeyringIdentityStore::for_self_hosted(&scoped.device_id)?
            .transfer(to_resident)?;
    }
    Ok(())
}
