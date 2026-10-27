//! Runtime stream-setting protocol (controller side).
//!
//! Two reliable data channels serve this state machine: the peer `HELLO`
//! feature negotiation runs on the control channel, while capture-setting
//! requests travel as `SESSION` frames on the session channel.
//! Capture requests are complete snapshots,
//! so no request is produced until the active remote screen's physical mode is
//! known from `ScreenSources`.

mod audio_quality;
pub(crate) mod control_turn;
use crate::diagnostics::performance::PerformanceMonitor;
use crate::i18n::t;
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
use settings::{
    apply_reported_color, capture_setting_request, color_supported, encode_capture_request,
    format_pb_error, format_proposal, frame_rate_choice,
    quality_from_capability, quality_name, restore_confirmed_capture, validate_color,
    viewing_quality_label,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use tokio::sync::{Notify, mpsc};
use wire::{
    PbCaptureSettingResponse, PbDpiScale, PbError, PbReportQosStats, PbScreen, PbScreenSources,
    PbWinRect,
};
pub(crate) use wire::{ProfileWatch, encode_pb_echo_request};

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

const CHROMA_420: i32 = 1;
const CHROMA_444: i32 = 3;
const RESOLUTION_DEFAULT: i32 = 1;
// Screen id -2 updates existing tracks, not a physical monitor. Negative
// dimensions bypass changeResolution, and zero DPI bypasses SetDisplayDpi on
// the host. Never echo a stale monitor mode.
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
    /// The tier's bitrate ceiling (bps) for `screen` at `fps` (the host
    /// caps it at the screen's refresh rate); `None` for 自动 and 自定义.
    pub(crate) fn ceiling(self, screen: &RemoteScreen, fps: u32, true_color: bool) -> Option<u32> {
        let quality = self.capability_quality();
        if !(1..=4).contains(&quality) || screen.width == 0 || screen.height == 0 {
            return None;
        }
        let fps = if screen.refresh_hz > 0 { fps.min(screen.refresh_hz) } else { fps };
        Some(crate::features::host::parameters::tier_rate(
            quality,
            (screen.width, screen.height),
            fps.max(1),
            true_color,
        ))
    }

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

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => t().features.stream_quality_auto,
            Self::Original => t().features.stream_quality_original,
            Self::High => t().features.stream_quality_ultra,
            Self::Clear => t().features.stream_quality_high,
            Self::Custom => t().features.stream_quality_custom,
            Self::Fast => t().features.stream_quality_low_bitrate,
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
}

