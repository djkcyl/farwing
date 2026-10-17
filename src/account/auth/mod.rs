//! Persistent account authorization and stable installation identity.
//!
//! Both records are kept in the operating system credential service. There is
//! deliberately no plaintext fallback: credentials use Windows Credential Manager
//! through the `keyring` crate.

use std::fmt;

use anyhow::{Context, Result, bail};
use keyring::{Entry, Error as KeyringError};
mod generation;
mod secret;
pub(crate) use secret::SecretEntry;

pub(crate) fn enroll_resident() -> Result<()> {
    use crate::platform::host_service::vault;
    if vault::applies()? {
        return Ok(());
    }
    ensure_resident_owner()?;
    let identity = KeyringIdentityStore::new()?
        .load_existing()?
        .context("尚未建立本机身份")?;
    let session = KeyringSessionStore::new()?
        .load()?
        .context("请先登录账号")?;
    SecretEntry::new(SERVICE, IDENTITY_ACCOUNT)?.enroll()?;
    SecretEntry::new(SERVICE, SESSION_ACCOUNT)?.enroll()?;
    SecretEntry::new(SERVICE, generation::ACCOUNT)?.enroll()?;
    crate::features::host::enroll_settings(
        session.user_id(),
        &identity.client_identity()?.device_id,
    )?;
    crate::features::host::transfer_guest_settings(&identity.client_identity()?.client_id, true)?;
    crate::features::host::displays::transfer_preferences(true)?;
    std::fs::write(vault::root()?.join("enabled"), b"OpenUUYC unattended v1\n")?;
    SecretEntry::new(SERVICE, SESSION_ACCOUNT)?.clear_portable()?;
    Ok(())
}
fn ensure_resident_owner() -> Result<()> {
    use crate::platform::host_service::vault;
    anyhow::ensure!(
        vault::owner()? == Some(vault::sid(std::process::id())?),
        "服务未登记到当前 Windows 用户"
    );
    Ok(())
}
pub(crate) fn restore_portable() -> Result<()> {
    use crate::platform::host_service::vault;
    if !vault::applies()? {
        return Ok(());
    }
    ensure_resident_owner()?;
    let identity = KeyringIdentityStore::new()?.load_existing()?;
    let session = KeyringSessionStore::new()?.load()?;
    if let Some(identity) = &identity {
        crate::features::host::transfer_guest_settings(
            &identity.client_identity()?.client_id,
            false,
        )?;
    }
    if let (Some(identity), Some(session)) = (identity, session) {
        crate::features::host::restore_portable_settings(
            session.user_id(),
            &identity.client_identity()?.device_id,
        )?;
    }
    SecretEntry::new(SERVICE, IDENTITY_ACCOUNT)?.restore_portable()?;
    SecretEntry::new(SERVICE, SESSION_ACCOUNT)?.restore_portable()?;
    SecretEntry::new(SERVICE, generation::ACCOUNT)?.restore_portable()?;
    crate::features::host::displays::transfer_preferences(false)?;
    Ok(())
}

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::account::api::{ClientIdentity, WindowsDeviceInitRequest};

mod assist;
pub(crate) use assist::AssistCodeStore;

// Stable credential namespace; independent of product and executable names.
const SERVICE: &str = "com.openuuyc.session";
const SESSION_ACCOUNT: &str = "default-nrd-login";
const IDENTITY_ACCOUNT: &str = "native-device-identity";
const SESSION_SCHEMA: u8 = 2;
const IDENTITY_SCHEMA: u8 = 2;

#[derive(Clone, Serialize, Deserialize)]
pub struct LoginSession {
    schema: u8,
    token: String,
    user_id: String,
    nickname: String,
}

impl LoginSession {
    pub fn new(
        token: impl Into<String>,
        user_id: impl Into<String>,
        nickname: impl Into<String>,
    ) -> Result<Self> {
        let session = Self {
            schema: SESSION_SCHEMA,
            token: token.into(),
            user_id: user_id.into(),
            nickname: nickname.into(),
        };
        session.validate_schema()?;
        Ok(session)
    }

    pub(crate) fn generation(&self) -> String {
        use sha2::{Digest, Sha256};
        format!(
            "{:x}",
            Sha256::digest(format!("{}\0{}", self.user_id, self.token))
        )
    }
    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub fn nickname(&self) -> &str {
        &self.nickname
    }

    fn validate_schema(&self) -> Result<()> {
        if self.schema != SESSION_SCHEMA {
            bail!("unsupported saved login-session schema: {}", self.schema);
        }
        if self.token.is_empty() || self.user_id.is_empty() {
            bail!("saved login session is incomplete");
        }
        Ok(())
    }
}

impl fmt::Debug for LoginSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginSession")
            .field("schema", &self.schema)
            .field("token", &"***REDACTED***")
            .field("user_id", &"***REDACTED***")
            .field("nickname", &"***REDACTED***")
            .finish()
    }
}

