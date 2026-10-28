//! Farwing session messages (peer frame type `SESSION`, v2).
//!
//! A `SESSION` frame body is one [`SessionMessage`] with exactly one body.
//! Requests carry a non-zero FPP request ID and are answered with the same ID
//! and the `RESPONSE` flag; pushes carry request ID 0 and are never answered.
//! See `docs/protocol/v2-design.md` §6 for every message.
use super::{control_turn, display_topology, microphone};
use crate::protocol::peer::{self, FrameType, flags};
use anyhow::{Context as _, Result};
use prost::Message as _;


/// `HELLO` request sent by the controller until the host answers. It
/// carries the chosen cursor colour, if any (§4.3).
pub(crate) fn encode_pb_echo_request() -> Vec<u8> {
    let color = crate::media::preferences::load()
        .map(|options| options.pointer_color & 0x00FF_FFFF)
        .unwrap_or(0);
    if color == 0 {
        return peer::hello_request(peer::Role::Controller);
    }
    peer::hello_request_with(farwing_protocol::peer::Profile { color })
}

/// The cursor colour setting during a session: a changed colour is sent as
/// another `HELLO` request with the profile (the host applies the profile
/// of any `HELLO`; 0 returns to the colour the host gave).
pub(crate) struct ProfileWatch {
    modified: Option<std::time::SystemTime>,
    color: u32,
}

impl ProfileWatch {
    pub(crate) fn new() -> Self {
        Self {
            modified: crate::media::preferences::modified(),
            color: crate::media::preferences::load()
                .map(|options| options.pointer_color & 0x00FF_FFFF)
                .unwrap_or(0),
        }
    }

    /// A `HELLO` request when the colour setting changed since last called.
    pub(crate) fn changed(&mut self) -> Option<Vec<u8>> {
        let modified = crate::media::preferences::modified();
        if modified == self.modified {
            return None;
        }
        self.modified = modified;
        let color = crate::media::preferences::load().ok()?.pointer_color & 0x00FF_FFFF;
        if color == self.color {
            return None;
        }
        self.color = color;
        Some(peer::hello_request_with(farwing_protocol::peer::Profile { color }))
    }
}

/// `HELLO|RESPONSE` answering a host `HELLO`.
pub(super) fn encode_pb_echo_response(request_id: u32) -> Vec<u8> {
    peer::hello_response(peer::Role::Controller, request_id)
}

