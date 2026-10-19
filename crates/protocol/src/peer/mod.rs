//! Farwing Peer Protocol (FPP) v2: the frame layer on WebRTC data channels
//! (`docs/protocol/v2-design.md` §5).
//!
//! ```text
//! 0      2       3       4        6               10
//! +------+-------+-------+--------+---------------+---------+
//! | "FW" | ver=2 | flags | type   | request id    | body    |
//! +------+-------+-------+--------+---------------+---------+
//! ```
//!
//! A request carries a non-zero request ID chosen by its sender; the reply
//! repeats it with `RESPONSE` set (and `ERROR` with a [`PeerError`] body on
//! failure). Pushes carry request ID 0.

use std::collections::BTreeMap;

pub const MAGIC: [u8; 2] = *b"FW";
pub const VERSION: u8 = 2;
pub const HEADER_LEN: usize = 10;

/// Data channel labels.
pub mod channel {
    pub const CONTROL: &str = "farwing.control.v2";
    pub const SESSION: &str = "farwing.session.v2";
    /// `TRANSFER` and `FLOW` of files, clipboard, drag and diagnostics.
    pub const BULK: &str = "farwing.bulk.v2";
    /// Port forwarding: stream control and TCP data, ordered and reliable.
    pub const TUNNEL: &str = "farwing.tunnel.v2";
    /// Port forwarding: UDP datagrams, unordered without retransmission.
    pub const DGRAM: &str = "farwing.dgram.v2";
    pub const ALL: [&str; 5] = [CONTROL, SESSION, BULK, TUNNEL, DGRAM];
}

/// Frame flags.
pub mod flags {
    pub const RESPONSE: u8 = 0x01;
    pub const ERROR: u8 = 0x02;
    /// More fragments of this message follow.
    pub const MORE: u8 = 0x04;
}

/// In-session feature names declared in [`Hello::features`] (name → version).
/// A peer only declares what it can do at runtime.
pub mod feature {
    pub const VIDEO_H264: &str = "video.h264";
    pub const VIDEO_HEVC: &str = "video.hevc";
    pub const VIDEO_AV1: &str = "video.av1";
    /// Full-chroma (4:4:4) video.
    pub const VIDEO_444: &str = "video.444";
    pub const VIDEO_HDR: &str = "video.hdr";
    /// Resolution, DPI and display-step requests.
    pub const DISPLAY_CONFIG: &str = "display.config";
    pub const DISPLAY_VIRTUAL: &str = "display.virtual";
    /// Exclusive (super) screen.
    pub const DISPLAY_EXCLUSIVE: &str = "display.exclusive";
    /// Driver-preferred panel mode in the screen list.
    pub const DISPLAY_GEOMETRY: &str = "display.geometry";
    pub const AUDIO_QUALITY: &str = "audio.quality";
    pub const AUDIO_MICROPHONE: &str = "audio.microphone";
    pub const INPUT_KEYBOARD_USAGE: &str = "input.keyboard_usage";
    pub const INPUT_POINTER: &str = "input.pointer";
    pub const CURSOR_SHAPE: &str = "cursor.shape";
    pub const CURSOR_PARTICIPANTS: &str = "cursor.participants";
    pub const CLIPBOARD: &str = "clipboard";
    pub const CLIPBOARD_FILES: &str = "clipboard.files";
    pub const FILES: &str = "files";
    pub const DRAG: &str = "drag";
    pub const ANNOTATION: &str = "annotation";
    pub const PORTS_TCP: &str = "ports.tcp";
    pub const PORTS_UDP: &str = "ports.udp";
    pub const DIAGNOSTICS: &str = "diagnostics";
    pub const PERMISSIONS: &str = "permissions";
    pub const PARTICIPANTS: &str = "participants";
    pub const HOST_EVENT: &str = "host_event";
    pub const FCT: &str = "fct";
    /// Loss recovery with long-term references: `frame-meta` RTP extension
    /// and SESSION `VideoFeedback` (`docs/protocol/rfi-ltr-brief.md`).
    pub const VIDEO_RECOVERY: &str = "video.recovery";
    /// Reserved for mobile controllers.
    pub const TOUCH: &str = "touch";
    pub const PEN: &str = "pen";
}