pub trait SessionStore {
    fn load(&self) -> Result<Option<LoginSession>>;
    fn save(&self, session: &LoginSession) -> Result<()>;
    fn clear(&self) -> Result<()>;
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct NativeIdentity {
    schema: u8,
    client_id: String,
    device_id: String,
    system_id: String,
    machine_guid: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    controllable: bool,
}

impl NativeIdentity {
    pub fn generate() -> Result<Self> {
        let hardware = crate::platform::device_profile::Hardware::read()?;
        Ok(Self {
            schema: IDENTITY_SCHEMA,
            client_id: Uuid::new_v4().to_string(),
            device_id: String::new(),
            system_id: hardware.system_uuid,
            machine_guid: hardware.machine_guid,
            name: None,
            controllable: false,
        })
    }

    pub fn client_identity(&self) -> Result<ClientIdentity> {
        self.validate_schema()?;
        ClientIdentity::new(&self.client_id, &self.device_id, &self.system_id)
    }

    pub(crate) fn set_controllable(&mut self, value: bool) {
        self.controllable = value;
    }
    pub(crate) fn same_registration_settings(&self, other: &Self) -> bool {
        self.name == other.name && self.controllable == other.controllable
    }

    pub(crate) fn set_device_name(&mut self, value: String) {
        self.name = Some(value);
    }

    pub(crate) fn suggested_name(&self) -> String {
        short_device_name(&self.client_id)
    }

    pub(crate) fn device_init_request(
        &self,
        hardware: &crate::platform::device_profile::Hardware,
    ) -> Result<WindowsDeviceInitRequest> {
        self.validate_schema()?;
        anyhow::ensure!(
            self.system_id.eq_ignore_ascii_case(&hardware.system_uuid)
                && self
                    .machine_guid
                    .eq_ignore_ascii_case(&hardware.machine_guid),
            "本机身份与注册记录不一致，需要明确重新注册，未覆盖原身份或登录信息"
        );
        Ok(WindowsDeviceInitRequest {
            name: self.name.clone().unwrap_or_else(|| hardware.name.clone()),
            client_id: self.client_id.clone(),
            system_id: self.system_id.clone(),
            machine_guid: self.machine_guid.clone(),
            os: hardware.os.clone(),
            base_board: hardware.base_board.clone(),
            cpu: hardware.cpu.clone(),
            video: hardware.video.clone(),
            mac: hardware.mac.clone(),
            memory: hardware.memory,
            screen: hardware.screen.clone(),
            platform: 1,
            controllable: self.controllable,
        })
    }

    pub fn complete_registration(&mut self, device_id: impl Into<String>) -> Result<()> {
        let device_id = device_id.into();
        let valid = device_id.len() == 16
            && device_id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
        if !valid {
            bail!("device initialization returned an invalid device_id");
        }
        self.device_id = device_id;
        self.validate_schema()
    }

    fn validate_schema(&self) -> Result<()> {
        if self.schema != IDENTITY_SCHEMA {
            bail!("本机设备注册记录需要重新建立，未修改现有账号登录");
        }
        Uuid::parse_str(&self.client_id).context("saved client_id is not a UUID")?;
        if self.system_id.is_empty() || self.machine_guid.is_empty() {
            bail!("saved native identity has an empty system_id");
        }
        if self.device_id.is_empty() {
            Uuid::parse_str(&self.system_id)
                .context("unregistered device has a non-UUID system_id")?;
        } else {
            let valid_device_id = self.device_id.len() == 16
                && self
                    .device_id
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
            if !valid_device_id {
                bail!("saved device_id has an invalid format");
            }
        }
        Ok(())
    }
}

fn short_device_name(identity: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(identity.as_bytes());
    // Stable installation identity; unrelated to account, MAC or build hash.
    format!(
        "OU-{:02X}{:02X}{:02X}{:02X}",
        hash[0], hash[1], hash[2], hash[3]
    )
}

impl fmt::Debug for NativeIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeIdentity")
            .field("schema", &self.schema)
            .field("client_id", &"***REDACTED***")
            .field("device_id", &"***REDACTED***")
            .field("system_id", &"***REDACTED***")
            .field("name", &"***REDACTED***")
            .finish()
    }
}

pub struct KeyringSessionStore {
    entry: SecretEntry,
}

pub struct KeyringIdentityStore {
    entry: SecretEntry,
}

impl KeyringIdentityStore {
    pub fn new() -> Result<Self> {
        generation::ensure()?;
        let entry = SecretEntry::new(SERVICE, IDENTITY_ACCOUNT)
            .context("native secure credential store is unavailable")?;
        Ok(Self { entry })
    }

    pub fn load_or_create(&self) -> Result<NativeIdentity> {
        let _lock = credential_store_lock("identity.lock")?;
        self.load_or_create_unlocked()
    }

    /// Verification must never create a replacement identity as a side effect.
    pub(crate) fn load_existing(&self) -> Result<Option<NativeIdentity>> {
        let _lock = credential_store_lock("identity.lock")?;
        match self.entry.get_secret() {
            Ok(bytes) => {
                let identity: NativeIdentity =
                    serde_json::from_slice(&bytes).context("saved native identity is invalid")?;
                identity.validate_schema()?;
                Ok(Some(identity))
            }
            Err(KeyringError::NoEntry) => Ok(None),
            Err(error) => Err(error).context("failed to read native identity"),
        }
    }

