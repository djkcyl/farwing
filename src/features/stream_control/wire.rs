//! Farwing session messages (peer frame type `SESSION`, v2).
//!
//! A `SESSION` frame body is one [`SessionMessage`] with exactly one body.
//! Requests carry a non-zero FPP request ID and are answered with the same ID
//! and the `RESPONSE` flag; pushes carry request ID 0 and are never answered.
//! See `docs/protocol/v2-design.md` §6 for every message.
use super::{camera, control_turn, microphone};
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
    #[prost(oneof = "SessionBody", tags = "1,2,3,4,5,7,8,10,11,16,17,22,23,24,25,26,27,28")]
    pub(crate) body: Option<SessionBody>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub(crate) enum SessionBody {
    // ------------------------------------------------------------ pushes
    /// The host's displays as this participant sees them (H→C), whenever
    /// the list, a display or a track mapping changes.
    #[prost(message, tag = "1")]
    Displays(Displays),
    /// The stream as the host holds and sends it (H→C), whenever it
    /// changes; also the answer to `StreamRequest`.
    #[prost(message, tag = "2")]
    Stream(StreamState),
    /// Host virtual microphone state (H→C).
    #[prost(message, tag = "3")]
    Microphone(MicrophoneState),
    /// Whether the host accepts files on the clipboard (H→C).
    #[prost(message, tag = "4")]
    ClipboardFiles(ClipboardPermission),
    /// Request (C→H): start or stop capturing a display → [`Displays`].
    #[prost(message, tag = "5")]
    Capture(CaptureControl),
    // ---------------------------------------------- requests (C→H)
    /// A change of this participant's stream → [`StreamState`].
    #[prost(message, tag = "10")]
    StreamRequest(StreamRequest),
    /// A change of the host's displays → [`Displays`].
    #[prost(message, tag = "11")]
    Display(DisplayRequest),
    /// The remote microphone on or off → an empty answer.
    #[prost(message, tag = "16")]
    MicrophonePolicy(microphone::PolicyRequest),
    /// Take or release the control token → an empty answer.
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
    /// The host's cameras in detail (H→C push).
    #[prost(message, tag = "26")]
    Cameras(camera::Cameras),
    /// A participant's camera parameters or adjustment → an empty answer.
    #[prost(message, tag = "27")]
    CameraRequest(camera::CameraRequest),
    /// Hear the host's microphone or stop → an empty answer.
    #[prost(message, tag = "28")]
    Listen(camera::ListenControl),
}

impl SessionBody {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Displays(_) => "Displays",
            Self::Stream(_) => "Stream",
            Self::Microphone(_) => "Microphone",
            Self::ClipboardFiles(_) => "ClipboardFiles",
            Self::Capture(_) => "Capture",
            Self::StreamRequest(_) => "StreamRequest",
            Self::ReceiverStats(_) => "ReceiverStats",
            Self::VideoFeedback(_) => "VideoFeedback",
            Self::Forward(_) => "Forward",
            Self::ForwardStatus(_) => "ForwardStatus",
            Self::Cameras(_) => "Cameras",
            Self::CameraRequest(_) => "CameraRequest",
            Self::Listen(_) => "Listen",
            Self::Display(_) => "Display",
            Self::MicrophonePolicy(_) => "MicrophonePolicy",
            Self::ControlTurn(_) => "ControlTurn",
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

/// The answer to request `request_id` with nothing more to say, or its
/// refusal (code, reason).
pub(crate) fn answer(request_id: u32, result: std::result::Result<(), (&str, &str)>) -> Vec<u8> {
    let empty = SessionMessage { body: None };
    peer::answer(FrameType::Session, request_id, result.map(|()| &empty))
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
/// session (stop keeps the slots so that start(-1) resumes them). Errors:
/// [`display_reason`] `not_allowed`, `no_free_track`, `not_found`.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct CaptureControl {
    #[prost(int32, tag = "1")]
    pub(crate) screen_id: i32,
    #[prost(bool, tag = "2")]
    pub(crate) start: bool,
    /// Show the display on this video track, in place of what it shows (a
    /// viewer with one picture); absent: a free track.
    #[prost(int32, optional, tag = "3")]
    pub(crate) track: Option<i32>,
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

/// Video codecs (0: the host chooses).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum Codec {
    Unspecified = 0,
    H264 = 1,
    Hevc = 2,
    Av1 = 3,
}

/// Stream quality (`farwing_protocol::session::Quality`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum Quality {
    Auto = 0,
    Low = 1,
    Standard = 2,
    High = 3,
    Ultra = 4,
    Custom = 5,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum WeakNetwork {
    Bitrate = 0,
    FrameRate = 1,
}

/// The stream a participant asks for (`farwing_protocol::session::StreamSettings`).
#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct StreamSettings {
    #[prost(enumeration = "Quality", tag = "1")]
    pub(crate) quality: i32,
    #[prost(uint32, tag = "2")]
    pub(crate) bitrate_kbps: u32,
    #[prost(uint32, tag = "3")]
    pub(crate) fps: u32,
    #[prost(bool, tag = "4")]
    pub(crate) chroma444: bool,
    #[prost(bool, tag = "5")]
    pub(crate) hdr: bool,
    #[prost(enumeration = "Codec", tag = "6")]
    pub(crate) codec: i32,
    #[prost(bool, tag = "7")]
    pub(crate) cursor: bool,
    #[prost(enumeration = "WeakNetwork", tag = "8")]
    pub(crate) weak_network: i32,
    #[prost(bool, tag = "9")]
    pub(crate) cinema: bool,
}

