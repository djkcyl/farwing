//! Persistent controlled-device settings, isolated by account and registered device.
use crate::account::auth::SecretEntry as Entry;
use crate::i18n::t;
use anyhow::{Result, bail, ensure};
use keyring::Error;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// What the owner set for this host, the access code aside: one value from
/// the store through the handle, the background host and the device centre.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct HostSettings {
    pub allow_control: bool,
    pub encoding: super::EncodingSettings,
    pub audio_device: Option<super::audio::Device>,
    pub audio_defaults: super::audio::DefaultDevices,
    pub audio_quality: crate::media::audio::encoder::Quality,
    pub clipboard: super::clipboard::Settings,
    pub file_transfer: bool,
    pub port_mapping: bool,
    pub remote_power: bool,
    pub wol: bool,
    /// Concurrent participants this host admits (published as policy).
    pub max_participants: u32,
    /// How this host's picture passes the server's forwarder.
    pub forwarding: super::forwarding::Mode,
    pub camera: CameraSettings,
    /// The desktop's sound leaves out this device's Farwing calls (§9.4).
    pub exclude_calls: bool,
}

/// Remote camera and microphone (`docs/protocol/camera-design.md` §2, §5).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct CameraSettings {
    /// Others may see this device's cameras.
    pub allowed: bool,
    /// Others may hear its microphone.
    pub listen: bool,
    /// The microphone they hear (endpoint ID); `None` follows the default
    /// recording device.
    pub microphone: Option<String>,
    /// The owner's parameters by camera ID; a camera not listed uses the
    /// defaults.
    pub defaults: std::collections::BTreeMap<String, crate::media::camera::Params>,
}

impl Default for HostSettings {
    fn default() -> Self {
        Self {
            allow_control: false,
            encoding: Default::default(),
            audio_device: None,
            audio_defaults: Default::default(),
            audio_quality: Default::default(),
            clipboard: Default::default(),
            file_transfer: true,
            port_mapping: false,
            remote_power: false,
            wol: false,
            max_participants: farwing_protocol::limits::DEFAULT_MAX_PARTICIPANTS,
            forwarding: Default::default(),
            camera: Default::default(),
            exclude_calls: true,
        }
    }
}

impl HostSettings {
    /// Before anything was saved: an account's own devices accept control,
    /// which the owner can turn off; guest hosting stays opt-in.
    pub fn initial(guest: bool) -> Self {
        Self {
            allow_control: !guest,
            ..Default::default()
        }
    }
    /// Checked before saving. The encoder choice is checked separately on
    /// load: an unavailable one stays editable.
    pub fn validate(&self) -> Result<()> {
        self.encoding.validate()?;
        self.audio_quality.validate()?;
        if let Some(device) = &self.audio_device {
            device.validate()?;
        }
        Ok(())
    }
    /// As loaded: values out of range brought back in.
    fn restored(mut self) -> Result<Self> {
        self.audio_quality = self.audio_quality.restore()?;
        if let Some(device) = &self.audio_device {
            device.validate()?;
        }
        self.max_participants = self
            .max_participants
            .clamp(1, farwing_protocol::limits::MAX_PARTICIPANTS);
        Ok(self)
    }
}

/// The owner's settings in a file (`host/<account>/<device>.json` in the
/// profile, or under the machine's vault directory for the background host),
/// the access code in the credential store.
#[derive(Clone)]
pub(super) struct Store {
    code: Arc<Entry>,
    /// The file's name relative to the settings directory.
    name: String,
    guest: bool,
}

/// The access-code record in the credential store.
#[derive(Serialize, Deserialize)]
struct Code {
    schema: u8,
    assistance: super::assist::Settings,
}

/// The settings file, where it applies now (vault for the background host).
fn location(name: &str) -> Result<std::path::PathBuf> {
    use crate::platform::windows::host_service::vault;
    if vault::applies()? {
        let path = vault::root()?.join("settings").join(name);
        crate::platform::windows::components::files::reject_reparse(&path)?;
        Ok(path)
    } else {
        portable(name)
    }
}
fn portable(name: &str) -> Result<std::path::PathBuf> {
    crate::storage::path(std::path::Path::new("host").join(name))
}

impl Store {
    pub fn guest(installation: &str) -> Result<Self> {
        uuid::Uuid::parse_str(installation).map_err(|_| anyhow::anyhow!(t().features.host_installation_invalid))?;
        Ok(Self {
            code: Arc::new(
                Entry::new(&crate::product::keyring_service("host.guest"), installation)
                    .map_err(|_| anyhow::anyhow!(t().features.host_guest_settings_unavailable))?,
            ),
            name: format!("guest/{installation}.json"),
            guest: true,
        })
    }
    pub fn new(account: &str, device: &str) -> Result<Self> {
        ensure!(!account.is_empty(), "{}", t().features.host_settings_no_account);
        crate::account::api::validate_device_id(device)?;
        let account = format!("{:x}", Sha256::digest(account.as_bytes()));
        let service = crate::product::keyring_service(&format!("host.{account}"));
        Ok(Self {
            code: Arc::new(
                Entry::new(&service, device).map_err(|_| anyhow::anyhow!(t().features.host_settings_unavailable))?,
            ),
            name: format!("{account}/{device}.json"),
            guest: false,
        })
    }
    /// The background host takes this user's settings over.
    pub(super) fn enroll(&self) -> Result<()> {
        self.code.enroll()?;
        let vault = crate::platform::windows::host_service::vault::root()?.join("settings").join(&self.name);
        match crate::storage::read_bytes(&portable(&self.name)?)? {
            Some(bytes) => crate::storage::write_bytes(&vault, &bytes),
            None => Ok(()),
        }
    }
    /// The portable host takes the background host's settings back.
    pub(super) fn restore_portable(&self) -> Result<()> {
        self.code.restore_portable()?;
        let vault = crate::platform::windows::host_service::vault::root()?.join("settings").join(&self.name);
        let portable = portable(&self.name)?;
        match crate::storage::read_bytes(&vault)? {
            Some(bytes) => crate::storage::write_bytes(&portable, &bytes),
            None => crate::storage::remove(&portable),
        }
    }
    pub fn load(&self) -> Result<(HostSettings, super::assist::Settings)> {
        let settings = match crate::storage::read::<HostSettings>(&location(&self.name)?) {
            Ok(Some(settings)) => settings.restored()?,
            Ok(None) => HostSettings::initial(self.guest),
            Err(_) => bail!("{}", t().features.host_settings_invalid),
        };
        let assistance = match self.code.get_secret() {
            Ok(bytes) => {
                let code: Code = serde_json::from_slice(&bytes)
                    .map_err(|_| anyhow::anyhow!(t().features.host_settings_invalid))?;
                ensure!(code.schema == 1, "{}", t().features.host_settings_unsupported);
                code.assistance.validate()?;
                code.assistance
            }
            Err(Error::NoEntry) => Default::default(),
            Err(_) => bail!("{}", t().features.host_settings_read_failed),
        };
        Ok((settings, assistance))
    }
    pub fn save(&self, settings: HostSettings, assistance: super::assist::Settings) -> Result<()> {
        settings.validate()?;
        assistance.validate()?;
        crate::storage::write(&location(&self.name)?, &settings)
            .map_err(|_| anyhow::anyhow!(t().features.host_settings_save_failed))?;
        self.code
            .set_secret(&serde_json::to_vec(&Code { schema: 1, assistance })?)
            .map_err(|_| anyhow::anyhow!(t().features.host_settings_save_failed))
    }
}