/// The FPP request ID of a controller request sequence.
pub(crate) fn request_id(sequence: i64) -> u32 {
    // Sequences start at 1 and only grow; 0 is reserved for pushes.
    ((sequence - 1).rem_euclid(i64::from(u32::MAX)) + 1) as u32
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct SessionMessage {
    #[prost(oneof = "SessionBody", tags = "1,2,3,4,5,7,8,10,11,12,13,14,15,16,17,20,21,22,23,24,25")]
    pub(crate) body: Option<SessionBody>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub(crate) enum SessionBody {
    // ------------------------------------------------------------ pushes
    /// Host screens (H→C) whenever the list or a screen changes.
    #[prost(message, tag = "1")]
    Screens(PbScreenSources),
    /// Host encoder and quality status (H→C).
    #[prost(message, tag = "2")]
    Quality(PbReportQosStats),
    /// Host virtual microphone state (H→C).
    #[prost(message, tag = "3")]
    Microphone(MicrophoneState),
    /// Whether the host accepts files on the clipboard (H→C).
    #[prost(message, tag = "4")]
    ClipboardFiles(ClipboardPermission),
    /// Start or stop capturing a screen (C→H); no reply.
    #[prost(message, tag = "5")]
    Capture(CaptureControl),
    // ---------------------------------------------- requests (C→H)
    #[prost(message, tag = "10")]
    CaptureSetting(PbCaptureSettingRequest),
    #[prost(message, tag = "11")]
    CreateVirtualDisplay(display_topology::PbCreateVirtualDisplay),
    #[prost(message, tag = "12")]
    RemoveVirtualDisplay(display_topology::PbRemoveVirtualDisplay),
    #[prost(message, tag = "13")]
    EnterExclusive(display_topology::PbEnterSuperScreen),
    #[prost(message, tag = "14")]
    QuitExclusive(display_topology::PbQuitSuperScreen),
    /// The video tracks the controller negotiated and can receive.
    #[prost(message, tag = "15")]
    VideoTracks(PbSendVideoTrackRequest),
    #[prost(message, tag = "16")]
    MicrophonePolicy(microphone::PolicyRequest),
    /// Take or release the control token.
    #[prost(message, tag = "17")]
    ControlTurn(control_turn::TurnRequest),
    /// Controller → host, once a second: how the controller keeps up.
    #[prost(message, tag = "7")]
    ReceiverStats(ReceiverStats),
    /// Controller → host on the control channel: long-term references it
    /// holds, or a request for a recovery frame (`video.recovery`).
    #[prost(message, tag = "8")]
    VideoFeedback(VideoFeedback),
    /// Host → controller: where the picture comes from — the forwarder, or
    /// the host directly again (`deployment-design.md` §5.4).
    #[prost(message, tag = "24")]
    Forward(MediaForward),
    /// Controller → host: whether it receives through the forwarder.
    #[prost(message, tag = "25")]
    ForwardStatus(ForwardStatus),
    // --------------------------------------------- responses (H→C)
    #[prost(message, tag = "20")]
    CaptureSettingResult(PbCaptureSettingResponse),
    /// Result of requests 11–15.
    #[prost(message, tag = "21")]
    DisplayResult(display_topology::PbDisplayResult),
    #[prost(message, tag = "22")]
    MicrophoneResult(microphone::PolicyResponse),
    #[prost(message, tag = "23")]
    ControlTurnResult(control_turn::TurnResponse),
}

impl SessionBody {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Screens(_) => "Screens",
            Self::Quality(_) => "Quality",
            Self::Microphone(_) => "Microphone",
            Self::ClipboardFiles(_) => "ClipboardFiles",
            Self::Capture(_) => "Capture",
            Self::CaptureSetting(_) => "CaptureSetting",
            Self::ReceiverStats(_) => "ReceiverStats",
            Self::VideoFeedback(_) => "VideoFeedback",
            Self::Forward(_) => "Forward",
            Self::ForwardStatus(_) => "ForwardStatus",
            Self::CreateVirtualDisplay(_) => "CreateVirtualDisplay",
            Self::RemoveVirtualDisplay(_) => "RemoveVirtualDisplay",
            Self::EnterExclusive(_) => "EnterExclusive",
            Self::QuitExclusive(_) => "QuitExclusive",
            Self::VideoTracks(_) => "VideoTracks",
            Self::MicrophonePolicy(_) => "MicrophonePolicy",
            Self::ControlTurn(_) => "ControlTurn",
            Self::ControlTurnResult(_) => "ControlTurnResult",
            Self::CaptureSettingResult(_) => "CaptureSettingResult",
            Self::DisplayResult(_) => "DisplayResult",
            Self::MicrophoneResult(_) => "MicrophoneResult",
        }
    }
}

/// `docs/protocol/rfi-ltr-brief.md` §6.3. Acknowledgements omit `recover`
/// and `request`.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct VideoFeedback {
    /// Media SSRC of the video track.
    #[prost(fixed32, tag = "1")]
    pub(crate) ssrc: u32,
    /// `frame-meta` epoch (0–255).
    #[prost(uint32, tag = "2")]
    pub(crate) epoch: u32,
    /// Newest `frame_id` of the continuously decoded chain in this epoch.
    #[prost(uint32, tag = "3")]
    pub(crate) decoded: u32,
    /// Decoded long-term references still held, newest first (at most 4).
    #[prost(uint32, repeated, packed = "true", tag = "4")]
    pub(crate) ltr: Vec<u32>,
    /// Loss after `decoded` that repair cannot undo: send a recovery frame.
    #[prost(bool, tag = "5")]
    pub(crate) recover: bool,
    /// Recovery request number, one per loss event.
    #[prost(uint32, tag = "6")]
    pub(crate) request: u32,
    /// Browsers, which do not see `frame-meta`: the RTP timestamp of the
    /// newest frame they decoded (fields 2–4 unused); the host resolves what
    /// that frame's dependency chain holds.
    #[prost(fixed32, optional, tag = "7")]
    pub(crate) rtp_timestamp: Option<u32>,
}

