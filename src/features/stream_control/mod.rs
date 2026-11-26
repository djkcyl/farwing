//! Runtime stream-setting protocol (controller side).
//!
//! Two reliable data channels serve this state machine: the peer `HELLO`
//! feature negotiation runs on the control channel, while capture-setting
//! requests travel as `SESSION` frames on the session channel.
//! Capture requests are complete snapshots,
//! so no request is produced until the active remote screen's physical mode is
//! known from `ScreenSources`.

mod audio_quality;
pub(crate) mod camera;
mod cameras;
pub(crate) mod control_turn;
use crate::diagnostics::performance::PerformanceMonitor;
use crate::i18n::t;
pub use crate::features::network_control::NetworkControlSnapshot;
pub use crate::features::remote_cursor::{CursorImage, RemoteCursor};
pub use crate::features::remote_input::MouseMode;
use crate::media::{ConnectionMediaProfile, FrameRateChoice, LocalDisplayInfo};
use anyhow::{Result, anyhow, bail};
pub use display_settings::{
    DisplayChangeRequest, DisplayChangeStatus, DisplayResolution, RemoteDisplayInfo,
    RemoteDisplayMode,
};
pub use display_topology::{DisplayTopologyAction, DisplayTopologyStatus, DisplayTopologySupport};
use settings::{color_supported, format_proposal, frame_rate_choice, viewing_quality_label};
pub(crate) use settings::Wanted;

/// The settings of a device with nothing saved.
pub(crate) fn default_settings(profile: ConnectionMediaProfile) -> StreamControlSettings {
    StreamControlSettings {
        true_color: false,
        hdr: false,
        quality: StreamQuality::Auto,
        frame_rate: frame_rate_choice(profile.stream_fps),
        custom_bitrate_mbps: DEFAULT_CUSTOM_BITRATE_MBPS,
    }
}
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use tokio::sync::{Notify, mpsc};
pub(crate) use wire::{ProfileWatch, encode_pb_echo_request};

pub(crate) mod annotation;
mod display_settings;
mod display_topology;
mod microphone;
mod preferences;
#[cfg(windows)]
pub(crate) mod publisher;

const DEFAULT_CUSTOM_BITRATE_MBPS: u32 = 30;
pub const MAX_CUSTOM_BITRATE_MBPS: u32 = 500;

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
    #[cfg(windows)]
    pub(crate) fn ceiling(self, screen: &RemoteScreen, fps: u32, true_color: bool) -> Option<u32> {
        use crate::features::host::parameters::Tier;
        let tier = match self {
            Self::Fast => Tier::P720,
            Self::Clear => Tier::P1080,
            Self::High => Tier::P1440,
            Self::Original => Tier::P2160,
            Self::Auto | Self::Custom => return None,
        };
        if screen.width == 0 || screen.height == 0 {
            return None;
        }
        let fps = if screen.refresh_hz > 0 { fps.min(screen.refresh_hz) } else { fps };
        Some(crate::features::host::parameters::tier_rate(
            tier,
            (screen.width, screen.height),
            fps.max(1),
            true_color,
        ))
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
    audio: Option<crate::media::audio::AudioSettings>,
    pub(crate) device: Option<crate::features::viewing_settings::DevicePreferences>,
}