impl StreamControlProtocol {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Negotiating => "negotiating",
            Self::CaptureSetting => "CaptureSetting",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
// Values are validated on load.
pub struct StreamControlSettings {
    pub hdr: bool,
    pub true_color: bool,
    pub frame_rate: FrameRateChoice,
    pub quality: StreamQuality,
    pub custom_bitrate_mbps: u32,
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
                bail!("{}", t().features.stream_custom_bitrate_out_of_range);
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
        if let Some(quality) = dev_quality() {
            settings.quality = quality;
        }
        settings.custom_bitrate_mbps = normalize_custom_bitrate(settings.custom_bitrate_mbps);
        Self {
            settings,
            device: None,
            custom_bitrate_limit: None,
            audio: None,
            auto_frame_quality: if settings.quality == StreamQuality::Auto {
                dev_auto_quality().unwrap_or(saved.auto_frame_quality)
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
            2 => t().features.stream_super_screen.into(),
            1 => (t().features.stream_virtual_screen_numbered)(number),
            _ => match crate::media::capture::monitor_name(&self.name) {
                Some(name) => (t().features.stream_display_numbered_named)(number, name),
                None => (t().features.stream_display_numbered)(number),
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
    /// Control awaits the host's user (asked for this connection).
    pub control_asking: bool,
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
    /// The latest notice about the host, with its number: each one is
    /// shown once, also when its text repeats an earlier one. True when it
    /// offers to ask the host's user for control.
    pub remote_notice: Option<(u64, String, bool)>,
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
    participants: crate::features::participants::ParticipantsState,
    audio: crate::media::audio::AudioPlayback,
    network: crate::features::network_control::NetworkControl,
    shared: Arc<Mutex<StreamControlState>>,
    outgoing: mpsc::UnboundedSender<OutgoingControlMessage>,
    echo_responses: mpsc::UnboundedSender<Vec<u8>>,
    protocol_changed: Arc<Notify>,
    /// The host's latest forwarding instruction.
    forward: tokio::sync::watch::Sender<Option<wire::MediaForward>>,
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
    /// Weak-network strategy and cinema mode (device preferences).
    weak_network: i32,
    cinema: bool,
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
    clipboard_files_allowed: bool,
    remote_upgrade: Option<crate::features::remote_upgrade::RemoteUpgrade>,
    annotation: annotation::Annotation,
    annotation_extension: Option<u64>,
    custom_bitrate_limit: u32,
    features: Option<crate::account::feature_ability::FeaturePolicy>,
    remote_notice: Option<(Instant, u64, String, bool)>,
    remote_notices: u64,
    /// A control-token take awaiting the host's answer: its sequence and
    /// whether it restores the previous control state (automatic).
    /// A take awaiting the host: its sequence, whether automatic, whether
    /// it asks the host's user.
    turn_request: Option<(i64, bool, bool)>,
    /// Capabilities the host put in effect for this participant (`PERMISSIONS`).
    grants: Option<Vec<String>>,
    device_preferences: crate::features::viewing_settings::DevicePreferences,
    device_preferences_loaded: bool,
    device_preference_updates:
        tokio::sync::watch::Sender<Option<crate::features::viewing_settings::DevicePreferences>>,
    device_persistence_error: Option<String>,
    restore_input_pending: bool,
    remote_cursor: crate::features::remote_cursor::RemoteCursorState,
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
    /// Features the host declared in its `HELLO`.
    peer: crate::protocol::peer::PeerFeatures,
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
    /// Dropped frames already reported in `ReceiverStats`.
    reported_drops: u64,
}

impl StreamControlState {
    /// A notice about the host, shown once.
    pub(super) fn notify_remote(&mut self, notice: String) {
        self.notify_remote_with(notice, false);
    }
    /// `ask`: the notice offers to ask the host's user for control.
    pub(super) fn notify_remote_with(&mut self, notice: String, ask: bool) {
        self.remote_notices = self.remote_notices.wrapping_add(1);
        self.remote_notice = Some((Instant::now(), self.remote_notices, notice, ask));
    }
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
            clipboard_files_allowed: true,
            remote_upgrade: None,
            annotation: Default::default(),
            annotation_extension: None,
            custom_bitrate_limit: MAX_CUSTOM_BITRATE_MBPS,
            features: None,
            device_preferences: crate::features::viewing_settings::DevicePreferences {
                cinema: dev_cinema().unwrap_or(false),
                ..Default::default()
            },
            device_preferences_loaded: false,
            device_preference_updates: tokio::sync::watch::channel(None).0,
            device_persistence_error: None,
            restore_input_pending: false,
            remote_notice: None,
            remote_notices: 0,
            turn_request: None,
            grants: None,
            remote_cursor: cursor.clone(),
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
                // Initial defaults, replaced when negotiation data is
                // available. Not hardware capability or a user-selected
                // output-resolution ceiling.
                max_scale_width: 1920,
                max_scale_height: 1080,
                weak_network: 0,
                cinema: dev_cinema().unwrap_or(false),
            },
            capability: None,
            remote_display: None,
            control_channel_open: false,
            text_channel_open: false,
            protocol_generation: 0,
            pb_connected: false,
            peer: Default::default(),
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
            reported_drops: 0,
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
                participants: Default::default(),
                audio,
                network: crate::features::network_control::NetworkControl::new(),
                shared: Arc::new(Mutex::new(state)),
                outgoing,
                echo_responses,
                protocol_changed: Arc::new(Notify::new()),
                forward: tokio::sync::watch::channel(None).0,
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

    /// Capabilities the host reported for this participant (`PERMISSIONS`).
    pub(crate) fn granted(&self) -> Option<Vec<String>> {
        lock(&self.shared).grants.clone()
    }

    pub(crate) fn clipboard(&self) -> &crate::features::clipboard::Clipboard {
        &self.clipboard
    }

    /// The other participants, their pointers and the control turn.
    pub(crate) fn participants(&self) -> &crate::features::participants::ParticipantsState {
        &self.participants
    }

    /// Report this participant's pointer to the host (`POINTER`). Call it
    /// on every UI frame with the current position: unchanged positions and
    /// moves within the rate limit are skipped, so the final position of a
    /// movement is still sent. `position` is normalized within `screen_id`;
    /// `None` once the pointer left the remote picture.
    pub(crate) fn send_pointer(
        &self,
        screen_id: i32,
        position: Option<[f32; 2]>,
        click: Option<crate::protocol::participants::Click>,
    ) {
        if !lock(&self.shared)
            .peer
            .has(crate::protocol::peer::feature::CURSOR_PARTICIPANTS)
        {
            return;
        }
        if let Some(frame) = self.participants.pointer_frame(screen_id, position, click) {
            let _ = self.echo_responses.send(frame);
        }
    }
    /// The host's forwarding instructions (`deployment-design.md` §5.4).
    pub(crate) fn forward_instructions(
        &self,
    ) -> tokio::sync::watch::Receiver<Option<wire::MediaForward>> {
        self.forward.subscribe()
    }
    /// The picture comes through the server's forwarder: whether it is
    /// end-to-end encrypted.
    pub(crate) fn forwarded_e2ee(&self) -> Option<bool> {
        self.forward
            .borrow()
            .as_ref()
            .filter(|f| !f.room.is_empty())
            .map(|f| f.mode == "e2ee")
    }
    /// Tell the host whether this viewer receives through the forwarder.
    pub(crate) fn forward_status(&self, status: wire::ForwardStatus) {
        let _ = self
            .echo_responses
            .send(wire::push(wire::SessionBody::ForwardStatus(status)));
    }
    /// Push `VideoFeedback` on the control channel (FCT when negotiated).
    /// False when the host does not take it (`video.recovery`).
    pub(crate) fn video_feedback(&self, feedback: wire::VideoFeedback) -> bool {
        if !lock(&self.shared)
            .peer
            .has(crate::protocol::peer::feature::VIDEO_RECOVERY)
        {
            return false;
        }
        self.echo_responses
            .send(wire::push(wire::SessionBody::VideoFeedback(feedback)))
            .is_ok()
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
            network.unavailable_reason = Some(t().features.stream_manual_relay_unavailable);
        }
        let waiting_for = if !state.control_channel_open {
            Some(t().features.stream_waiting_control_channel)
        } else if !state.text_channel_open {
            Some(t().features.stream_waiting_text_channel)
        } else if !state.pb_connected {
            Some(t().features.stream_waiting_feature_negotiation)
        } else if state.remote_display.is_none() {
            Some(t().features.stream_waiting_screen_baseline)
        } else if state.baseline.codec_type == 0 {
            Some(t().features.stream_waiting_codec_negotiation)
        } else {
            None
        };
        StreamControlSnapshot {
            hdr_supported: state.peer.has(crate::protocol::peer::feature::VIDEO_HDR)
                && state.capability.is_some(),
            hdr_unavailable: format_proposal(&state, None, Some(true))
                .err()
                .map(|e| e.to_string()),
            custom_bitrate_limit: state.custom_bitrate_limit,
            auto_quality_label: (t().features.stream_quality_auto_with)(quality_name(
                state.baseline.auto_frame_quality,
            )),
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
            control_asking: state.turn_request.is_some_and(|(_, _, ask)| ask),
            mouse_error: state.mouse.error(),
            cursor_pending: state.cursor_pending.is_some(),
            cursor_error: state.cursor_error.clone(),
            local_display: state.local_display,
            settings: state.settings,
            remote_display: state.remote_display,
            screens_generation: state.screens_generation,
            display_settings_supported: display_settings::supported(&state),
            dpi_settings_supported: display_settings::supported(&state)
                && state.peer.has(crate::protocol::peer::feature::DISPLAY_CONFIG),
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
                .as_ref()
                .filter(|(at, ..)| at.elapsed() < std::time::Duration::from_secs(3))
                .map(|(_, number, notice, ask)| (*number, notice.clone(), *ask)),
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
            bail!("{}", t().features.stream_manual_relay_unavailable);
        }
        self.network.request(enabled)
    }

    pub(crate) fn set_feature_policy(
        &self,
        policy: crate::account::feature_ability::FeaturePolicy,
    ) {
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
            state.last_error = Some(t().features.stream_settings_sender_stopped.to_owned());
            state
                .performance
                .fail_stream_switch(sequence, t().features.stream_settings_sender_stopped);
            bail!("{}", t().features.stream_settings_sender_stopped);
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
        bail!("{}", t().features.stream_screen_state_not_ready);
    }
    if state.baseline.codec_type == 0 {
        bail!("{}", t().features.stream_codec_not_negotiated);
    }
    Ok(())
}

fn ensure_business_ready(state: &StreamControlState) -> Result<()> {
    if !state.control_channel_open {
        bail!("{}", t().features.stream_control_channel_closed);
    }
    if !state.text_channel_open {
        bail!("{}", t().features.stream_session_channel_closed);
    }
    if protocol(state) == StreamControlProtocol::Negotiating {
        bail!("{}", t().features.stream_peer_negotiation_pending);
    }
    Ok(())
}

fn custom_bitrate_supported(state: &StreamControlState) -> bool {
    state.pb_connected && state.peer.known()
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

/// Every host speaks CaptureSetting once its HELLO is in (a closed session
/// channel forgets it until the next one).
fn protocol(state: &StreamControlState) -> StreamControlProtocol {
    if state.pb_connected && state.peer.known() {
        StreamControlProtocol::CaptureSetting
    } else {
        StreamControlProtocol::Negotiating
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
pub(crate) mod wire;

static DEV_CINEMA: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// `connect --dev-cinema`: start this viewer in (or out of) cinema mode
/// whatever the saved device preference (measurements; not saved).
pub(crate) fn set_dev_cinema(cinema: bool) {
    let _ = DEV_CINEMA.set(cinema);
}

fn dev_cinema() -> Option<bool> {
    DEV_CINEMA.get().copied()
}

static DEV_VIRTUAL_SCREEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `connect --dev-virtual-screen`: add one virtual screen to the host once
/// the session can (tests; the same request as the confirmed menu action).
pub(crate) fn set_dev_virtual_screen() {
    DEV_VIRTUAL_SCREEN.store(true, std::sync::atomic::Ordering::Release);
}

/// Whether the requested virtual screen is due now (`ready`); once.
pub(crate) fn take_dev_virtual_screen(ready: bool) -> bool {
    ready && DEV_VIRTUAL_SCREEN.swap(false, std::sync::atomic::Ordering::AcqRel)
}

static DEV_AUTO_QUALITY: std::sync::OnceLock<i32> = std::sync::OnceLock::new();

/// `connect --dev-auto-quality`: start automatic quality at this tier instead
/// of the one saved for the device, and do not save the settled tier
/// (measurements: every run starts alike).
static DEV_QUALITY: std::sync::OnceLock<StreamQuality> = std::sync::OnceLock::new();

/// `connect --dev-audio-only A,B`: audio only A seconds after the first
/// connection, the picture again at B seconds (once).
static DEV_AUDIO_SWITCH: std::sync::Mutex<Option<(u64, u64)>> = std::sync::Mutex::new(None);

pub(crate) fn set_dev_audio_switch(on: u64, off: u64) {
    *DEV_AUDIO_SWITCH.lock().unwrap_or_else(|p| p.into_inner()) = Some((on, off));
}

pub(crate) fn take_dev_audio_switch() -> Option<(u64, u64)> {
    DEV_AUDIO_SWITCH.lock().unwrap_or_else(|p| p.into_inner()).take()
}

/// `connect --dev-quality`: start at this tier (not saved).
pub(crate) fn set_dev_quality(quality: StreamQuality) {
    let _ = DEV_QUALITY.set(quality);
}

fn dev_quality() -> Option<StreamQuality> {
    DEV_QUALITY.get().copied()
}

pub(crate) fn set_dev_auto_quality(quality: i32) {
    let _ = DEV_AUTO_QUALITY.set(quality);
}

fn dev_auto_quality() -> Option<i32> {
    DEV_AUTO_QUALITY.get().copied()
}
