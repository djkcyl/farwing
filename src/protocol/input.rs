//! Remote input events (peer frame type `INPUT`, controller → host), Input v2
//! of `docs/protocol/v2-design.md` §7.
//!
//! Keys are HID usages `(page << 16) | id` of the physical key (keyboard page
//! 0x07, consumer page 0x0C), absolute positions are normalized to
//! `0.0..=1.0` of the target screen, relative motion is in host pixels, wheel
//! deltas are in 1/120 of a notch and mouse buttons use the masks 1 (left),
//! 2 (right), 16 (middle), 32 (X1) and 64 (X2).

use anyhow::{Context as _, Result};
use prost::Message as _;

use crate::protocol::peer::{self, FrameType};

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct InputMessage {
    #[prost(oneof = "InputKind", tags = "1, 2, 3, 4, 5, 6, 7, 8")]
    pub kind: Option<InputKind>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub(crate) enum InputKind {
    #[prost(message, tag = "1")]
    Key(Key),
    #[prost(message, tag = "2")]
    LockState(LockState),
    #[prost(message, tag = "3")]
    MouseAbsolute(MouseAbsolute),
    #[prost(message, tag = "4")]
    MouseRelative(MouseRelative),
    #[prost(message, tag = "5")]
    MouseButton(MouseButton),
    #[prost(message, tag = "6")]
    MouseWheel(MouseWheel),
    #[prost(enumeration = "SystemCommand", tag = "7")]
    SystemCommand(i32),
    /// Keep-alive while keys are held; any INPUT frame also counts.
    #[prost(message, tag = "8")]
    Heartbeat(Heartbeat),
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Key {
    /// HID usage `(page << 16) | id`.
    #[prost(uint32, tag = "1")]
    pub usage: u32,
    #[prost(bool, tag = "2")]
    pub down: bool,
    /// Auto-repeat of a key that is already down.
    #[prost(bool, tag = "3")]
    pub repeat: bool,
}

/// The controller's local lock state. `seq` is the number of Caps/Num/Scroll
/// Lock presses the controller sent before this message in the same stream.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct LockState {
    #[prost(bool, tag = "1")]
    pub caps: bool,
    #[prost(bool, tag = "2")]
    pub num: bool,
    #[prost(bool, tag = "3")]
    pub scroll: bool,
    #[prost(uint32, tag = "4")]
    pub seq: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct MouseAbsolute {
    #[prost(int32, optional, tag = "1")]
    pub screen_id: Option<i32>,
    #[prost(double, tag = "2")]
    pub x: f64,
    #[prost(double, tag = "3")]
    pub y: f64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct MouseRelative {
    #[prost(sint32, tag = "1")]
    pub dx: i32,
    #[prost(sint32, tag = "2")]
    pub dy: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct MouseButton {
    #[prost(uint32, tag = "1")]
    pub button: u32,
    #[prost(bool, tag = "2")]
    pub down: bool,
}

/// Both axes in one message, in 1/120 of a notch.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct MouseWheel {
    #[prost(sint32, tag = "1")]
    pub dx: i32,
    #[prost(sint32, tag = "2")]
    pub dy: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Heartbeat {}

/// Explicit viewer commands; never derived from the key stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub(crate) enum SystemCommand {
    Unspecified = 0,
    /// Ctrl+Alt+Del (secure attention sequence); needs the host service.
    Sas = 1,
    Lock = 2,
    TaskManager = 3,
    ShowDesktop = 4,
    TaskView = 5,
}

pub(crate) fn encode(kind: InputKind) -> Vec<u8> {
    peer::message(FrameType::Input, &InputMessage { kind: Some(kind) })
}

/// Decode an `INPUT` frame; `None` for other frames.
pub(crate) fn decode(bytes: &[u8]) -> Result<Option<InputMessage>> {
    peer::body_of(FrameType::Input, bytes)
        .map(|body| InputMessage::decode(body).context("输入消息无效"))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(kind: InputKind) {
        let bytes = encode(kind.clone());
        assert_eq!(decode(&bytes).unwrap().unwrap().kind, Some(kind));
    }

    #[test]
    fn every_kind_round_trips() {
        round_trip(InputKind::Key(Key {
            usage: 0x0007_0004,
            down: true,
            repeat: true,
        }));
        round_trip(InputKind::LockState(LockState {
            caps: true,
            num: false,
            scroll: true,
            seq: u32::MAX,
        }));
        round_trip(InputKind::MouseAbsolute(MouseAbsolute {
            screen_id: Some(2),
            x: 0.25,
            y: 1.0,
        }));
        round_trip(InputKind::MouseRelative(MouseRelative { dx: -5, dy: 7 }));
        round_trip(InputKind::MouseButton(MouseButton {
            button: 16,
            down: false,
        }));
        round_trip(InputKind::MouseWheel(MouseWheel { dx: -120, dy: 60 }));
        round_trip(InputKind::SystemCommand(SystemCommand::TaskView as i32));
        round_trip(InputKind::Heartbeat(Heartbeat {}));
    }

    #[test]
    fn empty_heartbeat_is_still_a_heartbeat() {
        let bytes = encode(InputKind::Heartbeat(Heartbeat {}));
        let message = decode(&bytes).unwrap().unwrap();
        assert!(matches!(message.kind, Some(InputKind::Heartbeat(_))));
    }

    #[test]
    fn other_frames_are_not_input() {
        let bytes = peer::message(FrameType::Cursor, &Heartbeat {});
        assert!(decode(&bytes).unwrap().is_none());
    }
}
