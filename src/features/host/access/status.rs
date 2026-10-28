//! What the host reports: its status, connections and streams.
use super::*;

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Status {
    /// The first participant's connection (single-connection views).
    #[serde(default)]
    pub connection: Option<ConnectionInfo>,
    /// Every participant connected to this host.
    #[serde(default)]
    pub connections: Vec<ConnectionInfo>,
    pub ended_connection: Option<EndedConnection>,
    #[serde(default)]
    pub power_message: Option<String>,
    #[serde(default)]
    pub wol: super::super::wol::Status,
    #[serde(default)]
    pub wol_setup: super::super::wol::setup::Status,
    pub ready: bool,
    pub connected: bool,
    pub session_active: bool,
    pub assistance: bool,
    pub message: String,
    pub error: Option<String>,
    pub settings_error: Option<String>,
    pub input_backend: Option<String>,
    pub input_error: Option<String>,
    #[serde(default)]
    pub clipboard: super::super::clipboard::Status,
    #[serde(default)]
    pub files: super::super::files::Status,
    #[serde(default)]
    pub file_transfers: Vec<crate::features::file_transfer::host::notices::Notice>,
    #[serde(default)]
    pub ports: crate::features::port_mapping::host::Status,
    #[serde(default)]
    pub audio: super::super::audio::Status,
    #[serde(default)]
    pub microphone: super::super::microphone::Status,
    pub saving: bool,
    pub encoder: Option<(i32, (u32, u32))>,
    pub capture: Option<String>,
    pub screen: Option<capture::Screen>,
    pub video: Option<ActiveEncoding>,
    pub streams: std::collections::BTreeMap<usize, StreamStatus>,
}

impl Status {
    pub(crate) fn disconnect_target(&self) -> Option<&str> {
        self.connection
            .as_ref()
            .filter(|c| self.session_active && !c.observation_lost && !c.session_id.is_empty())
            .map(|c| c.session_id.as_str())
    }
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ConnectionInfo {
    #[serde(default)]
    pub session_id: String,
    pub device_id: String,
    pub client_id: String,
    pub elapsed_seconds: Option<u64>,
    pub observation_lost: bool,
    /// Remote assistance rather than an account or friend session.
    #[serde(default)]
    pub assistance: bool,
    /// The participant's key within this host (`participants`).
    #[serde(default)]
    pub participant: u64,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub device_name: String,
    /// `own`, `friend`, `assist` or `paired`.
    #[serde(default)]
    pub relation: String,
    /// The host owner withdrew `control` from this participant.
    #[serde(default)]
    pub control_blocked: bool,
    /// The participant's device key (empty when unknown).
    #[serde(default)]
    pub public_key: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct EndedConnection {
    pub id: String,
    pub connection: ConnectionInfo,
    pub assistance: bool,
    pub ended_at: i64,
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct StreamStatus {
    pub encoder: Option<(i32, (u32, u32))>,
    pub capture: Option<String>,
    pub screen: Option<capture::Screen>,
    pub video: Option<ActiveEncoding>,
}

pub(super) fn update_media_summary(status: &mut Status) {
    let current = status
        .streams
        .values()
        .find(|s| s.video.is_some())
        .or_else(|| status.streams.values().next());
    status.encoder = current.and_then(|s| s.encoder);
    status.capture = current.and_then(|s| s.capture.clone());
    status.screen = current.and_then(|s| s.screen.clone());
    status.video = current.and_then(|s| s.video);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ActiveEncoding {
    #[serde(default)]
    pub preferred_gpu: Option<crate::media::selection::GpuId>,
    #[serde(default)]
    pub preferred_codec: Option<farwing_protocol::session::Codec>,
    #[serde(default)]
    pub selection: crate::media::selection::SelectionReason,
    pub backend: super::super::format::Backend,
    pub adapter: u64,
    pub format: super::super::format::Format,
    pub size: (u32, u32),
    pub fps: u32,
    pub target_bps: u32,
}

#[derive(Clone)]
pub(crate) struct AccessRequest {
    pub(super) generation: u64,
}

pub(crate) enum RemoteAction {
    Retry,
    Disconnect(String),
    /// Withdraw or restore `control` for one participant's session.
    BlockControl(String, bool),
}
