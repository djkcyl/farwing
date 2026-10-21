//! Persistent account authorization and stable installation identity.
//!
//! Both records are kept in the operating system credential service. There is
//! deliberately no plaintext fallback: credentials use Windows Credential Manager
//! through the `keyring` crate.

use std::fmt;

use anyhow::{Context, Result, bail};
use keyring::{Entry, Error as KeyringError};
mod secret;
pub(crate) use secret::SecretEntry;

pub(crate) fn enroll_resident() -> Result<()> {
    use crate::platform::windows::host_service::vault;
    if vault::applies()? {
        return Ok(());
    }
    ensure_resident_owner()?;
    let identity = KeyringIdentityStore::new()?.load_or_create()?;
    // Signed out, the service hosts without an account (access codes,
    // trusted devices, direct connections); signing in later moves it over.
    let session = KeyringSessionStore::new()?.load()?;
    SecretEntry::new(&service(), IDENTITY_ACCOUNT)?.enroll()?;
    SecretEntry::new(&service(), SESSION_ACCOUNT)?.enroll()?;
    if let Some(session) = &session {
        crate::features::host::enroll_settings(
            session.user_id(),
            &identity.client_identity()?.device_id,
        )?;
    }
    crate::features::host::transfer_guest_settings(&identity.client_identity()?.client_id, true)?;
    crate::transport::direct::managed::hand_over()?;
    crate::features::host::displays::transfer_preferences(true)?;
    // The resident (SYSTEM) needs the server setting too.
    crate::account::server::ServerConfig::enroll()?;
    std::fs::write(vault::root()?.join("enabled"), b"Farwing unattended v1\n")?;
    SecretEntry::new(&service(), SESSION_ACCOUNT)?.clear_portable()?;
    Ok(())
}
fn ensure_resident_owner() -> Result<()> {
    use crate::platform::windows::host_service::vault;
    anyhow::ensure!(
        vault::owner()? == Some(vault::sid(std::process::id())?),
        "{}",
        crate::i18n::t().session.resident_owner_mismatch
    );
    Ok(())
}
pub(crate) fn restore_portable() -> Result<()> {
    use crate::platform::windows::host_service::vault;
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
    SecretEntry::new(&service(), IDENTITY_ACCOUNT)?.restore_portable()?;
    SecretEntry::new(&service(), SESSION_ACCOUNT)?.restore_portable()?;
    crate::features::host::displays::transfer_preferences(false)?;
    crate::account::server::ServerConfig::restore_portable()?;
    crate::transport::direct::managed::take_back();
    Ok(())
}

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::account::api::{ClientIdentity, DeviceInitRequest, DevicePolicy};

mod assist;
pub(crate) use assist::AssistCodeStore;

/// Credential service of the current profile (`farwing[.<profile>].session`).
pub(crate) fn service() -> String {
    crate::product::keyring_service("session")
}
const SESSION_ACCOUNT: &str = "account-session";
const IDENTITY_ACCOUNT: &str = "device-identity";
const SESSION_SCHEMA: u8 = 1;
/// v2: Ed25519 device key instead of a server-issued device token.
const IDENTITY_SCHEMA: u8 = 2;

/// A logged-in account on one Farwing Server.
#[derive(Clone, Serialize, Deserialize)]
pub struct LoginSession {
    schema: u8,
    /// Server key (`ServerConfig::key`) that issued the token.
    server: String,
    token: String,
    user_id: String,
    #[serde(default)]
    username: String,
    nickname: String,
}

