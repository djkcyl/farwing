//! Official runtime stream-setting protocol.
//!
//! UU uses two different reliable DataChannels for this state machine:
//! protobuf ECHO/feature negotiation is binary data on CONTROL, while both
//! `CaptureSettingRequest` is sent as protobuf bytes with the text PPID on TEXT.
//! Capture requests are complete snapshots,
//! so no request is produced until the active remote screen's physical mode is
//! known from `ScreenSources`.

mod audio_quality;
use crate::diagnostics::performance::PerformanceMonitor;
pub use crate::features::network_control::NetworkControlSnapshot;
pub use crate::features::remote_cursor::{CursorImage, RemoteCursor};
pub use crate::features::remote_input::MouseMode;
use crate::media::{ConnectionMediaProfile, FrameRateChoice, LocalDisplayInfo};
use crate::protocol::capability::DualCapability;
use anyhow::{Result, anyhow, bail};
pub use display_settings::{
    DisplayChangeRequest, DisplayChangeStatus, DisplayResolution, RemoteDisplayInfo,
    RemoteDisplayMode,
};
pub use display_topology::{DisplayTopologyAction, DisplayTopologyStatus, DisplayTopologySupport};
use input::expire_cursor_request;
use prost::Message as _;
use settings::{
    apply_reported_color, capture_setting_request, color_supported, encode_capture_request,
    format_pb_error, format_proposal, frame_rate_choice, normalize_low_quality,
    quality_from_capability, quality_name, restore_confirmed_capture, validate_color,
    viewing_quality_label,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use tokio::sync::{Notify, mpsc};
use wire::{
    PbCaptureSettingResponse, PbControlMessage, PbDpiScale, PbError, PbFeatureFlag, PbPayload,
    PbReportQosStats, PbRequestHeader, PbResponseHeader, PbRpcRequest, PbRpcResponse,
    PbRpcResponsePayload, PbScreen, PbScreenSources, PbSendVideoTrackResponse, PbSimpleAction,
    PbSimpleActionParams, PbWinRect, encode_envelope,
};
pub(crate) use wire::{
    decode_port_mapping, encode_pb_echo_request, encode_port_mapping,
    encode_read_only_feature_flags,
};

pub(crate) mod annotation;
mod display_settings;
mod display_topology;
mod microphone;
mod preferences;
pub(crate) mod publisher;

const VIDEO_QUALITY_FAST: i32 = 1;
const VIDEO_QUALITY_GENERAL: i32 = 2;
const VIDEO_QUALITY_HD: i32 = 3;
const VIDEO_QUALITY_BLURAY: i32 = 4;
const VIDEO_QUALITY_AUTO: i32 = 5;
const VIDEO_QUALITY_CUSTOM: i32 = 6;

const FPS_30: i32 = 1;
const FPS_60: i32 = 2;
const FPS_90: i32 = 3;
const FPS_144: i32 = 4;

const ACTION_TYPE_ECHO_REQUEST: i32 = 0;
const ACTION_TYPE_ECHO_RESPONSE: i32 = 1;
const CHROMA_420: i32 = 1;
const CHROMA_444: i32 = 3;
const RESOLUTION_DEFAULT: i32 = 1;
// Official VideoScreenState::buildCaptureConfig(-2): update existing tracks,
// not a physical monitor. Negative dimensions bypass changeResolution, and
// zero DPI bypasses SetDisplayDpi on the host. Never echo a stale monitor mode.
const EXISTING_SESSION_TRACKS: i32 = -2;
const UNCHANGED_PHYSICAL_DIMENSION: i32 = -1;
const DEFAULT_CUSTOM_BITRATE_MBPS: u32 = 30;
pub const MAX_CUSTOM_BITRATE_MBPS: u32 = 500;
const CAPTURE_RESULT_FPS_ADJUSTED: i32 = -3;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamQuality {
    Auto,
    Original,
    High,
    Clear,
    Custom,
    Fast,
}

impl StreamQuality {
    const fn capability_quality(self) -> i32 {
        match self {
            Self::Auto => 0,
            Self::Fast => 1,
            Self::Clear => 2,
            Self::High => 3,
            Self::Original => 4,
            Self::Custom => 5,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::Original => "原画",
            Self::High => "超清",
            Self::Clear => "高清",
            Self::Custom => "自定义",
            Self::Fast => "低码率",
        }
    }

    const fn protobuf(self) -> i32 {
        match self {
            Self::Auto => VIDEO_QUALITY_AUTO,
            Self::Original => VIDEO_QUALITY_BLURAY,
            Self::High => VIDEO_QUALITY_HD,
            Self::Clear => VIDEO_QUALITY_GENERAL,
            Self::Custom => VIDEO_QUALITY_CUSTOM,
            Self::Fast => VIDEO_QUALITY_FAST,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamControlProtocol {
    Negotiating,
    CaptureSetting,
    Unsupported,
}

impl StreamControlProtocol {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Negotiating => "协商中",
            Self::CaptureSetting => "CaptureSetting",
            Self::Unsupported => "串流协议不受支持",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
// Stored preferences may contain fields removed in a later release. Read the
// supported choices without reviving those features; validate values on load.
pub struct StreamControlSettings {
    #[serde(default)]
    pub hdr: bool,
    #[serde(default)]
    pub true_color: bool,
    pub frame_rate: FrameRateChoice,
    pub quality: StreamQuality,
    pub custom_bitrate_mbps: u32,
}

pub(crate) fn default_auto_quality() -> i32 {
    VIDEO_QUALITY_GENERAL
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SavedStreamControl {
    pub settings: StreamControlSettings,
    pub auto_frame_quality: i32,
}

pub(crate) struct LoadedStreamControl {
    pub settings: Option<StreamControlSettings>,
    pub auto_frame_quality: i32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ViewingPreferenceUpdate {
    Settings(SavedStreamControl),
    AutoQuality(i32),
}

pub fn custom_bitrate_choices(limit: u32) -> Vec<u32> {
    let limit = limit.clamp(1, MAX_CUSTOM_BITRATE_MBPS);
    let mut choices: Vec<_> = (1..=20)
        .chain((25..=100).step_by(5))
        .chain((110..=200).step_by(10))
        .chain([250, 300, 350, 400, 500])
        .filter(|value| *value <= limit)
        .collect();
    if choices.last() != Some(&limit) {
        choices.push(limit);
    }
    choices
}

pub(crate) fn normalize_custom_bitrate(value: u32) -> u32 {
    custom_bitrate_choices(MAX_CUSTOM_BITRATE_MBPS)
        .into_iter()
        .rev()
        .find(|n| *n <= value)
        .unwrap_or(1)
}

/// User-facing state survives a room replacement, unlike PB sequence numbers,
/// screen snapshots, or the newly negotiated codec/transport.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StreamControlPreferences {
    pub(crate) settings: StreamControlSettings,
    pub(crate) custom_bitrate_limit: Option<u32>,
    audio: Option<crate::media::audio::AudioSettings>,
    auto_frame_quality: i32,
    device: Option<crate::features::viewing_settings::DevicePreferences>,
}

impl StreamControlPreferences {
    fn saved(self) -> SavedStreamControl {
        SavedStreamControl {
            settings: self.settings,
            auto_frame_quality: self.auto_frame_quality,
        }
    }
    pub(crate) fn initial_capture_quality(self) -> Result<(u64, u64, u64)> {
        let bitrate = if self.settings.quality == StreamQuality::Custom {
            if !(1..=MAX_CUSTOM_BITRATE_MBPS).contains(&self.settings.custom_bitrate_mbps) {
                bail!("自定义码率超出有效范围");
            }
            u64::from(
                self.settings
                    .custom_bitrate_mbps
                    .min(self.custom_bitrate_limit.unwrap_or(MAX_CUSTOM_BITRATE_MBPS)),
            ) * 1_000_000
        } else {
            0
        };
        Ok((
            u64::try_from(self.settings.quality.protobuf())?,
            u64::try_from(self.auto_frame_quality)?,
            bitrate,
        ))
    }

    pub(crate) fn from_saved(saved: LoadedStreamControl, profile: ConnectionMediaProfile) -> Self {
        let mut settings = saved.settings.unwrap_or(StreamControlSettings {
            true_color: false,
            hdr: false,
            quality: StreamQuality::Auto,
            frame_rate: frame_rate_choice(profile.stream_fps),
            custom_bitrate_mbps: DEFAULT_CUSTOM_BITRATE_MBPS,
        });
        normalize_low_quality(&mut settings);
        settings.custom_bitrate_mbps = normalize_custom_bitrate(settings.custom_bitrate_mbps);
        Self {
            settings,
            device: None,
            custom_bitrate_limit: None,
            audio: None,
            auto_frame_quality: if settings.quality == StreamQuality::Auto {
                saved.auto_frame_quality
            } else {
                VIDEO_QUALITY_GENERAL
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemoteDisplayState {
    pub screen_id: i32,
    pub video_track_index: i32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemoteScreen {
    pub id: i32,
    pub name: String,
    pub primary: bool,
    pub video_track_index: i32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub display: RemoteDisplayInfo,
}

impl RemoteScreen {
    pub(crate) fn label(&self, catalog: &[Self]) -> String {
        let kind = self.display.screen_type;
        let number = catalog
            .iter()
            .filter(|s| s.display.screen_type == kind)
            .position(|s| s.id == self.id)
            .unwrap_or(0)
            + 1;
        match kind {
            2 => "超级屏".into(),
            1 => format!("虚拟屏 {number}"),
            _ => match crate::media::capture::monitor_name(&self.name) {
                Some(name) => format!("显示屏 {number}（{name}）"),
                None => format!("显示屏 {number}"),
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct StreamControlSnapshot {
    pub hdr_supported: bool,
    pub hdr_unavailable: Option<String>,
    pub custom_bitrate_limit: u32,
    pub auto_quality_label: String,
    pub true_color_supported: bool,
    pub true_color_unavailable: Option<String>,
    pub true_color_max_quality: Option<StreamQuality>,
    pub mouse_preference: MouseMode,
    pub mouse_mode: MouseMode,
    pub mouse_pending: bool,
    pub mouse_error: Option<String>,
    pub cursor_pending: bool,
    pub cursor_error: Option<String>,
    pub local_display: LocalDisplayInfo,
    pub settings: StreamControlSettings,
    pub remote_display: Option<RemoteDisplayState>,
    pub screens: Vec<RemoteScreen>,
    pub screens_generation: u64,
    pub topology: DisplayTopologyStatus,
    pub topology_support: DisplayTopologySupport,
    pub display_settings_supported: bool,
    pub dpi_settings_supported: bool,
    pub display_changes: std::collections::BTreeMap<i32, DisplayChangeStatus>,

    pub protocol: StreamControlProtocol,
    pub custom_bitrate_supported: bool,
    pub mouse_modes_supported: bool,
    pub control_channel_open: bool,
    pub text_channel_open: bool,
    pub pb_connected: bool,
    pub ready: bool,
    pub waiting_for: Option<&'static str>,
    pub pending_sequence: Option<i64>,
    pub pending_count: usize,
    pub last_applied_sequence: Option<i64>,
    pub last_error: Option<String>,
    pub last_notice: Option<String>,
    pub remote_notice: Option<&'static str>,
    pub persistence_error: Option<String>,
    pub network: NetworkControlSnapshot,
}

#[derive(Clone)]
pub struct StreamControlHandle {
    audio_mode: tokio::sync::watch::Sender<bool>,
    audio_quality: Arc<Mutex<audio_quality::State>>,
    microphone: crate::media::microphone::Microphone,
    clipboard: crate::features::clipboard::Clipboard,
    drag_drop: crate::features::drag_drop::controller::Controller,
    files: Arc<crate::features::file_transfer::Transport>,
    diagnostics: crate::diagnostics::remote::Controller,
    mouse: crate::features::remote_input::RemoteInput,
    cursor: crate::features::remote_cursor::RemoteCursorState,
    audio: crate::media::audio::AudioPlayback,
    network: crate::features::network_control::NetworkControl,
    shared: Arc<Mutex<StreamControlState>>,
    outgoing: mpsc::UnboundedSender<OutgoingControlMessage>,
    echo_responses: mpsc::UnboundedSender<Vec<u8>>,
    protocol_changed: Arc<Notify>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PbMessageSource {
    Control,
    Text,
    Signal,
}

#[derive(Clone, Copy)]
pub(crate) struct PbHandshakeStatus {
    pub generation: u64,
    pub open: bool,
    pub connected: bool,
}

pub(crate) struct OutgoingControlMessage {
    pub annotation_generation: Option<u64>,
    pub sequence: i64,
    pub payload: Vec<u8>,
    pub protocol: StreamControlProtocol,
    pub completion: Option<tokio::sync::oneshot::Sender<std::result::Result<(), String>>>,
}

#[derive(Clone, Copy)]
struct CaptureSettingBaseline {
    requested_fps: u32,
    fps_count: u32,
    frame_quality: i32,
    auto_frame_quality: i32,
    cursor_capture: bool,
    chroma_format: i32,
    max_custom_bitrate: u32,
    enable_hdr: bool,
    codec_type: i32,
    max_scale_width: u32,
    max_scale_height: u32,
}

#[derive(Clone)]
struct ScreenBaseline {
    id: i32,
    name: String,
    primary: bool,
    video_track_index: i32,
    fps: u32,
    width: u32,
    height: u32,
    pixel_width: u32,
    pixel_height: u32,
    dpi_scale: u32,
    resolution_type: i32,
    display: RemoteDisplayInfo,
}

struct PendingCapturePreferences {
    sequence: i64,
    preferences: StreamControlPreferences,
    cursor_capture: bool,
    persist: bool,
}

struct StreamControlState {
    peer_clipboard: i32,
    clipboard_files_allowed: bool,
    /// Last reported value of the clipboard gate, so the diagnostic only fires
    /// when it changes.
    clipboard_ready_reported: bool,
    remote_upgrade: Option<crate::features::remote_upgrade::RemoteUpgrade>,
    annotation: annotation::Annotation,
    annotation_extension: Option<u64>,
    custom_bitrate_limit: u32,
    features: Option<crate::account::feature_ability::FeaturePolicy>,
    remote_notice: Option<(Instant, &'static str)>,
    device_preferences: crate::features::viewing_settings::DevicePreferences,
    device_preferences_loaded: bool,
    device_preference_updates:
        tokio::sync::watch::Sender<Option<crate::features::viewing_settings::DevicePreferences>>,
    device_persistence_error: Option<String>,
    restore_input_pending: bool,
    #[allow(
        dead_code,
        reason = "Smart mouse waits for special_game_mouse instead of inferring from a hidden cursor."
    )]
    remote_cursor: crate::features::remote_cursor::RemoteCursorState,
    peer_mouse_relative: Option<bool>,
    cursor_sync_needed: bool,
    cursor_desired_capture: bool,
    mouse_restore_point: Option<(i32, [f64; 2])>,
    mouse: crate::features::remote_input::RemoteInput,
    mouse_transport_connected: bool,
    cursor_pending: Option<(i64, bool, Instant)>,
    cursor_error: Option<String>,
    available_video_tracks: Vec<i32>,
    registered_video_tracks: Vec<i32>,
    track_registration: Option<(i64, Vec<i32>)>,
    track_registration_error: Option<String>,
    local_display: LocalDisplayInfo,
    active_video_track_index: i32,
    current_screen_id: i32,
    screens: Vec<ScreenBaseline>,
    screens_generation: u64,
    display_changes: display_settings::DisplayChanges,
    topology: display_topology::DisplayTopology,
    assistance: bool,

    settings: StreamControlSettings,
    baseline: CaptureSettingBaseline,
    capability: Option<DualCapability>,
    remote_display: Option<RemoteDisplayState>,
    control_channel_open: bool,
    text_channel_open: bool,
    protocol_generation: u64,
    pb_connected: bool,
    peer_capture_setting: u32,
    initial_capture_sync_sent: bool,
    viewing_enabled: bool,
    next_sequence: i64,
    pending_sequences: VecDeque<i64>,
    last_applied_sequence: Option<i64>,
    latest_requested_sequence: Option<i64>,
    last_error: Option<String>,
    last_notice: Option<String>,
    preference_updates: tokio::sync::watch::Sender<Option<ViewingPreferenceUpdate>>,
    confirmed_preferences: StreamControlPreferences,
    pending_capture_preferences: VecDeque<PendingCapturePreferences>,
    user_settings_requested: bool,
    persistence_error: Option<String>,
    audio_persistence_error: Option<String>,
    performance: PerformanceMonitor,
}

impl StreamControlHandle {
    pub(crate) fn new(
        profile: ConnectionMediaProfile,
        performance: PerformanceMonitor,
    ) -> (
        Self,
        mpsc::UnboundedReceiver<OutgoingControlMessage>,
        mpsc::UnboundedReceiver<Vec<u8>>,
    ) {
        let (outgoing, receiver) = mpsc::unbounded_channel();
        let (echo_responses, echo_receiver) = mpsc::unbounded_channel();
        let frame_rate = frame_rate_choice(profile.stream_fps);
        let fps_count = profile
            .local_display
            .refresh_hz
            .clamp(1, profile.stream_fps.max(1));
        let mouse = crate::features::remote_input::RemoteInput::default();
        let cursor = crate::features::remote_cursor::RemoteCursorState::default();
        let state = StreamControlState {
            peer_clipboard: 0,
            clipboard_files_allowed: true,
            clipboard_ready_reported: false,
            remote_upgrade: None,
            annotation: Default::default(),
            annotation_extension: None,
            custom_bitrate_limit: MAX_CUSTOM_BITRATE_MBPS,
            features: None,
            device_preferences: Default::default(),
            device_preferences_loaded: false,
            device_preference_updates: tokio::sync::watch::channel(None).0,
            device_persistence_error: None,
            restore_input_pending: false,
            remote_notice: None,
            remote_cursor: cursor.clone(),
            peer_mouse_relative: None,
            cursor_sync_needed: false,
            cursor_desired_capture: true,
            mouse_restore_point: None,
            mouse: mouse.clone(),
            mouse_transport_connected: false,
            cursor_pending: None,
            cursor_error: None,
            available_video_tracks: Vec::new(),
            registered_video_tracks: Vec::new(),
            track_registration: None,
            track_registration_error: None,
            local_display: profile.local_display,
            active_video_track_index: 0,
            current_screen_id: 0,
            screens: Vec::new(),
            screens_generation: 0,
            display_changes: Default::default(),
            topology: Default::default(),
            assistance: false,

            settings: StreamControlSettings {
                true_color: false,
                hdr: false,
                frame_rate,
                quality: StreamQuality::Auto,
                custom_bitrate_mbps: DEFAULT_CUSTOM_BITRATE_MBPS,
            },
            baseline: CaptureSettingBaseline {
                requested_fps: profile.stream_fps,
                fps_count,
                frame_quality: VIDEO_QUALITY_AUTO,
                auto_frame_quality: VIDEO_QUALITY_GENERAL,
                // Independent CursorShape coordinates are only sampled at
                // shape changes. Watching needs capture-side cursor motion.
                cursor_capture: true,
                chroma_format: CHROMA_420,
                max_custom_bitrate: 0,
                enable_hdr: false,
                codec_type: 0,
                // UU VideoScreenState ctor B9EDA0. B743A0/BA41F0 replace
                // these when negotiation data is available; without it the
                // original retains these defaults. Not hardware capability
                // or a user-selected output-resolution ceiling.
                max_scale_width: 1920,
                max_scale_height: 1080,
            },
            capability: None,
            remote_display: None,
            control_channel_open: false,
            text_channel_open: false,
            protocol_generation: 0,
            pb_connected: false,
            peer_capture_setting: 0,
            initial_capture_sync_sent: false,
            viewing_enabled: true,
            next_sequence: 1,
            pending_sequences: VecDeque::new(),
            last_applied_sequence: None,
            latest_requested_sequence: None,
            last_error: None,
            last_notice: None,
            preference_updates: tokio::sync::watch::channel(None).0,
            confirmed_preferences: StreamControlPreferences {
                device: None,
                custom_bitrate_limit: None,
                settings: StreamControlSettings {
                    true_color: false,
                    hdr: false,
                    frame_rate,
                    quality: StreamQuality::Auto,
                    custom_bitrate_mbps: DEFAULT_CUSTOM_BITRATE_MBPS,
                },
                audio: None,
                auto_frame_quality: VIDEO_QUALITY_GENERAL,
            },
            pending_capture_preferences: VecDeque::new(),
            user_settings_requested: false,
            persistence_error: None,
            audio_persistence_error: None,
            performance,
        };
        state.performance.set_quality(viewing_quality_label(&state));
        let audio = crate::media::audio::AudioPlayback::new();
        audio.set_settings(crate::media::audio::AudioSettings {
            volume: 100,
            muted: profile.muted,
        });
        (
            Self {
                audio_mode: tokio::sync::watch::channel(profile.audio_only).0,
                audio_quality: Arc::new(Mutex::new(audio_quality::State::new(profile.audio_only))),
                microphone: crate::media::microphone::Microphone::new(),
                clipboard: crate::features::clipboard::Clipboard::new(),
                drag_drop: Default::default(),
                files: Arc::new(crate::features::file_transfer::Transport::default()),
                diagnostics: Default::default(),
                mouse,
                cursor,
                audio,
                network: crate::features::network_control::NetworkControl::new(),
                shared: Arc::new(Mutex::new(state)),
                outgoing,
                echo_responses,
                protocol_changed: Arc::new(Notify::new()),
            },
            receiver,
            echo_receiver,
        )
    }

    /// Last independent shape notification for a future local-input presenter.
    /// Its position is sampled at shape change, not continuous motion tracking.
    /// Watching uses capture-side composition instead of a stale local overlay.
    pub(crate) fn mouse(&self) -> &crate::features::remote_input::RemoteInput {
        &self.mouse
    }

    pub(crate) fn clipboard(&self) -> &crate::features::clipboard::Clipboard {
        &self.clipboard
    }
    // Dropping files onto the remote screen is offered by the Windows viewer only.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn official_file_drop_available(&self) -> bool {
        feature_supported(
            &lock(&self.shared),
            crate::account::feature_ability::Feature::FileDrop,
        ) && self.clipboard.drop_available()
    }
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn drop_files(
        &self,
        paths: Vec<std::path::PathBuf>,
        point: crate::protocol::drag_drop::Point,
    ) -> Result<Arc<crate::features::clipboard::drag::Submission>> {
        anyhow::ensure!(
            self.official_file_drop_available(),
            "对端尚未开放文件拖放，或文件剪贴板未开启"
        );
        self.clipboard.drop_files(paths, point)
    }
    pub(crate) fn drag_drop(&self) -> &crate::features::drag_drop::controller::Controller {
        &self.drag_drop
    }

    pub(crate) fn file_transfer(&self) -> &Arc<crate::features::file_transfer::Transport> {
        &self.files
    }

    pub(crate) fn diagnostics(&self) -> &crate::diagnostics::remote::Controller {
        &self.diagnostics
    }

    pub fn snapshot(&self) -> StreamControlSnapshot {
        let mut state = lock(&self.shared);
        expire_cursor_request(&mut state);
        let active_protocol = protocol(&state);
        let mut network = self.network.snapshot();
        if !feature_supported(
            &state,
            crate::account::feature_ability::Feature::ManualTransfer,
        ) {
            network.available = false;
            network.unavailable_reason = Some("官方当前能力配置未开放手动中转");
        }
        let waiting_for = if !state.control_channel_open {
            Some("CONTROL 通道")
        } else if !state.text_channel_open {
            Some("TEXT 通道")
        } else if !state.pb_connected {
            Some("PB 特性协商")
        } else if state.remote_display.is_none() {
            Some("活动屏幕基线")
        } else if state.baseline.codec_type == 0 {
            Some("视频编码协商")
        } else {
            None
        };
        StreamControlSnapshot {
            hdr_supported: state.peer_capture_setting >= 6 && state.capability.is_some(),
            hdr_unavailable: format_proposal(&state, None, Some(true))
                .err()
                .map(|e| e.to_string()),
            custom_bitrate_limit: state.custom_bitrate_limit,
            auto_quality_label: format!(
                "自动（{}）",
                quality_name(state.baseline.auto_frame_quality)
            ),
            true_color_supported: color_supported(&state),
            true_color_unavailable: validate_color(&state, true)
                .err()
                .map(|error| error.to_string()),
            true_color_max_quality: state
                .capability
                .as_ref()
                .map(|cap| cap.select(3, state.settings.hdr, 0))
                .filter(|cap| cap.result == 0)
                .and_then(|cap| quality_from_capability(cap.max_frame_quality)),
            mouse_preference: state.device_preferences.mouse_mode,
            mouse_mode: state.mouse.mode(),
            mouse_pending: state.mouse.waiting_for_neutral() || state.mouse.transport_recovering(),
            mouse_error: state.mouse.error(),
            cursor_pending: state.cursor_pending.is_some(),
            cursor_error: state.cursor_error.clone(),
            local_display: state.local_display,
            settings: state.settings,
            remote_display: state.remote_display,
            screens_generation: state.screens_generation,
            display_settings_supported: display_settings::supported(&state),
            dpi_settings_supported: display_settings::supported(&state)
                && state.peer_capture_setting >= 5,
            display_changes: state.display_changes.status.clone(),
            topology: state.topology.status.clone(),
            topology_support: display_topology::support(&state),

            screens: state
                .screens
                .iter()
                .map(|screen| RemoteScreen {
                    id: screen.id,
                    name: screen.name.clone(),
                    primary: screen.primary,
                    video_track_index: screen.video_track_index,
                    width: screen.width,
                    height: screen.height,
                    refresh_hz: screen.fps,
                    display: screen.display.clone(),
                })
                .collect(),
            protocol: active_protocol,
            custom_bitrate_supported: custom_bitrate_supported(&state)
                && feature_supported(
                    &state,
                    crate::account::feature_ability::Feature::CustomBitrate,
                ),
            mouse_modes_supported: feature_supported(
                &state,
                crate::account::feature_ability::Feature::SmartMouse,
            ),
            control_channel_open: state.control_channel_open,
            text_channel_open: state.text_channel_open,
            pb_connected: state.pb_connected,
            ready: waiting_for.is_none()
                && active_protocol == StreamControlProtocol::CaptureSetting,
            waiting_for,
            pending_sequence: state.pending_sequences.back().copied(),
            pending_count: state.pending_sequences.len(),
            last_applied_sequence: state.last_applied_sequence,
            last_error: state.last_error.clone(),
            last_notice: state.last_notice.clone(),
            remote_notice: state
                .remote_notice
                .filter(|(at, _)| at.elapsed() < std::time::Duration::from_secs(3))
                .map(|(_, notice)| notice),
            persistence_error: state
                .persistence_error
                .clone()
                .or_else(|| state.audio_persistence_error.clone())
                .or_else(|| state.device_persistence_error.clone()),
            network,
        }
    }

    pub(crate) fn audio(&self) -> crate::media::audio::AudioPlayback {
        self.audio.clone()
    }
    pub(crate) fn request_audio_only(&self, enabled: bool) {
        if enabled {
            self.default_audio_quality();
        }
        self.audio_mode.send_replace(enabled);
    }
    pub(crate) fn audio_mode_requests(&self) -> tokio::sync::watch::Receiver<bool> {
        self.audio_mode.subscribe()
    }

    pub(crate) fn preferences(&self) -> StreamControlPreferences {
        let state = lock(&self.shared);
        StreamControlPreferences {
            device: Some(state.device_preferences),
            settings: state.confirmed_preferences.settings,
            custom_bitrate_limit: (state.custom_bitrate_limit < MAX_CUSTOM_BITRATE_MBPS)
                .then_some(state.custom_bitrate_limit),
            audio: Some(self.audio.settings()),
            auto_frame_quality: state.confirmed_preferences.auto_frame_quality,
        }
    }
    pub(crate) fn network_control(&self) -> crate::features::network_control::NetworkControl {
        self.network.clone()
    }

    pub fn set_relay_enabled(&self, enabled: bool) -> Result<()> {
        if !feature_supported(
            &lock(&self.shared),
            crate::account::feature_ability::Feature::ManualTransfer,
        ) {
            bail!("官方当前能力配置未开放手动中转");
        }
        self.network.request(enabled)
    }

    pub(crate) fn set_feature_policy(
        &self,
        policy: crate::account::feature_ability::FeaturePolicy,
    ) {
        self.clipboard
            .platform(if policy.is_windows() { 1 } else { 4 });
        lock(&self.shared).features = Some(policy);
    }

    pub(crate) fn set_remote_upgrade(
        &self,
        upgrade: crate::features::remote_upgrade::RemoteUpgrade,
    ) {
        lock(&self.shared).remote_upgrade = Some(upgrade);
    }

    pub(crate) fn remote_upgrade(&self) -> Option<crate::features::remote_upgrade::RemoteUpgrade> {
        lock(&self.shared).remote_upgrade.clone()
    }

    pub(crate) fn preference_updates(
        &self,
    ) -> tokio::sync::watch::Receiver<Option<ViewingPreferenceUpdate>> {
        lock(&self.shared).preference_updates.subscribe()
    }

    pub(crate) fn set_persistence_error(&self, error: Option<String>) {
        let changed = {
            let mut state = lock(&self.shared);
            let changed = state.persistence_error != error;
            state.persistence_error = error.clone();
            changed
        };
        if changed && let Some(error) = error {
            tracing::warn!(%error, "viewing settings persistence failed");
        }
    }

    pub(crate) fn set_audio_persistence_error(&self, error: Option<String>) {
        lock(&self.shared).audio_persistence_error = error;
    }

    fn send_locked(
        &self,
        state: &mut StreamControlState,
        outgoing: OutgoingControlMessage,
        target: Option<String>,
    ) -> Result<i64> {
        let sequence = outgoing.sequence;
        state.latest_requested_sequence = Some(sequence);
        if let Some(target) = target {
            state.performance.begin_stream_switch(sequence, target);
        }
        // Enqueue before releasing the state lock: an automatic request cannot
        // overtake a later explicit user choice on this reliable channel.
        if self.outgoing.send(outgoing).is_err() {
            state.initial_capture_sync_sent = false;
            state.pending_sequences.clear();
            state.pending_capture_preferences.clear();
            restore_confirmed_capture(state);
            state.last_error = Some("串流设置发送任务已经停止".to_owned());
            state
                .performance
                .fail_stream_switch(sequence, "串流设置发送任务已经停止");
            bail!("串流设置发送任务已经停止");
        }
        state
            .pending_capture_preferences
            .push_back(PendingCapturePreferences {
                sequence,
                preferences: StreamControlPreferences {
                    device: None,
                    custom_bitrate_limit: (state.custom_bitrate_limit < MAX_CUSTOM_BITRATE_MBPS)
                        .then_some(state.custom_bitrate_limit),
                    settings: state.settings,
                    audio: None,
                    auto_frame_quality: state.baseline.auto_frame_quality,
                },
                cursor_capture: state.baseline.cursor_capture,
                persist: state.user_settings_requested,
            });
        Ok(sequence)
    }
}

fn ensure_ready(state: &StreamControlState) -> Result<()> {
    ensure_business_ready(state)?;
    if state.remote_display.is_none() {
        bail!("尚未收到活动屏幕基线，已阻止发送不完整的串流设置");
    }
    if state.baseline.codec_type == 0 {
        bail!("视频编码尚未完成协商");
    }
    Ok(())
}

fn ensure_business_ready(state: &StreamControlState) -> Result<()> {
    if !state.control_channel_open {
        bail!("UU CONTROL 通道尚未打开");
    }
    if !state.text_channel_open {
        bail!("UU TEXT 通道尚未打开");
    }
    if !state.pb_connected {
        bail!("UU PB 特性协商尚未完成");
    }
    if protocol(state) == StreamControlProtocol::Unsupported {
        bail!("对端不支持当前串流协议（需要CaptureSetting RPC）");
    }
    Ok(())
}

fn custom_bitrate_supported(state: &StreamControlState) -> bool {
    state.pb_connected
        && state.peer_capture_setting >= crate::protocol::official_version::CUSTOM_BITRATE_MIN_LEVEL
}

fn feature_supported(
    state: &StreamControlState,
    feature: crate::account::feature_ability::Feature,
) -> bool {
    state
        .features
        .as_ref()
        .is_some_and(|policy| policy.supports(feature))
}

fn protocol(state: &StreamControlState) -> StreamControlProtocol {
    if !state.pb_connected {
        StreamControlProtocol::Negotiating
    } else if state.peer_capture_setting
        >= crate::protocol::official_version::CAPTURE_SETTING_RPC_MIN_LEVEL
    {
        StreamControlProtocol::CaptureSetting
    } else {
        StreamControlProtocol::Unsupported
    }
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

mod handshake;
mod input;
mod settings;
mod tracks;
mod wire;
