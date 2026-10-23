//! Persistent controlled-device settings, isolated by account and registered device.
use crate::account::auth::SecretEntry as Entry;
use crate::i18n::t;
use anyhow::{Result, bail, ensure};
use keyring::Error;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Clone)]
/// Settings record; the flag marks the guest (not logged in) store.
pub(super) struct Store(Arc<Entry>, bool);
#[derive(Serialize, Deserialize)]
struct Record {
    schema: u8,
    allow_control: bool,
    #[serde(default)]
    encoding: super::EncodingSettings,
    #[serde(default)]
    audio_device: Option<super::audio::Device>,
    #[serde(default)]
    audio_defaults: super::audio::DefaultDevices,
    #[serde(default)]
    audio_quality: crate::media::audio::encoder::Quality,
    #[serde(default)]
    assistance: super::assist::Settings,
    #[serde(default)]
    clipboard: super::clipboard::Settings,
    #[serde(default = "file_transfer_default")]
    file_transfer: bool,
    #[serde(default)]
    port_mapping: bool,
    #[serde(default)]
    remote_power: bool,
    #[serde(default)]
    wol: bool,
    #[serde(default = "max_participants_default")]
    max_participants: u32,
    #[serde(default)]
    forwarding: super::forwarding::Mode,
}
fn file_transfer_default() -> bool {
    true
}
fn max_participants_default() -> u32 {
    farwing_protocol::limits::DEFAULT_MAX_PARTICIPANTS
}

impl Store {
    pub fn guest(installation: &str) -> Result<Self> {
        uuid::Uuid::parse_str(installation).map_err(|_| anyhow::anyhow!(t().features.host_installation_invalid))?;
        Ok(Self(Arc::new(
            Entry::new(&crate::product::keyring_service("host.guest"), installation)
                .map_err(|_| anyhow::anyhow!(t().features.host_guest_settings_unavailable))?,
        ), true))
    }
    pub(super) fn enroll(&self) -> Result<()> {
        self.0.enroll()
    }
    pub(super) fn restore_portable(&self) -> Result<()> {
        self.0.restore_portable()
    }
    pub fn new(account: &str, device: &str) -> Result<Self> {
        ensure!(!account.is_empty(), "{}", t().features.host_settings_no_account);
        crate::account::api::validate_device_id(device)?;
        let service = crate::product::keyring_service(&format!(
            "host.{:x}",
            Sha256::digest(account.as_bytes())
        ));
        Ok(Self(Arc::new(
            Entry::new(&service, device).map_err(|_| anyhow::anyhow!(t().features.host_settings_unavailable))?,
        ), false))
    }
    pub fn load(
        &self,
    ) -> Result<(
        bool,
        super::EncodingSettings,
        Option<super::audio::Device>,
        super::audio::DefaultDevices,
        crate::media::audio::encoder::Quality,
        super::assist::Settings,
        super::clipboard::Settings,
        bool,
        bool,
        bool,
        bool,
        u32,
        super::forwarding::Mode,
    )> {
        let bytes = match self.0.get_secret() {
            Ok(bytes) => bytes,
            Err(Error::NoEntry) => {
                // An account's own devices accept control by default; the
                // owner can turn it off. Guest hosting stays opt-in.
                return Ok((
                    !self.1,
                    Default::default(),
                    None,
                    Default::default(),
                    Default::default(),
                    Default::default(),
                    Default::default(),
                    true,
                    false,
                    false,
                    false,
                    max_participants_default(),
                    Default::default(),
                ));
            }
            Err(_) => bail!("{}", t().features.host_settings_read_failed),
        };
        let record: Record =
            serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!(t().features.host_settings_invalid))?;
        ensure!(record.schema == 1, "{}", t().features.host_settings_unsupported);
        let audio_quality = record.audio_quality.restore()?;
        record.assistance.validate()?;
        if let Some(device) = &record.audio_device {
            device.validate()?;
        }
        Ok((
            record.allow_control,
            record.encoding,
            record.audio_device,
            record.audio_defaults,
            audio_quality,
            record.assistance,
            record.clipboard,
            record.file_transfer,
            record.port_mapping,
            record.remote_power,
            record.wol,
            record
                .max_participants
                .clamp(1, farwing_protocol::limits::MAX_PARTICIPANTS),
            record.forwarding,
        ))
    }
    pub fn save(
        &self,
        allowed: bool,
        encoding: super::EncodingSettings,
        audio_device: Option<super::audio::Device>,
        audio_defaults: super::audio::DefaultDevices,
        audio_quality: crate::media::audio::encoder::Quality,
        assistance: super::assist::Settings,
        clipboard: super::clipboard::Settings,
        file_transfer: bool,
        port_mapping: bool,
        remote_power: bool,
        wol: bool,
        max_participants: u32,
        forwarding: super::forwarding::Mode,
    ) -> Result<()> {
        encoding.validate()?;
        audio_quality.validate()?;
        assistance.validate()?;
        if let Some(device) = &audio_device {
            device.validate()?;
        }
        self.0
            .set_secret(&serde_json::to_vec(&Record {
                schema: 1,
                allow_control: allowed,
                encoding,
                audio_device,
                audio_defaults,
                audio_quality,
                assistance,
                clipboard,
                file_transfer,
                port_mapping,
                remote_power,
                wol,
                max_participants,
                forwarding,
            })?)
            .map_err(|_| anyhow::anyhow!(t().features.host_settings_save_failed))
    }
}