/// A change of some of [`StreamSettings`]; absent fields stay. Refusals
/// carry `updating` or `invalid`.
#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct StreamRequest {
    #[prost(enumeration = "Quality", optional, tag = "1")]
    pub(crate) quality: Option<i32>,
    #[prost(uint32, optional, tag = "2")]
    pub(crate) bitrate_kbps: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub(crate) fps: Option<u32>,
    #[prost(bool, optional, tag = "4")]
    pub(crate) chroma444: Option<bool>,
    #[prost(bool, optional, tag = "5")]
    pub(crate) hdr: Option<bool>,
    #[prost(enumeration = "Codec", optional, tag = "6")]
    pub(crate) codec: Option<i32>,
    #[prost(bool, optional, tag = "7")]
    pub(crate) cursor: Option<bool>,
    #[prost(enumeration = "WeakNetwork", optional, tag = "8")]
    pub(crate) weak_network: Option<i32>,
    #[prost(bool, optional, tag = "9")]
    pub(crate) cinema: Option<bool>,
}

/// The stream as the host holds and sends it to one participant.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct StreamState {
    /// Grows with every change; older states are dropped.
    #[prost(uint64, tag = "1")]
    pub(crate) revision: u64,
    /// What the participant asked for, as the host holds it.
    #[prost(message, optional, tag = "2")]
    pub(crate) settings: Option<StreamSettings>,
    #[prost(message, optional, tag = "3")]
    pub(crate) limits: Option<StreamLimits>,
    /// What each of its video tracks carries.
    #[prost(message, repeated, tag = "4")]
    pub(crate) tracks: Vec<TrackState>,
}

/// What this host can send this participant.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct StreamLimits {
    #[prost(enumeration = "Codec", repeated, tag = "1")]
    pub(crate) codecs: Vec<i32>,
    #[prost(bool, tag = "2")]
    pub(crate) chroma444: bool,
    /// 10-bit encoding for HDR displays.
    #[prost(bool, tag = "3")]
    pub(crate) hdr: bool,
    /// The highest tier a decoder and an encoder share.
    #[prost(enumeration = "Quality", tag = "4")]
    pub(crate) max_quality: i32,
}

/// One video track as it is sent.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct TrackState {
    #[prost(uint32, tag = "1")]
    pub(crate) track: u32,
    #[prost(int32, tag = "2")]
    pub(crate) screen: i32,
    #[prost(enumeration = "Codec", tag = "3")]
    pub(crate) codec: i32,
    #[prost(bool, tag = "4")]
    pub(crate) chroma444: bool,
    #[prost(bool, tag = "5")]
    pub(crate) hdr: bool,
    #[prost(bool, tag = "6")]
    pub(crate) bit10: bool,
    /// The tier in effect (automatic resolved); [`Quality::Custom`] for a
    /// custom bitrate.
    #[prost(enumeration = "Quality", tag = "7")]
    pub(crate) quality: i32,
    #[prost(uint32, tag = "10")]
    pub(crate) fps: u32,
    #[prost(bool, tag = "11")]
    pub(crate) hardware: bool,
    /// The encoder and capture backends, for diagnostics.
    #[prost(string, tag = "12")]
    pub(crate) encoder: String,
    #[prost(string, tag = "13")]
    pub(crate) capture: String,
}

