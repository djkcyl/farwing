//! One-use media intent handoff for a locally initiated update. This is not
//! authorization or a general preference cache; every new peer is authenticated
//! and negotiated normally before it can use the handoff.
use super::VideoConfig;
use crate::account::auth::SecretEntry;
use crate::features::stream_control::publisher::{CaptureParams, ConnectOptions};
use anyhow::{Result, ensure};
use prost::Message;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const LIFETIME_SECONDS: i64 = 300;
fn service() -> String {
    crate::product::keyring_service("host.update-resume")
}

#[derive(Clone)]
pub(crate) struct Context {
    scope: String,
    controller: String,
    initial: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct Record {
    schema: u8,
    created: i64,
    controller: String,
    initial: Vec<u8>,
    media: MediaIntent,
}

#[derive(Serialize, Deserialize)]
struct MediaIntent {
    fps: u32,
    fps_limit: u32,
    quality: i32,
    auto_quality: i32,
    bitrate: u32,
    chroma: u8,
    hdr: bool,
    cursor_capture: bool,
}
impl MediaIntent {
    fn capture(config: VideoConfig) -> Self {
        Self {
            fps: config.requested_fps,
            fps_limit: config.fps_limit,
            quality: config.quality,
            auto_quality: config.auto_quality,
            bitrate: config.bitrate,
            chroma: config.format.chroma,
            hdr: config.color.is_hdr(),
            cursor_capture: config.cursor_capture,
        }
    }

    fn apply(&self, params: &mut CaptureParams, initial: &CaptureParams) -> Result<()> {
        params.fps = match self.fps {
            30 => 1,
            60 => 2,
            90 => 3,
            144 => 4,
            _ => anyhow::bail!("更新前的帧率档位无效"),
        };
        ensure!((1..=1000).contains(&self.fps_limit), "更新前的帧率限制无效");
        ensure!((1..=6).contains(&self.quality), "更新前的画质无效");
        ensure!((2..=4).contains(&self.auto_quality), "更新前的自动画质无效");
        ensure!(matches!(self.chroma, 1 | 3), "更新前的颜色格式无效");
        // fpsCount belongs to the saved request, not decoder capability. The
        // reconnect builder emits its default 60 alongside fps=0,
        // even when the unchanged viewing window last applied 90/144.
        params.fps_count = if params.fps_count > 0 && params.fps_count != initial.fps_count {
            (self.fps_limit as i32).min(params.fps_count)
        } else {
            self.fps_limit as i32
        };
        // Restore stale bootstrap fields only. A genuinely changed field in
        // the new request is authoritative, independently of the missing FPS.
        if params.quality == initial.quality {
            params.quality = self.quality;
        }
        if params.auto_quality == initial.auto_quality {
            params.auto_quality = self.auto_quality;
        }
        if params.bitrate == initial.bitrate {
            params.bitrate = i32::try_from(self.bitrate)?;
        }
        if params.chroma == initial.chroma {
            params.chroma = i32::from(self.chroma);
        }
        if params.hdr == initial.hdr {
            params.hdr = self.hdr;
        }
        if params.cursor_capture == initial.cursor_capture {
            params.cursor_capture = self.cursor_capture;
        }
        // Never restore physical resolution, DPI, screen selection or input
        // ownership. Fresh display selection and capabilities remain in force.
        Ok(())
    }
}

impl Context {
    pub(crate) fn new(scope: &str, options: &ConnectOptions) -> Option<Self> {
        let params = options.params.as_ref()?;
        if options.kind != 1
            || options.connect_type != 1
            || crate::account::api::validate_device_id(&options.device_id).is_err()
        {
            return None;
        }
        Some(Self {
            scope: scope.to_owned(),
            controller: format!("{:x}", Sha256::digest(options.device_id.as_bytes())),
            initial: params.encode_to_vec(),
        })
    }

    pub(crate) fn save(&self, config: VideoConfig) -> Result<()> {
        // Only settings actually accepted by the live media owner are copied.
        let record = Record {
            schema: 1,
            created: chrono::Utc::now().timestamp(),
            controller: self.controller.clone(),
            initial: self.initial.clone(),
            media: MediaIntent::capture(config),
        };
        SecretEntry::new(&service(), &self.scope)?.set_secret(&serde_json::to_vec(&record)?)?;
        tracing::info!(
            requested_fps = record.media.fps,
            "saved media intent for local update"
        );
        Ok(())
    }

    pub(crate) fn take(&self, options: &ConnectOptions) -> Result<Option<ConnectOptions>> {
        let entry = SecretEntry::new(&service(), &self.scope)?;
        let bytes = match entry.get_secret() {
            Ok(bytes) => bytes,
            Err(keyring::Error::NoEntry) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        // Consume before use, including expired/mismatched records. Another
        // controller or an ordinary later connection must not inherit it.
        entry.delete_credential()?;
        ensure!(bytes.len() <= 16384, "更新会话记录过大");
        let record: Record = serde_json::from_slice(&bytes)?;
        let age = chrono::Utc::now()
            .timestamp()
            .saturating_sub(record.created);
        if record.schema != 1
            || !(0..=LIFETIME_SECONDS).contains(&age)
            || record.controller != self.controller
            || options.params.as_ref().is_none_or(|p| p.fps != 0)
        {
            return Ok(None);
        }
        let mut restored = options.clone();
        let initial = CaptureParams::decode(record.initial.as_slice())?;
        record
            .media
            .apply(restored.params.as_mut().unwrap(), &initial)?;
        tracing::info!(
            requested_fps = record.media.fps,
            fps_limit = record.media.fps_limit,
            "restored media intent for authenticated update reconnect"
        );
        Ok(Some(restored))
    }
}

pub(crate) fn clear(scope: &str) -> Result<()> {
    if scope.is_empty() {
        return Ok(());
    }
    match SecretEntry::new(&service(), scope)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.into()),
    }
}