/// Where the controller's picture comes from.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub(crate) struct MediaForward {
    /// Changes with every instruction; the status names it.
    #[prost(uint32, tag = "1")]
    pub(crate) generation: u32,
    /// The forwarding room; empty: directly from the host again.
    #[prost(string, tag = "2")]
    pub(crate) room: String,
    /// `e2ee` or `compatible`.
    #[prost(string, tag = "3")]
    pub(crate) mode: String,
    /// The controller's video tracks and the forwarder tracks they come from.
    #[prost(message, repeated, tag = "4")]
    pub(crate) tracks: Vec<ForwardedTrack>,
    /// The session the controller subscribes with.
    #[prost(string, tag = "5")]
    pub(crate) session: String,
    /// End-to-end mode: the frame key (16-byte AES key, 4-byte salt).
    #[prost(bytes = "vec", tag = "6")]
    pub(crate) key: Vec<u8>,
    #[prost(uint32, tag = "7")]
    pub(crate) key_id: u32,
    /// The forwarder's audio track for this controller; empty: its sound
    /// stays direct (no audio grant).
    #[prost(string, tag = "8")]
    pub(crate) audio: String,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub(crate) struct ForwardedTrack {
    /// The controller's video track index (`video_<index>`).
    #[prost(uint32, tag = "1")]
    pub(crate) index: u32,
    /// The forwarder's track ID.
    #[prost(string, tag = "2")]
    pub(crate) track: String,
}

#[derive(Clone, PartialEq, Eq, prost::Message)]
pub(crate) struct ForwardStatus {
    #[prost(uint32, tag = "1")]
    pub(crate) generation: u32,
    /// Receiving through the forwarder; false: it could not, send directly.
    #[prost(bool, tag = "2")]
    pub(crate) receiving: bool,
    #[prost(string, tag = "3")]
    pub(crate) error: String,
}

/// A push (request ID 0).
pub(crate) fn push(body: SessionBody) -> Vec<u8> {
    peer::message(FrameType::Session, &SessionMessage { body: Some(body) })
}

/// A request of controller sequence `sequence`.
pub(crate) fn request(sequence: i64, body: SessionBody) -> Vec<u8> {
    peer::framed(
        FrameType::Session,
        0,
        request_id(sequence),
        &SessionMessage { body: Some(body) },
    )
}

/// The reply to request `request_id`.
pub(crate) fn response(request_id: u32, body: SessionBody) -> Vec<u8> {
    peer::framed(
        FrameType::Session,
        flags::RESPONSE,
        request_id,
        &SessionMessage { body: Some(body) },
    )
}

/// A failed reply to request `request_id`; `reason` is worded by the
/// controller.
pub(crate) fn error_response(request_id: u32, code: &str, reason: &str) -> Vec<u8> {
    peer::error_response(FrameType::Session, request_id, code, reason)
}

/// A decoded `SESSION` frame.
pub(crate) struct Received {
    pub request_id: u32,
    pub response: bool,
    /// Set on a failed reply; `body` is then `None`.
    pub error: Option<farwing_protocol::peer::PeerError>,
    pub body: Option<SessionBody>,
}

impl Received {
    /// The controller sequence a reply answers.
    pub(crate) fn sequence(&self) -> i64 {
        i64::from(self.request_id)
    }
}

/// Decode `bytes` if it is a `SESSION` frame; `None` for other frames.
pub(crate) fn decode(bytes: &[u8]) -> Result<Option<Received>> {
    let Ok((header, body)) = farwing_protocol::peer::decode(bytes) else {
        return Ok(None);
    };
    if header.frame_type() != Some(FrameType::Session) {
        return Ok(None);
    }
    if header.is_error() {
        let error = farwing_protocol::peer::PeerError::decode(body)
            .context("invalid SESSION error reply")?;
        return Ok(Some(Received {
            request_id: header.request_id,
            response: true,
            error: Some(error),
            body: None,
        }));
    }
    let message = SessionMessage::decode(body).context("invalid SESSION message")?;
    Ok(Some(Received {
        request_id: header.request_id,
        response: header.is_response(),
        error: None,
        body: message.body,
    }))
}