impl StreamControlPreferences {
    pub(crate) fn from_saved(saved: StreamControlSettings, _profile: ConnectionMediaProfile) -> Self {
        let mut settings = saved;
        if let Some(quality) = dev_quality() {
            settings.quality = quality;
        }
        settings.custom_bitrate_mbps = normalize_custom_bitrate(settings.custom_bitrate_mbps);
        Self {
            settings,
            device: None,
            audio: None,
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
    /// What input and tabs bind to: the display and its kind.
    pub(crate) fn binding(&self) -> u64 {
        binding(self.id, self.display.kind)
    }
    pub(crate) fn label(&self, catalog: &[Self]) -> String {
        let kind = self.display.kind;
        let number = catalog
            .iter()
            .filter(|s| s.display.kind == kind)
            .position(|s| s.id == self.id)
            .unwrap_or(0)
            + 1;
        match kind {
            wire::DisplayKind::Camera => self.name.clone(),
            wire::DisplayKind::Exclusive => t().features.stream_super_screen.into(),
            wire::DisplayKind::Virtual => (t().features.stream_virtual_screen_numbered)(number),
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
    pub auto_quality_label: String,
    pub true_color_supported: bool,
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

    pub custom_bitrate_supported: bool,
    pub mouse_modes_supported: bool,
    pub control_channel_open: bool,
    pub text_channel_open: bool,
    pub hello_answered: bool,
    pub ready: bool,
    pub waiting_for: Option<&'static str>,
    pub pending_sequence: Option<i64>,
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
    #[cfg(windows)]
    clipboard: crate::features::clipboard::Clipboard,
    #[cfg(windows)]
    drag_drop: crate::features::drag_drop::controller::Controller,
    #[cfg(windows)]
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
pub(crate) struct HelloStatus {
    pub generation: u64,
    pub open: bool,
    pub connected: bool,
}

pub(crate) struct OutgoingControlMessage {
    pub annotation_generation: Option<u64>,
    pub sequence: i64,
    pub payload: Vec<u8>,
    pub completion: Option<tokio::sync::oneshot::Sender<std::result::Result<(), String>>>,
}

fn binding(id: i32, kind: wire::DisplayKind) -> u64 {
    (id as u32 as u64) | ((kind as i32 as u32 as u64) << 32)
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
    display: RemoteDisplayInfo,
}
impl ScreenBaseline {
    fn binding(&self) -> u64 {
        binding(self.id, self.display.kind)
    }
}

struct StreamControlState {
    #[cfg(windows)]
    clipboard_files_allowed: bool,
    #[cfg(windows)]
    remote_upgrade: Option<crate::features::remote_upgrade::RemoteUpgrade>,
    annotation: annotation::Annotation,
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
    /// The stream request carrying the cursor change in flight.
    cursor_pending: Option<i64>,
    cursor_error: Option<String>,
    /// The cursor is drawn into the picture (the mouse policy wants it).
    cursor_capture: bool,
    /// SESSION requests awaiting the host's answer.
    requests: requests::Requests,
    /// The revision of the display list in effect.
    displays_revision: u64,
    local_display: LocalDisplayInfo,
    active_video_track_index: i32,
    current_screen_id: i32,
    screens: Vec<ScreenBaseline>,
    screens_generation: u64,
    display_changes: display_settings::DisplayChanges,
    topology: display_topology::DisplayTopology,

    settings: StreamControlSettings,
    /// What the host holds of this controller's stream (the start offer,
    /// then each request).
    held: Option<wire::StreamSettings>,
    /// The host's latest stream state.
    stream_state: Option<wire::StreamState>,
    /// The stream request in flight.
    pending_stream: Option<i64>,
    /// The codec this device prefers; `None` lets the host choose.
    codec_preference: Option<farwing_protocol::session::Codec>,
    /// What this controller decodes (its start offer).
    decoders: Vec<farwing_protocol::session::Decoder>,
    remote_display: Option<RemoteDisplayState>,
    control_channel_open: bool,
    text_channel_open: bool,
    protocol_generation: u64,
    hello_answered: bool,
    /// Features the host declared in its `HELLO`.
    peer: crate::protocol::peer::PeerFeatures,
    viewing_enabled: bool,
    next_sequence: i64,
    last_applied_sequence: Option<i64>,
    latest_requested_sequence: Option<i64>,
    last_error: Option<String>,
    last_notice: Option<String>,
    preference_updates: tokio::sync::watch::Sender<Option<StreamControlSettings>>,
    confirmed_preferences: StreamControlPreferences,
    user_settings_requested: bool,
    persistence_error: Option<String>,
    audio_persistence_error: Option<String>,
    performance: PerformanceMonitor,
    /// Dropped frames already reported in `ReceiverStats`.
    reported_drops: u64,
    cameras: cameras::Cameras,
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
        let mouse = crate::features::remote_input::RemoteInput::default();
        let cursor = crate::features::remote_cursor::RemoteCursorState::default();
        let state = StreamControlState {
            #[cfg(windows)]
            clipboard_files_allowed: true,
            #[cfg(windows)]
            remote_upgrade: None,
            annotation: Default::default(),
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
            // Watching needs capture-side cursor motion.
            cursor_capture: true,
            requests: Default::default(),
            displays_revision: 0,
            local_display: profile.local_display,
            active_video_track_index: 0,
            current_screen_id: 0,
            screens: Vec::new(),
            screens_generation: 0,
            display_changes: Default::default(),
            topology: Default::default(),

            settings: StreamControlSettings {
                true_color: false,
                hdr: false,
                frame_rate,
                quality: StreamQuality::Auto,
                custom_bitrate_mbps: DEFAULT_CUSTOM_BITRATE_MBPS,
            },
            held: None,
            stream_state: None,
            pending_stream: None,
            codec_preference: profile.codec.preferred(),
            decoders: Vec::new(),
            remote_display: None,
            control_channel_open: false,
            text_channel_open: false,
            protocol_generation: 0,
            hello_answered: false,
            peer: Default::default(),
            viewing_enabled: true,
            next_sequence: 1,
            last_applied_sequence: None,
            latest_requested_sequence: None,
            last_error: None,
            last_notice: None,
            preference_updates: tokio::sync::watch::channel(None).0,
            confirmed_preferences: StreamControlPreferences {
                device: None,
                settings: StreamControlSettings {
                    true_color: false,
                    hdr: false,
                    frame_rate,
                    quality: StreamQuality::Auto,
                    custom_bitrate_mbps: DEFAULT_CUSTOM_BITRATE_MBPS,
                },
                audio: None,
            },
            user_settings_requested: false,
            persistence_error: None,
            audio_persistence_error: None,
            performance,
            reported_drops: 0,
            cameras: Default::default(),
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
                #[cfg(windows)]
                clipboard: crate::features::clipboard::Clipboard::new(),
                #[cfg(windows)]
                drag_drop: Default::default(),
                #[cfg(windows)]
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

    #[cfg(windows)]
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
    #[cfg(windows)]
    pub(crate) fn drag_drop(&self) -> &crate::features::drag_drop::controller::Controller {
        &self.drag_drop
    }

    #[cfg(windows)]
    pub(crate) fn file_transfer(&self) -> &Arc<crate::features::file_transfer::Transport> {
        &self.files
    }

    pub(crate) fn diagnostics(&self) -> &crate::diagnostics::remote::Controller {
        &self.diagnostics
    }

    pub fn snapshot(&self) -> StreamControlSnapshot {
        let mut state = lock(&self.shared);
        self.expire_requests(&mut state);
        let mut network = self.network.snapshot();
        if !feature_supported(
            &state,
            crate::account::feature_ability::Feature::Files,
        ) {
            network.available = false;
            network.unavailable_reason = Some(t().features.stream_manual_relay_unavailable);
        }
        let waiting_for = if !state.control_channel_open {
            Some(t().features.stream_waiting_control_channel)
        } else if !state.text_channel_open {
            Some(t().features.stream_waiting_text_channel)
        } else if !state.hello_answered {
            Some(t().features.stream_waiting_feature_negotiation)
        } else if state.remote_display.is_none() {
            Some(t().features.stream_waiting_screen_baseline)
        } else {
            None
        };
        StreamControlSnapshot {
            hdr_supported: state
                .stream_state
                .as_ref()
                .and_then(|s| s.limits.as_ref())
                .is_some_and(|limits| limits.hdr)
                && state.decoders.iter().any(|d| d.bit10),
            hdr_unavailable: format_proposal(&state, None, Some(true))
                .err()
                .map(|e| e.to_string()),
            auto_quality_label: (t().features.stream_quality_auto_with)(settings::quality_name(
                settings::automatic_tier(&state).unwrap_or(wire::Quality::Standard),
            )),
            true_color_supported: color_supported(&state),
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
            custom_bitrate_supported: custom_bitrate_supported(&state)
                && feature_supported(
                    &state,
                    crate::account::feature_ability::Feature::Host,
                ),
            mouse_modes_supported: feature_supported(
                &state,
                crate::account::feature_ability::Feature::Host,
            ),
            control_channel_open: state.control_channel_open,
            text_channel_open: state.text_channel_open,
            hello_answered: state.hello_answered,
            ready: waiting_for.is_none() && negotiated(&state),
            waiting_for,
            pending_sequence: state.pending_stream,
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
            audio: Some(self.audio.settings()),
        }
    }
    pub(crate) fn network_control(&self) -> crate::features::network_control::NetworkControl {
        self.network.clone()
    }

    pub fn set_relay_enabled(&self, enabled: bool) -> Result<()> {
        if !feature_supported(
            &lock(&self.shared),
            crate::account::feature_ability::Feature::Files,
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

    #[cfg(windows)]
    pub(crate) fn set_remote_upgrade(
        &self,
        upgrade: crate::features::remote_upgrade::RemoteUpgrade,
    ) {
        lock(&self.shared).remote_upgrade = Some(upgrade);
    }

    #[cfg(windows)]
    pub(crate) fn remote_upgrade(&self) -> Option<crate::features::remote_upgrade::RemoteUpgrade> {
        lock(&self.shared).remote_upgrade.clone()
    }

    pub(crate) fn preference_updates(
        &self,
    ) -> tokio::sync::watch::Receiver<Option<StreamControlSettings>> {
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

}

fn ensure_ready(state: &StreamControlState) -> Result<()> {
    ensure_business_ready(state)?;
    if state.remote_display.is_none() {
        bail!("{}", t().features.stream_screen_state_not_ready);
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
    if !negotiated(state) {
        bail!("{}", t().features.stream_peer_negotiation_pending);
    }
    Ok(())
}

fn custom_bitrate_supported(state: &StreamControlState) -> bool {
    state.hello_answered && state.peer.known()
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

/// The host answered our HELLO: its features are known (a closed channel
/// forgets them until the next HELLO).
fn negotiated(state: &StreamControlState) -> bool {
    state.hello_answered && state.peer.known()
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

mod handshake;
mod input;
mod requests;
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

/// `connect --dev-quality`: start at this tier (not saved).
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

pub(crate) fn set_dev_quality(quality: StreamQuality) {
    let _ = DEV_QUALITY.set(quality);
}

fn dev_quality() -> Option<StreamQuality> {
    DEV_QUALITY.get().copied()
}