impl Codec {
    pub(crate) fn of(codec: Option<farwing_protocol::session::Codec>) -> Self {
        use farwing_protocol::session::Codec as C;
        match codec {
            None => Self::Unspecified,
            Some(C::H264) => Self::H264,
            Some(C::Hevc) => Self::Hevc,
            Some(C::Av1) => Self::Av1,
        }
    }
    pub(crate) fn session(self) -> Option<farwing_protocol::session::Codec> {
        use farwing_protocol::session::Codec as C;
        match self {
            Self::Unspecified => None,
            Self::H264 => Some(C::H264),
            Self::Hevc => Some(C::Hevc),
            Self::Av1 => Some(C::Av1),
        }
    }
}

impl Quality {
    pub(crate) fn of(quality: farwing_protocol::session::Quality) -> Self {
        use farwing_protocol::session::Quality as Q;
        match quality {
            Q::Auto => Self::Auto,
            Q::Low => Self::Low,
            Q::Standard => Self::Standard,
            Q::High => Self::High,
            Q::Ultra => Self::Ultra,
            Q::Custom => Self::Custom,
        }
    }
    pub(crate) fn session(self) -> farwing_protocol::session::Quality {
        use farwing_protocol::session::Quality as Q;
        match self {
            Self::Auto => Q::Auto,
            Self::Low => Q::Low,
            Self::Standard => Q::Standard,
            Self::High => Q::High,
            Self::Ultra => Q::Ultra,
            Self::Custom => Q::Custom,
        }
    }
}

impl From<farwing_protocol::session::StreamSettings> for StreamSettings {
    fn from(settings: farwing_protocol::session::StreamSettings) -> Self {
        use farwing_protocol::session::WeakNetwork as W;
        Self {
            quality: Quality::of(settings.quality) as i32,
            bitrate_kbps: settings.bitrate_kbps,
            fps: settings.fps,
            chroma444: settings.chroma444,
            hdr: settings.hdr,
            codec: Codec::of(settings.codec) as i32,
            cursor: settings.cursor,
            weak_network: match settings.weak_network {
                W::Bitrate => WeakNetwork::Bitrate,
                W::FrameRate => WeakNetwork::FrameRate,
            } as i32,
            cinema: settings.cinema,
        }
    }
}

impl From<StreamSettings> for farwing_protocol::session::StreamSettings {
    fn from(settings: StreamSettings) -> Self {
        use farwing_protocol::session::WeakNetwork as W;
        Self {
            quality: settings.quality().session(),
            bitrate_kbps: settings.bitrate_kbps,
            fps: settings.fps,
            chroma444: settings.chroma444,
            hdr: settings.hdr,
            codec: settings.codec().session(),
            cursor: settings.cursor,
            weak_network: match settings.weak_network() {
                WeakNetwork::Bitrate => W::Bitrate,
                WeakNetwork::FrameRate => W::FrameRate,
            },
            cinema: settings.cinema,
        }
    }
}