/// Frame types.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[repr(u16)]
pub enum FrameType {
    Hello = 0x0001,
    Error = 0x0002,
    Input = 0x0101,
    PointerPolicy = 0x0102,
    Cursor = 0x0103,
    /// Reserved: touch input of mobile controllers.
    Touch = 0x0104,
    /// Reserved: pen input.
    Pen = 0x0105,
    /// A participant's pointer (indicator or controller cursor).
    Pointer = 0x0106,
    Session = 0x0201,
    Audio = 0x0202,
    HostEvent = 0x0203,
    Clipboard = 0x0301,
    File = 0x0401,
    /// Unified bulk data (`docs/protocol/v2-design.md` §8).
    Transfer = 0x0402,
    Drag = 0x0501,
    Annotation = 0x0601,
    Port = 0x0701,
    PortDatagram = 0x0702,
    Diagnostics = 0x0801,
    Permissions = 0x0901,
    Participants = 0x0902,
    Flow = 0x0A01,
}

impl FrameType {
    pub fn from_u16(value: u16) -> Option<Self> {
        Some(match value {
            0x0001 => Self::Hello,
            0x0002 => Self::Error,
            0x0101 => Self::Input,
            0x0102 => Self::PointerPolicy,
            0x0103 => Self::Cursor,
            0x0104 => Self::Touch,
            0x0105 => Self::Pen,
            0x0106 => Self::Pointer,
            0x0201 => Self::Session,
            0x0202 => Self::Audio,
            0x0203 => Self::HostEvent,
            0x0301 => Self::Clipboard,
            0x0401 => Self::File,
            0x0402 => Self::Transfer,
            0x0501 => Self::Drag,
            0x0601 => Self::Annotation,
            0x0701 => Self::Port,
            0x0702 => Self::PortDatagram,
            0x0801 => Self::Diagnostics,
            0x0901 => Self::Permissions,
            0x0902 => Self::Participants,
            0x0A01 => Self::Flow,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Hello => "HELLO",
            Self::Error => "ERROR",
            Self::Input => "INPUT",
            Self::PointerPolicy => "POINTER_POLICY",
            Self::Cursor => "CURSOR",
            Self::Touch => "TOUCH",
            Self::Pen => "PEN",
            Self::Pointer => "POINTER",
            Self::Session => "SESSION",
            Self::Audio => "AUDIO",
            Self::HostEvent => "HOST_EVENT",
            Self::Clipboard => "CLIPBOARD",
            Self::File => "FILE",
            Self::Transfer => "TRANSFER",
            Self::Drag => "DRAG",
            Self::Annotation => "ANNOTATION",
            Self::Port => "PORT",
            Self::PortDatagram => "PORT_DGRAM",
            Self::Diagnostics => "DIAGNOSTICS",
            Self::Permissions => "PERMISSIONS",
            Self::Participants => "PARTICIPANTS",
            Self::Flow => "FLOW",
        }
    }
}

/// A decoded frame header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    pub flags: u8,
    /// Raw type; use [`FrameType::from_u16`] to classify.
    pub kind: u16,
    pub request_id: u32,
}

impl Header {
    pub fn frame_type(&self) -> Option<FrameType> {
        FrameType::from_u16(self.kind)
    }
    pub fn is_response(&self) -> bool {
        self.flags & flags::RESPONSE != 0
    }
    pub fn is_error(&self) -> bool {
        self.flags & flags::ERROR != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameError {
    TooShort,
    BadMagic,
    UnsupportedVersion(u8),
    TooLarge(usize),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => f.write_str("peer frame shorter than its header"),
            Self::BadMagic => f.write_str("not a Farwing peer frame"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported peer frame version {v}"),
            Self::TooLarge(n) => write!(f, "peer frame of {n} bytes exceeds the limit"),
        }
    }
}

impl std::error::Error for FrameError {}

/// Whether `bytes` starts with a Farwing frame header (cheap check).
pub fn is_frame(bytes: &[u8]) -> bool {
    bytes.len() >= HEADER_LEN && bytes[..2] == MAGIC
}

/// Encode a frame.
pub fn encode(kind: FrameType, flags: u8, request_id: u32, body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HEADER_LEN + body.len());
    write_header(&mut frame, kind as u16, flags, request_id);
    frame.extend_from_slice(body);
    frame
}

/// Encode only a header (for callers that stream the body).
pub fn write_header(output: &mut Vec<u8>, kind: u16, flags: u8, request_id: u32) {
    output.extend_from_slice(&MAGIC);
    output.push(VERSION);
    output.push(flags);
    output.extend_from_slice(&kind.to_be_bytes());
    output.extend_from_slice(&request_id.to_be_bytes());
}

/// Decode a frame into its header and body.
pub fn decode(bytes: &[u8]) -> Result<(Header, &[u8]), FrameError> {
    if bytes.len() > crate::limits::MAX_PEER_FRAME {
        return Err(FrameError::TooLarge(bytes.len()));
    }
    if bytes.len() < HEADER_LEN {
        return Err(FrameError::TooShort);
    }
    if bytes[..2] != MAGIC {
        return Err(FrameError::BadMagic);
    }
    if bytes[2] != VERSION {
        return Err(FrameError::UnsupportedVersion(bytes[2]));
    }
    let header = Header {
        flags: bytes[3],
        kind: u16::from_be_bytes([bytes[4], bytes[5]]),
        request_id: u32::from_be_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]),
    };
    Ok((header, &bytes[HEADER_LEN..]))
}