/// Start (`start`) or stop capturing `screen_id`; -1 = every screen of the
/// session (stop keeps the slots so that start(-1) resumes them).
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct CaptureControl {
    #[prost(int32, tag = "1")]
    pub(crate) screen_id: i32,
    #[prost(bool, tag = "2")]
    pub(crate) start: bool,
}

/// Host virtual microphone state.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct MicrophoneState {
    /// One of [`microphone_state`].
    #[prost(int32, tag = "1")]
    pub(crate) state: i32,
}

pub(crate) mod microphone_state {
    /// The host plays the controller's microphone.
    pub const ACTIVE: i32 = 1;
    /// Enabled but not playing (or just disabled).
    pub const INACTIVE: i32 = 2;
    /// The host's audio path failed: restart local capture.
    pub const RESTART: i32 = 3;
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct ClipboardPermission {
    #[prost(bool, tag = "1")]
    pub(crate) enabled: bool,
}

/// Host encoder and quality status.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct PbReportQosStats {
    /// `software` or `hardware`.
    #[prost(string, tag = "1")]
    pub(crate) encoder_type: String,
    /// Capture backend name.
    #[prost(string, tag = "2")]
    pub(crate) capture_type: String,
    /// Effective quality tier 1..4 (automatic already resolved) or 6 = custom.
    #[prost(int32, tag = "4")]
    pub(crate) video_quality: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct PbSendVideoTrackRequest {
    #[prost(int32, repeated, tag = "1")]
    pub(crate) video_track_index: Vec<i32>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct PbCaptureSettingRequest {
    #[prost(int32, tag = "1")]
    pub(crate) fps: i32,
    #[prost(int32, tag = "2")]
    pub(crate) frame_quality: i32,
    #[prost(bool, tag = "3")]
    pub(crate) cursor_capture: bool,
    #[prost(int32, tag = "4")]
    pub(crate) screen_id: i32,
    #[prost(int32, tag = "5")]
    pub(crate) resolution_width: i32,
    #[prost(int32, tag = "6")]
    pub(crate) resolution_height: i32,
    #[prost(int32, tag = "7")]
    pub(crate) chroma_format: i32,
    #[prost(int32, tag = "8")]
    pub(crate) max_custom_bitrate: i32,
    #[prost(int32, tag = "9")]
    pub(crate) dpi_scale: i32,
    #[prost(int32, tag = "10")]
    pub(crate) resolution_type: i32,
    #[prost(bool, tag = "11")]
    pub(crate) enable_hdr: bool,
    #[prost(int32, tag = "12")]
    pub(crate) auto_frame_quality: i32,
    #[prost(int32, tag = "13")]
    pub(crate) codec_type: i32,
    #[prost(int32, tag = "14")]
    pub(crate) max_scale_width: i32,
    #[prost(int32, tag = "15")]
    pub(crate) max_scale_height: i32,
    #[prost(int32, tag = "18")]
    pub(crate) fps_count: i32,
    /// What gives way on a weak network: 0 = bitrate first, 1 = frame rate first.
    #[prost(int32, tag = "19")]
    pub(crate) weak_network: i32,
    /// Quality-first encoding for watching video.
    #[prost(bool, tag = "20")]
    pub(crate) cinema: bool,
}

/// What the controller receives, decodes and shows of the active video.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct ReceiverStats {
    #[prost(float, tag = "1")]
    pub(crate) receive_fps: f32,
    #[prost(float, tag = "2")]
    pub(crate) decode_fps: f32,
    #[prost(float, tag = "3")]
    pub(crate) render_fps: f32,
    /// Time from a frame's arrival to the end of its decode.
    #[prost(float, tag = "4")]
    pub(crate) decode_delay_ms: f32,
    /// Frames dropped before decode or presentation since the last report.
    #[prost(uint32, tag = "5")]
    pub(crate) dropped_frames: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct PbCaptureSettingResponse {
    #[prost(message, repeated, tag = "1")]
    pub(crate) errors: Vec<PbError>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct PbError {
    #[prost(int32, tag = "1")]
    pub(crate) error_code: i32,
    /// One of [`setting_reason`] (or empty); the receiver words it.
    #[prost(string, tag = "2")]
    pub(crate) error_message: String,
    #[prost(string, tag = "3")]
    pub(crate) error_detail: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct PbScreenSources {
    #[prost(message, repeated, tag = "1")]
    pub(crate) screens: Vec<PbScreen>,
    #[prost(int32, tag = "2")]
    pub(crate) current_screen_id: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct PbScreen {
    /// Preferred (native panel) mode. Never repurpose init_resolution: it is
    /// a connection snapshot, not the panel mode.
    #[prost(message, optional, tag = "100")]
    pub(crate) display_geometry: Option<PbDisplayGeometry>,
    #[prost(int32, tag = "1")]
    pub(crate) id: i32,
    #[prost(int32, tag = "2")]
    pub(crate) fps: i32,
    #[prost(message, repeated, tag = "3")]
    pub(crate) resolutions: Vec<PbWinRect>,
    #[prost(message, optional, tag = "4")]
    pub(crate) current_resolution: Option<PbWinRect>,
    #[prost(int32, tag = "5")]
    pub(crate) screen_type: i32,
    #[prost(message, optional, tag = "6")]
    pub(crate) init_resolution: Option<PbWinRect>,
    #[prost(bool, tag = "7")]
    pub(crate) is_primary_screen: bool,
    #[prost(message, optional, tag = "9")]
    pub(crate) dpi_scale: Option<PbDpiScale>,
    #[prost(string, tag = "10")]
    pub(crate) display_name: String,
    #[prost(int32, tag = "11")]
    pub(crate) resolution_type: i32,
    #[prost(int32, tag = "12")]
    pub(crate) video_track_index: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct PbDisplayGeometry {
    #[prost(uint32, tag = "2")]
    pub width: u32,
    #[prost(uint32, tag = "3")]
    pub height: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct PbWinRect {
    #[prost(int32, tag = "1")]
    pub(crate) left: i32,
    #[prost(int32, tag = "2")]
    pub(crate) top: i32,
    #[prost(int32, tag = "3")]
    pub(crate) width: i32,
    #[prost(int32, tag = "4")]
    pub(crate) height: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct PbDpiScale {
    #[prost(int32, tag = "1")]
    pub(crate) current_dpi: i32,
    #[prost(int32, tag = "2")]
    pub(crate) recommended_dpi: i32,
    #[prost(int32, repeated, tag = "3")]
    pub(crate) dpis: Vec<i32>,
}

#[cfg(test)]
mod receiver_stats_tests {
    use super::*;

    #[test]
    fn receiver_stats_push_round_trips() {
        let frame = push(SessionBody::ReceiverStats(ReceiverStats {
            receive_fps: 144.0,
            decode_fps: 143.5,
            render_fps: 143.0,
            decode_delay_ms: 3.5,
            dropped_frames: 0,
        }));
        let received = decode(&frame).unwrap().expect("a SESSION frame");
        assert!(!received.response);
        assert_eq!(received.request_id, 0);
        assert!(matches!(received.body, Some(SessionBody::ReceiverStats(_))), "{}", frame.len());
    }
}

/// Why a capture setting was not applied ([`PbError::error_message`]).
pub(crate) mod setting_reason {
    /// The participant may not change the host's displays.
    pub const DISPLAY_NOT_ALLOWED: &str = "display_not_allowed";
    /// The host is installing an update.
    pub const UPDATING: &str = "updating";
    pub const INVALID_SCALE_LIMIT: &str = "invalid_scale_limit";
    pub const UNSUPPORTED_CODEC: &str = "unsupported_codec";
    /// The resolution needs a super display, which could not be made.
    pub const CONVERSION_FAILED: &str = "conversion_failed";
    /// Anything else; the host logs why.
    pub const NOT_APPLIED: &str = "not_applied";
}
