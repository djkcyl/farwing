//! Persistent controlled-device settings, isolated by account and registered device.
use crate::account::auth::SecretEntry as Entry;
use anyhow::{Result, bail, ensure};
use keyring::Error;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct Store(Arc<Entry>);
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
    terminal: bool,
    /// Whether a controller with the terminal grant may also see and attach
    /// to the shells opened on this machine (the sessions page, the command
    /// line). Off by default and meaningless while `terminal` is off.
    #[serde(default)]
    terminal_local: bool,
    #[serde(default)]
    remote_power: bool,
    #[serde(default)]
    wol: bool,
}
fn file_transfer_default() -> bool {
    true
}

impl Store {
    pub fn guest(installation: &str) -> Result<Self> {
        uuid::Uuid::parse_str(installation).map_err(|_| anyhow::anyhow!("本机安装身份无效"))?;
        Ok(Self(Arc::new(
            Entry::new("com.openuuyc.host.guest", installation)
                .map_err(|_| anyhow::anyhow!("游客协助设置存储不可用"))?,
        )))
    }
    pub(super) fn enroll(&self) -> Result<()> {
        self.0.enroll()
    }
    pub(super) fn restore_portable(&self) -> Result<()> {
        self.0.restore_portable()
    }
    pub fn new(account: &str, device: &str) -> Result<Self> {
        ensure!(!account.is_empty(), "无法确定被控设置所属账号");
        crate::account::api::validate_device_id(device)?;
        let service = format!("com.openuuyc.host.{:x}", Sha256::digest(account.as_bytes()));
        Ok(Self(Arc::new(
            Entry::new(&service, device).map_err(|_| anyhow::anyhow!("被控设置存储不可用"))?,
        )))
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
        bool,
        bool,
    )> {
        let bytes = match self.0.get_secret() {
            Ok(bytes) => bytes,
            Err(Error::NoEntry) => {
                return Ok((
                    false,
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
                    false,
                    false,
                ));
            }
            Err(_) => bail!("无法读取被控设置"),
        };
        let record: Record =
            serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("已保存的被控设置无效"))?;
        ensure!(record.schema == 1, "不支持的被控设置格式");
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
            record.terminal,
            record.remote_power,
            record.wol,
            record.terminal_local,
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
        terminal: bool,
        remote_power: bool,
        wol: bool,
        terminal_local: bool,
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
                terminal,
                terminal_local,
                remote_power,
                wol,
            })?)
            .map_err(|_| anyhow::anyhow!("无法保存被控设置"))
    }
}