impl StreamRequest {
    /// The fields of `to` that differ from `from`.
    pub(crate) fn between(from: StreamSettings, to: StreamSettings) -> Self {
        fn changed<T: PartialEq>(from: T, to: T) -> Option<T> {
            (from != to).then_some(to)
        }
        Self {
            quality: changed(from.quality, to.quality),
            bitrate_kbps: changed(from.bitrate_kbps, to.bitrate_kbps),
            fps: changed(from.fps, to.fps),
            chroma444: changed(from.chroma444, to.chroma444),
            hdr: changed(from.hdr, to.hdr),
            codec: changed(from.codec, to.codec),
            cursor: changed(from.cursor, to.cursor),
            weak_network: changed(from.weak_network, to.weak_network),
            cinema: changed(from.cinema, to.cinema),
        }
    }
    pub(crate) fn is_empty(&self) -> bool {
        *self == Self::default()
    }
    /// `settings` with this change.
    pub(crate) fn apply(&self, mut settings: StreamSettings) -> StreamSettings {
        settings.quality = self.quality.unwrap_or(settings.quality);
        settings.bitrate_kbps = self.bitrate_kbps.unwrap_or(settings.bitrate_kbps);
        settings.fps = self.fps.unwrap_or(settings.fps);
        settings.chroma444 = self.chroma444.unwrap_or(settings.chroma444);
        settings.hdr = self.hdr.unwrap_or(settings.hdr);
        settings.codec = self.codec.unwrap_or(settings.codec);
        settings.cursor = self.cursor.unwrap_or(settings.cursor);
        settings.weak_network = self.weak_network.unwrap_or(settings.weak_network);
        settings.cinema = self.cinema.unwrap_or(settings.cinema);
        settings
    }
}

/// Why a `StreamRequest` was refused (`PeerError::reason`).
pub(crate) mod stream_reason {
    /// The host is installing an update.
    pub const UPDATING: &str = "updating";
    /// A value out of range.
    pub const INVALID: &str = "invalid";
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

/// What a display is ([`Display::kind`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum DisplayKind {
    Physical = 0,
    /// Added in this session; it can be removed.
    Virtual = 1,
    /// The machine's display while no monitor is usable.
    Fallback = 2,
    /// Another product's virtual display.
    Foreign = 3,
    /// The super screen: the only active display while it lasts.
    Exclusive = 4,
    /// A camera: a video source without desktop, input or display modes
    /// (`docs/protocol/camera-design.md` §4).
    Camera = 5,
}

