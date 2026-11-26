//! `TERM` 帧与被控端终端列表的公共格式。
//!
//! 被控端和主控端各写一份解析器很容易慢慢走偏；帧格式只从抓包和官方
//! 编码器确认过一次，两边共用这里的定义，谁改了都会同时影响另一侧。

use serde::{Deserialize, Serialize};

/// The four bytes every terminal frame starts with.
pub(crate) const MAGIC: &[u8; 4] = b"TERM";
/// Fixed header size, magic included.
pub(crate) const HEADER: usize = 16;
/// Only version 1 has ever been seen; refuse anything else rather than guess.
pub(crate) const VERSION: u8 = 1;

/// Frame kinds, in the declaration order the shipped enum uses.
///
/// The wire value is `index + 1`, so `open` is 1 and `detach` is 16. The
/// encoder adds one; both binaries agree on the resulting numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameType {
    Open = 1,
    Stdin = 2,
    Resize = 3,
    Close = 4,
    Stdout = 5,
    Exit = 6,
    Signal = 7,
    Error = 8,
    ListSessionsReq = 9,
    ListSessionsResp = 10,
    KillSessionReq = 11,
    KillSessionResp = 12,
    OpenResp = 13,
    UnlockReq = 14,
    UnlockResp = 15,
    Detach = 16,
}

impl FrameType {
    pub(crate) fn from_wire(byte: u8) -> Option<Self> {
        Some(match byte {
            1 => Self::Open,
            2 => Self::Stdin,
            3 => Self::Resize,
            4 => Self::Close,
            5 => Self::Stdout,
            6 => Self::Exit,
            7 => Self::Signal,
            8 => Self::Error,
            9 => Self::ListSessionsReq,
            10 => Self::ListSessionsResp,
            11 => Self::KillSessionReq,
            12 => Self::KillSessionResp,
            13 => Self::OpenResp,
            14 => Self::UnlockReq,
            15 => Self::UnlockResp,
            16 => Self::Detach,
            _ => return None,
        })
    }

    pub(crate) fn wire(self) -> u8 {
        self as u8
    }
}

/// One decoded `TERM` frame.
pub(crate) struct Frame {
    pub(crate) kind: FrameType,
    pub(crate) session: u32,
    pub(crate) payload: Vec<u8>,
}

impl Frame {
    /// Decodes a frame, or returns `None` for protobuf or malformed data.
    ///
    /// A frame is never abandoned halfway: the declared payload length has to
    /// fit, otherwise a truncated read would be mistaken for a short payload.
    pub(crate) fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < HEADER || &bytes[..4] != MAGIC || bytes[4] != VERSION {
            return None;
        }
        let kind = FrameType::from_wire(bytes[5])?;
        let session = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
        let length = u32::from_le_bytes(bytes[12..16].try_into().ok()?) as usize;
        let payload = bytes.get(HEADER..HEADER.checked_add(length)?)?.to_vec();
        Some(Self {
            kind,
            session,
            payload,
        })
    }

    /// Builds a frame the other side will accept.
    pub(crate) fn encode(kind: FrameType, session: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + payload.len());
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.push(kind.wire());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&session.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }
}

/// 被控端 `listSessionsResp` 里的一项。字段名跟着主机端 JSON 走，缺省的
/// 字段保留空值，旧版本少报一点不影响列表显示。
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct SessionInfo {
    #[serde(default, alias = "sessionId")]
    pub(crate) session_id: u32,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) state: String,
    #[serde(default)]
    pub(crate) shell: String,
    #[serde(default, alias = "createdAtMs")]
    pub(crate) created_at_ms: i64,
    #[serde(default, alias = "lastActiveMs")]
    pub(crate) last_active_ms: i64,
}

impl SessionInfo {
    pub(crate) fn state_label(&self) -> &'static str {
        match self.state.as_str() {
            "running" => "运行中",
            "detached" => "已分离",
            "exited" => "已退出",
            _ => "未知",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips() {
        let encoded = Frame::encode(FrameType::ListSessionsResp, 7, b"hello");
        assert_eq!(&encoded[..4], MAGIC);
        assert_eq!(encoded[4], VERSION);
        assert_eq!(encoded[5], 10);
        let frame = Frame::parse(&encoded).expect("parse");
        assert_eq!(frame.kind, FrameType::ListSessionsResp);
        assert_eq!(frame.session, 7);
        assert_eq!(frame.payload, b"hello");
    }

    #[test]
    fn the_captured_list_request_decodes() {
        // The phone's first terminal frame, byte for byte.
        let captured = [
            0x54, 0x45, 0x52, 0x4d, 0x01, 0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ];
        let frame = Frame::parse(&captured).expect("parse");
        assert_eq!(frame.kind, FrameType::ListSessionsReq);
        assert_eq!(frame.session, 0);
        assert!(frame.payload.is_empty());
    }

    #[test]
    fn protobuf_is_not_mistaken_for_a_frame() {
        // The check_env envelope the phone sends first.
        let protobuf = [
            0x08, 0x05, 0x10, 0x8a, 0x86, 0xb6, 0xed, 0x93, 0x34, 0xaa, 0x01, 0x07, 0x0a, 0x02,
            0x08, 0x01, 0xe2, 0x01, 0x00,
        ];
        assert!(Frame::parse(&protobuf).is_none());
    }

    #[test]
    fn a_truncated_frame_is_refused() {
        let mut encoded = Frame::encode(FrameType::Stdout, 1, b"payload");
        encoded.truncate(HEADER + 2);
        assert!(Frame::parse(&encoded).is_none());
    }

    #[test]
    fn session_fields_tolerate_a_short_response() {
        let session: SessionInfo = serde_json::from_str(r#"{"session_id":3,"name":"x"}"#).unwrap();
        assert_eq!(session.session_id, 3);
        assert_eq!(session.name, "x");
        assert_eq!(session.state, "");
    }
}