/// Peer role in [`Hello`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum Role {
    Unspecified = 0,
    Controller = 1,
    Host = 2,
}

/// How a participant is shown to others (cursor label, participant list).
#[derive(Clone, PartialEq, prost::Message)]
pub struct Profile {
    #[prost(string, tag = "1")]
    pub nickname: String,
    /// Preferred cursor colour as 0xRRGGBB; 0 lets the host choose.
    #[prost(uint32, tag = "2")]
    pub color: u32,
}

/// Handshake message (frame type `HELLO`): the controller sends it as a
/// request once the session channel opens; the host answers with its own.
#[derive(Clone, PartialEq, prost::Message)]
pub struct Hello {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(uint32, tag = "2")]
    pub min_protocol_version: u32,
    #[prost(enumeration = "Role", tag = "3")]
    pub role: i32,
    #[prost(string, tag = "4")]
    pub client_version: String,
    /// Feature name → version. See [`feature`].
    #[prost(btree_map = "string, uint32", tag = "5")]
    pub features: BTreeMap<String, u32>,
    #[prost(uint32, tag = "6")]
    pub max_message_bytes: u32,
    /// [`crate::Platform`] name.
    #[prost(string, tag = "7")]
    pub platform: String,
    #[prost(message, optional, tag = "8")]
    pub profile: Option<Profile>,
}

impl Hello {
    pub fn new(role: Role, client_version: &str, features: BTreeMap<String, u32>) -> Self {
        Self {
            protocol_version: crate::PROTOCOL_VERSION,
            min_protocol_version: crate::MIN_PROTOCOL_VERSION,
            role: role as i32,
            client_version: client_version.to_owned(),
            features,
            max_message_bytes: crate::limits::MAX_PEER_FRAME as u32,
            platform: String::new(),
            profile: None,
        }
    }

    /// Version of a feature, 0 when undeclared.
    pub fn level(&self, feature: &str) -> u32 {
        self.features.get(feature).copied().unwrap_or(0)
    }

    /// Whether the peer declared `feature` (any version).
    pub fn has(&self, feature: &str) -> bool {
        self.level(feature) > 0
    }

    /// Whether this build and the peer share a protocol version.
    pub fn compatible(&self) -> bool {
        self.protocol_version >= crate::MIN_PROTOCOL_VERSION
            && self.min_protocol_version <= crate::PROTOCOL_VERSION
    }
}

/// Error body of a response with the `ERROR` flag.
#[derive(Clone, PartialEq, prost::Message)]
pub struct PeerError {
    /// One of [`error_code`].
    #[prost(string, tag = "1")]
    pub code: String,
    #[prost(string, tag = "2")]
    pub message: String,
    /// Optional machine-readable detail (JSON).
    #[prost(string, tag = "3")]
    pub detail: String,
}

/// [`PeerError::code`] values.
pub mod error_code {
    pub const UNSUPPORTED: &str = "unsupported";
    pub const BAD_REQUEST: &str = "bad_request";
    pub const FORBIDDEN: &str = "forbidden";
    pub const NOT_FOUND: &str = "not_found";
    pub const BUSY: &str = "busy";
    pub const UNAVAILABLE: &str = "unavailable";
    pub const INTERNAL: &str = "internal";
    pub const TIMEOUT: &str = "timeout";
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    #[test]
    fn frame_round_trip() {
        let hello = Hello::new(Role::Controller, "1.0.0", BTreeMap::from([("files".into(), 2)]));
        let body = hello.encode_to_vec();
        let frame = encode(FrameType::Hello, flags::RESPONSE, 7, &body);
        assert!(is_frame(&frame));
        let (header, payload) = decode(&frame).unwrap();
        assert_eq!(header.frame_type(), Some(FrameType::Hello));
        assert!(header.is_response());
        assert_eq!(header.request_id, 7);
        let decoded = Hello::decode(payload).unwrap();
        assert_eq!(decoded.level("files"), 2);
        assert!(decoded.compatible());
        assert_eq!(decode(b"XX\x02\x00\x00\x01\x00\x00\x00\x00"), Err(FrameError::BadMagic));
        assert_eq!(
            decode(b"FW\x01\x00\x00\x01\x00\x00\x00\x00"),
            Err(FrameError::UnsupportedVersion(1))
        );
    }
}