    fn load_or_create_unlocked(&self) -> Result<NativeIdentity> {
        match self.entry.get_secret() {
            Ok(bytes) => {
                let identity: NativeIdentity = serde_json::from_slice(&bytes)
                    .context("saved native identity has an invalid shape")?;
                identity.validate_schema()?;

                Ok(identity)
            }
            Err(KeyringError::NoEntry) => {
                let identity = NativeIdentity::generate()?;
                self.save_unlocked(&identity)?;
                Ok(identity)
            }
            Err(error) => Err(error).context("failed to read native identity"),
        }
    }

    pub fn save(&self, identity: &NativeIdentity) -> Result<()> {
        let _lock = credential_store_lock("identity.lock")?;
        self.save_unlocked(identity)
    }

    pub(crate) fn replace_if_matches(
        &self,
        expected: &NativeIdentity,
        updated: &NativeIdentity,
    ) -> Result<bool> {
        let _lock = credential_store_lock("identity.lock")?;
        if self.load_or_create_unlocked()? != *expected {
            return Ok(false);
        }
        self.save_unlocked(updated)?;
        Ok(true)
    }

    fn save_unlocked(&self, identity: &NativeIdentity) -> Result<()> {
        identity.validate_schema()?;
        let bytes = serde_json::to_vec(identity).context("failed to serialize native identity")?;
        self.entry
            .set_secret(&bytes)
            .context("failed to save native identity")
    }
}

impl KeyringSessionStore {
    pub fn new() -> Result<Self> {
        generation::ensure()?;
        let entry = SecretEntry::new(SERVICE, SESSION_ACCOUNT)
            .context("native secure credential store is unavailable")?;
        Ok(Self { entry })
    }

    /// An old room/API completion must not erase a newer QR login. The lock
    /// serializes compare/delete with save across GUI and CLI processes; it
    /// contains no credentials and is never held across a network await.
    pub fn clear_if_matches(&self, expected: &LoginSession) -> Result<bool> {
        let _lock = session_store_lock()?;
        if self.load_unlocked()?.is_some_and(|current| {
            current.user_id == expected.user_id && current.token == expected.token
        }) {
            self.clear_unlocked()?;
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) fn save_if_matches(
        &self,
        expected: Option<&LoginSession>,
        next: &LoginSession,
    ) -> Result<bool> {
        let _lock = session_store_lock()?;
        let current = self.load_unlocked()?;
        let matches = match (current.as_ref(), expected) {
            (None, None) => true,
            (Some(current), Some(expected)) => {
                current.user_id == expected.user_id && current.token == expected.token
            }
            _ => false,
        };
        if !matches {
            return Ok(false);
        }
        self.save_session_unlocked(next)?;
        Ok(true)
    }

    fn save_session_unlocked(&self, session: &LoginSession) -> Result<()> {
        session.validate_schema()?;
        let bytes = serde_json::to_vec(session).context("failed to serialize login session")?;
        self.entry
            .set_secret(&bytes)
            .context("failed to save login session in native secure credential store")
    }

    fn load_unlocked(&self) -> Result<Option<LoginSession>> {
        let bytes = match self.entry.get_secret() {
            Ok(bytes) => bytes,
            Err(KeyringError::NoEntry) => return Ok(None),
            Err(error) => {
                return Err(error).context("failed to read native secure credential store");
            }
        };
        let session: LoginSession =
            serde_json::from_slice(&bytes).context("saved login session is not valid JSON")?;
        session.validate_schema()?;
        Ok(Some(session))
    }

    fn clear_unlocked(&self) -> Result<()> {
        match self.entry.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(error) => {
                Err(error).context("failed to clear login session from secure credential store")
            }
        }
    }
}

fn session_store_lock() -> Result<std::fs::File> {
    credential_store_lock("session.lock")
}

fn credential_store_lock(filename: &str) -> Result<std::fs::File> {
    let base = crate::platform::paths::local_app_data()
        .context("用户数据目录不可用，无法协调登录态存储")?;

    let directory = if crate::platform::host_service::vault::applies()? {
        crate::platform::host_service::vault::root()?
    } else {
        base.join("openuuyc")
    };
    std::fs::create_dir_all(&directory).context("create session coordination directory")?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);

    let lock = options
        .open(directory.join(filename))
        .context("open session coordination lock")?;
    lock.lock().context("lock session credential transaction")?;
    Ok(lock)
}

impl SessionStore for KeyringSessionStore {
    fn load(&self) -> Result<Option<LoginSession>> {
        let _lock = session_store_lock()?;
        self.load_unlocked()
    }

    fn save(&self, session: &LoginSession) -> Result<()> {
        let _lock = session_store_lock()?;
        self.save_session_unlocked(session)
    }

    fn clear(&self) -> Result<()> {
        let _lock = session_store_lock()?;
        self.clear_unlocked()
    }
}