impl LoginSession {
    pub fn new(
        server: impl Into<String>,
        token: impl Into<String>,
        user_id: impl Into<String>,
        username: impl Into<String>,
        nickname: impl Into<String>,
    ) -> Result<Self> {
        let session = Self {
            schema: SESSION_SCHEMA,
            server: server.into(),
            token: token.into(),
            user_id: user_id.into(),
            username: username.into(),
            nickname: nickname.into(),
        };
        session.validate_schema()?;
        Ok(session)
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn username(&self) -> &str {
        &self.username
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
        if self.token.is_empty() || self.user_id.is_empty() || self.server.is_empty() {
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

/// This installation's identity and its registration on a Farwing Server.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct NativeIdentity {
    schema: u8,
    /// Installation ID (UUID). Random per profile; never derived from hardware.
    client_id: String,
    /// base64url seed of the installation's Ed25519 device key.
    key: String,
    /// Server key of the registration, empty when unregistered.
    #[serde(default)]
    server: String,
    #[serde(default)]
    device_id: String,
    #[serde(default)]
    name: Option<String>,
    /// What the owner allows, as last published to the server.
    #[serde(default = "default_policy")]
    policy: DevicePolicy,
}

/// Own devices accept control by default (`docs/improvement-proposals.md` §7 #12).
fn default_policy() -> DevicePolicy {
    DevicePolicy {
        control: true,
        ..DevicePolicy::default()
    }
}

impl NativeIdentity {
    pub fn generate() -> Result<Self> {
        let key = farwing_protocol::crypto::DeviceKey::generate();
        Ok(Self {
            schema: IDENTITY_SCHEMA,
            client_id: Uuid::new_v4().to_string(),
            key: farwing_protocol::crypto::b64(&key.seed()),
            server: String::new(),
            device_id: String::new(),
            name: None,
            policy: default_policy(),
        })
    }

    fn key_seed(&self) -> Result<[u8; 32]> {
        farwing_protocol::crypto::unb64(&self.key)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .context("saved device key is invalid")
    }

    pub fn client_identity(&self) -> Result<ClientIdentity> {
        self.validate_schema()?;
        ClientIdentity::new(&self.client_id, &self.device_id, &self.key_seed()?)
    }

    /// Server key this installation is registered with (empty if none).
    pub(crate) fn server(&self) -> &str {
        &self.server
    }

    /// Whether the registration belongs to `server_key`.
    pub(crate) fn registered_for(&self, server_key: &str) -> bool {
        !self.device_id.is_empty() && self.server == server_key
    }

    /// Forget the server registration, keeping the installation ID, the
    /// device key and local settings (a different server, or a registration
    /// the server no longer knows).
    pub(crate) fn clear_registration(&mut self) {
        self.server.clear();
        self.device_id.clear();
    }

    pub(crate) fn policy(&self) -> DevicePolicy {
        self.policy
    }
    pub(crate) fn set_policy(&mut self, value: DevicePolicy) {
        self.policy = value;
    }
    pub(crate) fn same_registration_settings(&self, other: &Self) -> bool {
        self.name == other.name && self.policy == other.policy
    }

    pub(crate) fn set_device_name(&mut self, value: String) {
        self.name = Some(value);
    }

    /// The name the user gave this device, if any.
    pub(crate) fn device_name(&self) -> Option<&str> {
        self.name.as_deref().filter(|name| !name.trim().is_empty())
    }

    pub(crate) fn suggested_name(&self) -> String {
        short_device_name(&self.client_id)
    }

    pub(crate) fn device_init_request(
        &self,
        hardware: &crate::platform::windows::device_profile::Hardware,
    ) -> Result<DeviceInitRequest> {
        self.validate_schema()?;
        Ok(DeviceInitRequest {
            installation_id: self.client_id.clone(),
            name: self.name.clone().unwrap_or_else(|| hardware.name.clone()),
            platform: farwing_protocol::Platform::Windows,
            os: hardware.os.clone(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            capabilities: crate::account::feature_ability::local_capabilities(),
            policy: self.policy,
            hardware: Some(farwing_protocol::api::HardwareInfo {
                cpu: hardware.cpu.clone(),
                base_board: hardware.base_board.clone(),
                video: hardware.video.clone(),
                // The local profile counts MiB; the server takes bytes.
                memory: hardware.memory.saturating_mul(1024 * 1024),
                screen: hardware.screen.clone(),
                mac: hardware.mac.clone(),
            }),
            // Filled and signed by `ServerApi::init_device`.
            public_key: String::new(),
            timestamp: 0,
            proof: String::new(),
        })
    }

    /// Record a successful registration.
    pub fn complete_registration(
        &mut self,
        server_key: &str,
        device_id: impl Into<String>,
    ) -> Result<()> {
        let device_id = device_id.into();
        if !farwing_protocol::ids::is_device_id(&device_id) {
            bail!("device initialization returned an invalid device_id");
        }
        self.device_id = device_id;
        self.server = server_key.to_owned();
        self.validate_schema()
    }

    fn validate_schema(&self) -> Result<()> {
        if self.schema != IDENTITY_SCHEMA {
            bail!("{}", crate::i18n::t().session.registration_record_outdated);
        }
        Uuid::parse_str(&self.client_id).context("saved installation id is not a UUID")?;
        self.key_seed()?;
        if !self.device_id.is_empty() {
            anyhow::ensure!(
                farwing_protocol::ids::is_device_id(&self.device_id),
                "saved device_id has an invalid format"
            );
            anyhow::ensure!(!self.server.is_empty(), "saved device registration is incomplete");
        }
        Ok(())
    }
}

fn short_device_name(identity: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(identity.as_bytes());
    // Stable installation identity; unrelated to account, MAC or build hash.
    format!(
        "FW-{:02X}{:02X}{:02X}{:02X}",
        hash[0], hash[1], hash[2], hash[3]
    )
}

impl fmt::Debug for NativeIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeIdentity")
            .field("schema", &self.schema)
            .field("client_id", &"***REDACTED***")
            .field("registered", &!self.device_id.is_empty())
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
        let entry = SecretEntry::new(&service(), IDENTITY_ACCOUNT)
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
        let entry = SecretEntry::new(&service(), SESSION_ACCOUNT)
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
    let directory = if crate::platform::windows::host_service::vault::applies()? {
        crate::platform::windows::host_service::vault::root()?
    } else {
        crate::product::local_data_dir()
            .context("LOCALAPPDATA is required for session-store coordination")?
            .join("locks")
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
