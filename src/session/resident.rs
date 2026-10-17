//! Requests between the desktop client and the resident host owner that an
//! installed service runs. Plain data: the transport is platform-specific.
use crate::{account::auth::NativeIdentity, features::host, session::presence::PresenceState};
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub(crate) enum Request {
    Resume,
    Pause,
    Quiescent,
    DeploymentReady,
    FinishMigration {
        target: String,
    },
    PrepareUpdate,
    CancelUpdate,
    Snapshot {
        ui: bool,
        #[serde(default)]
        device_cursor: Option<crate::account::device_change::relay::Cursor>,
    },
    Assist {
        account: String,
        action: host::assist::Action,
    },
    RefreshPublication,
    RefreshWol,
    WolSetup {
        account: String,
        action: host::wol::setup::Action,
    },
    Initialize {
        force: bool,
    },
    Controllable(bool),
    Name {
        device: String,
        value: String,
    },
    Settings {
        account: String,
        allowed: bool,
        encoding: host::EncodingSettings,
        audio_device: Option<host::audio::Device>,
        audio_defaults: host::audio::DefaultDevices,
        audio_quality: crate::media::audio::encoder::Quality,
        assistance: host::assist::Settings,
        clipboard: host::clipboard::Settings,
        file_transfer: bool,
        port_mapping: bool,
        terminal: bool,
        #[serde(default)]
        terminal_local: bool,
        remote_power: bool,
        wol: bool,
    },
    Disconnect {
        account: String,
        session: String,
    },
    Retry {
        account: String,
    },
    Retire {
        account: String,
    },
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Snapshot {
    #[serde(default)]
    pub device_changes: Option<crate::account::device_change::relay::Batch>,
    pub assistance: host::assist::Snapshot,
    pub publication: crate::account::reporting::Snapshot,
    pub account: String,
    pub online: PresenceState,
    pub allowed: bool,
    pub encoding: host::EncodingSettings,
    #[serde(default)]
    pub audio_device: Option<host::audio::Device>,
    #[serde(default)]
    pub audio_defaults: host::audio::DefaultDevices,
    #[serde(default)]
    pub audio_quality: crate::media::audio::encoder::Quality,
    #[serde(default)]
    pub clipboard: host::clipboard::Settings,
    #[serde(default)]
    pub file_transfer: bool,
    #[serde(default)]
    pub port_mapping: bool,
    #[serde(default)]
    pub terminal: bool,
    #[serde(default)]
    pub terminal_local: bool,
    #[serde(default)]
    pub remote_power: bool,
    #[serde(default)]
    pub wol: bool,
    pub status: host::Status,
    pub capabilities: Option<host::desktop::Capabilities>,
}
#[derive(Serialize, Deserialize)]
pub(crate) enum Reply {
    Done,
    #[cfg(windows)]
    Migration(crate::platform::windows::components::migration::Report),
    Snapshot(Box<Snapshot>),
    Identity(Box<NativeIdentity>),
    Error(String),
    ApiError {
        code: i32,
        message: String,
    },
}
impl Reply {
    pub(crate) fn from_error(error: anyhow::Error) -> Self {
        match error.downcast::<crate::account::api::ApiFailure>() {
            Ok(error) => Self::ApiError {
                code: error.code,
                message: error.message,
            },
            Err(error) => Self::Error(format!("{error:#}")),
        }
    }
    pub fn checked(self) -> Result<Self> {
        match self {
            Self::Error(e) => anyhow::bail!(e),
            Self::ApiError { code, message } => {
                Err(crate::account::api::ApiFailure { code, message }.into())
            }
            other => Ok(other),
        }
    }
}