/// The host's displays (push).
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Displays {
    /// Grows with every change.
    #[prost(uint64, tag = "1")]
    pub(crate) revision: u64,
    #[prost(message, repeated, tag = "2")]
    pub(crate) displays: Vec<Display>,
    /// The display this participant uses now; -1 none.
    #[prost(int32, tag = "3")]
    pub(crate) active: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Display {
    #[prost(int32, tag = "1")]
    pub(crate) id: i32,
    #[prost(enumeration = "DisplayKind", tag = "2")]
    pub(crate) kind: i32,
    /// The monitor's name; empty when Windows has none.
    #[prost(string, tag = "3")]
    pub(crate) name: String,
    #[prost(bool, tag = "4")]
    pub(crate) primary: bool,
    /// The current mode.
    #[prost(uint32, tag = "5")]
    pub(crate) width: u32,
    #[prost(uint32, tag = "6")]
    pub(crate) height: u32,
    /// 0 when unknown.
    #[prost(uint32, tag = "7")]
    pub(crate) refresh_hz: u32,
    /// The display outputs HDR10.
    #[prost(bool, tag = "8")]
    pub(crate) hdr: bool,
    #[prost(message, repeated, tag = "9")]
    pub(crate) modes: Vec<Size>,
    /// The driver's preferred (native panel) mode.
    #[prost(message, optional, tag = "10")]
    pub(crate) preferred: Option<Size>,
    /// The mode before this session changed it; absent when it did not.
    #[prost(message, optional, tag = "11")]
    pub(crate) initial: Option<Size>,
    /// Scaling in percent; 0 when unknown.
    #[prost(uint32, tag = "12")]
    pub(crate) scale: u32,
    #[prost(uint32, tag = "13")]
    pub(crate) recommended_scale: u32,
    #[prost(uint32, repeated, tag = "14")]
    pub(crate) scales: Vec<u32>,
    /// This participant's video track of the display; -1 none.
    #[prost(int32, tag = "15")]
    pub(crate) track: i32,
    /// Show the picture mirrored left to right (cameras).
    #[prost(bool, tag = "16")]
    pub(crate) mirror: bool,
    /// Then rotated clockwise by this many quarter turns (cameras).
    #[prost(uint32, tag = "17")]
    pub(crate) quarter_turns: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct Size {
    #[prost(uint32, tag = "1")]
    pub(crate) width: u32,
    #[prost(uint32, tag = "2")]
    pub(crate) height: u32,
}

/// A change of the host's displays. Refusals carry a [`display_reason`].
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct DisplayRequest {
    #[prost(oneof = "DisplayOp", tags = "1,2,3,4,5,6")]
    pub(crate) op: Option<DisplayOp>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub(crate) enum DisplayOp {
    /// One of the display's modes; a virtual display takes any size.
    #[prost(message, tag = "1")]
    SetMode(SetMode),
    #[prost(message, tag = "2")]
    SetScale(SetScale),
    /// A virtual display of this size and scale (0: the primary's).
    #[prost(message, tag = "3")]
    AddVirtual(AddVirtual),
    /// Remove a virtual display.
    #[prost(message, tag = "4")]
    Remove(RemoveDisplay),
    /// Replace the layout with one super screen of this size and scale.
    #[prost(message, tag = "5")]
    EnterExclusive(AddVirtual),
    /// Restore the layout from before the super screen.
    #[prost(message, tag = "6")]
    ExitExclusive(ExitExclusive),
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct SetMode {
    #[prost(int32, tag = "1")]
    pub(crate) id: i32,
    #[prost(uint32, tag = "2")]
    pub(crate) width: u32,
    #[prost(uint32, tag = "3")]
    pub(crate) height: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct SetScale {
    #[prost(int32, tag = "1")]
    pub(crate) id: i32,
    #[prost(uint32, tag = "2")]
    pub(crate) scale: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct AddVirtual {
    #[prost(uint32, tag = "1")]
    pub(crate) width: u32,
    #[prost(uint32, tag = "2")]
    pub(crate) height: u32,
    #[prost(uint32, tag = "3")]
    pub(crate) scale: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct RemoveDisplay {
    #[prost(int32, tag = "1")]
    pub(crate) id: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct ExitExclusive {}

/// Why a `Capture` or `Display` request was refused (`PeerError::reason`).
pub(crate) mod display_reason {
    /// The participant may not change the host's displays (or watch).
    pub const NOT_ALLOWED: &str = "not_allowed";
    /// The host is installing an update.
    pub const UPDATING: &str = "updating";
    /// The display has no such mode; a super screen can show it.
    pub const UNSUPPORTED_MODE: &str = "unsupported_mode";
    /// Every video track already shows a display.
    pub const NO_FREE_TRACK: &str = "no_free_track";
    /// No more displays can be added.
    pub const LIMIT: &str = "limit";
    /// Only virtual displays added in a session can be removed.
    pub const NOT_REMOVABLE: &str = "not_removable";
    /// Not while the super screen is in use.
    pub const EXCLUSIVE: &str = "exclusive";
    pub const NOT_FOUND: &str = "not_found";
    pub const INVALID: &str = "invalid";
    /// The participant may not see the host's cameras (not granted, or the
    /// owner does not allow it).
    pub const CAMERA_NOT_ALLOWED: &str = "camera_not_allowed";
    /// The participant may not hear the host's microphone.
    pub const LISTEN_NOT_ALLOWED: &str = "listen_not_allowed";
    /// Anything else; the host logs why.
    pub const FAILED: &str = "failed";
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


#[cfg(test)]
mod stream_tests {
    use super::*;
    use farwing_protocol::session;

    #[test]
    fn a_request_carries_what_changed_and_applies_back() {
        let before = session::StreamSettings {
            quality: session::Quality::Auto,
            fps: 60,
            codec: Some(session::Codec::Av1),
            cursor: true,
            ..Default::default()
        };
        let after = session::StreamSettings {
            quality: session::Quality::Custom,
            bitrate_kbps: 30_000,
            cursor: false,
            ..before
        };
        let request = StreamRequest::between(before.into(), after.into());
        assert_eq!(request.quality, Some(Quality::Custom as i32));
        assert_eq!(request.bitrate_kbps, Some(30_000));
        assert_eq!(request.cursor, Some(false));
        assert_eq!((request.fps, request.codec, request.hdr), (None, None, None));
        let applied: session::StreamSettings = request.apply(before.into()).into();
        assert_eq!(applied, after);
        assert!(StreamRequest::between(after.into(), after.into()).is_empty());
    }
}
