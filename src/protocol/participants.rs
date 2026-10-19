//! Multi-participant frames (`docs/protocol/v2-design.md` §4.2–4.3):
//! `POINTER` (participant → host, control channel), `PARTICIPANTS`
//! (host → participant, session channel) and the other participants'
//! pointers carried in `CURSOR` (host → participant, control channel).

use crate::protocol::peer::{self, FrameType};
use anyhow::{Result, ensure};
use prost::Message as _;

/// Most participants a host lists (the server admits at most 16).
pub(crate) const MAX_PARTICIPANTS: usize = 16;
const MAX_TEXT: usize = 64;

/// A participant's pointer over the shared screens. Viewers send it too, as
/// a pointing aid; it never injects input.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Pointer {
    /// Increments with every frame the participant sends.
    #[prost(uint32, tag = "1")]
    pub seq: u32,
    #[prost(int32, tag = "2")]
    pub screen_id: i32,
    /// Position within the screen, normalized to 0..=1.
    #[prost(float, tag = "3")]
    pub x: f32,
    #[prost(float, tag = "4")]
    pub y: f32,
    /// False once the pointer left the remote picture.
    #[prost(bool, tag = "5")]
    pub visible: bool,
    #[prost(message, optional, tag = "6")]
    pub click: Option<Click>,
}

#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct Click {
    /// One of [`button`].
    #[prost(int32, tag = "1")]
    pub button: i32,
    /// One of [`phase`].
    #[prost(int32, tag = "2")]
    pub phase: i32,
}

pub(crate) mod button {
    pub const LEFT: i32 = 1;
    pub const RIGHT: i32 = 2;
    pub const MIDDLE: i32 = 3;
}

pub(crate) mod phase {
    pub const DOWN: i32 = 1;
    pub const UP: i32 = 2;
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Participant {
    /// Stable for the participant's session on this host.
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(string, tag = "2")]
    pub nickname: String,
    #[prost(string, tag = "3")]
    pub device_name: String,
    /// `own`, `friend` or `assist`.
    #[prost(string, tag = "4")]
    pub relation: String,
    /// Cursor and label colour, 0xRRGGBB.
    #[prost(fixed32, tag = "5")]
    pub color: u32,
    /// Holds the `control` capability.
    #[prost(bool, tag = "6")]
    pub control: bool,
    /// The receiving participant itself.
    #[prost(bool, tag = "7")]
    pub you: bool,
    /// Unix time the participant joined, milliseconds.
    #[prost(int64, tag = "8")]
    pub joined_ms: i64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Participants {
    #[prost(message, repeated, tag = "1")]
    pub participants: Vec<Participant>,
    /// The participant whose keyboard and mouse the host applies now; 0 when
    /// nobody holds the turn.
    #[prost(uint64, tag = "2")]
    pub controlling: u64,
}

/// Another participant's pointer as forwarded by the host.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct ParticipantPointer {
    #[prost(uint64, tag = "1")]
    pub id: u64,
    #[prost(int32, tag = "2")]
    pub screen_id: i32,
    #[prost(float, tag = "3")]
    pub x: f32,
    #[prost(float, tag = "4")]
    pub y: f32,
    #[prost(bool, tag = "5")]
    pub visible: bool,
    /// Time since the pointer last moved; receivers fade it after 3 s.
    #[prost(uint32, tag = "6")]
    pub idle_ms: u32,
    /// The latest click, repeated for a short while; a new `click_seq`
    /// marks a new click.
    #[prost(message, optional, tag = "7")]
    pub click: Option<Click>,
    #[prost(uint32, tag = "8")]
    pub click_seq: u32,
}

pub(crate) fn encode_pointer(pointer: &Pointer) -> Vec<u8> {
    peer::message(FrameType::Pointer, pointer)
}

/// A `POINTER` body with its values checked.
pub(crate) fn decode_pointer(body: &[u8]) -> Result<Pointer> {
    let pointer = Pointer::decode(body)?;
    ensure!(
        pointer.x.is_finite()
            && pointer.y.is_finite()
            && (0.0..=1.0).contains(&pointer.x)
            && (0.0..=1.0).contains(&pointer.y),
        "参与者指针坐标无效"
    );
    if let Some(click) = pointer.click {
        ensure!(
            (button::LEFT..=button::MIDDLE).contains(&click.button)
                && matches!(click.phase, phase::DOWN | phase::UP),
            "参与者点击无效"
        );
    }
    Ok(pointer)
}

pub(crate) fn encode_participants(participants: &Participants) -> Vec<u8> {
    peer::message(FrameType::Participants, participants)
}

pub(crate) fn decode_participants(body: &[u8]) -> Result<Participants> {
    let mut participants = Participants::decode(body)?;
    ensure!(
        participants.participants.len() <= MAX_PARTICIPANTS,
        "参与者列表过长"
    );
    for participant in &mut participants.participants {
        truncate(&mut participant.nickname);
        truncate(&mut participant.device_name);
        truncate(&mut participant.relation);
    }
    Ok(participants)
}

fn truncate(text: &mut String) {
    if let Some((at, _)) = text.char_indices().nth(MAX_TEXT) {
        text.truncate(at);
    }
}

/// Palette for participants without a colour of their own, in join order.
pub(crate) const PALETTE: [u32; 8] = [
    0x3B82F6, 0xF97316, 0x22C55E, 0xA855F7, 0xEC4899, 0x14B8A6, 0xEAB308, 0xEF4444,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_round_trip_and_validation() {
        let pointer = Pointer {
            seq: 3,
            screen_id: 1,
            x: 0.5,
            y: 0.25,
            visible: true,
            click: Some(Click {
                button: button::RIGHT,
                phase: phase::DOWN,
            }),
        };
        let bytes = encode_pointer(&pointer);
        let body = peer::body_of(FrameType::Pointer, &bytes).unwrap();
        assert_eq!(decode_pointer(body).unwrap(), pointer);
        let outside = Pointer { x: 1.5, ..pointer };
        assert!(decode_pointer(&outside.encode_to_vec()).is_err());
    }

    #[test]
    fn participants_are_bounded() {
        let many = Participants {
            participants: vec![Participant::default(); MAX_PARTICIPANTS + 1],
            controlling: 0,
        };
        assert!(decode_participants(&many.encode_to_vec()).is_err());
        let long = Participants {
            participants: vec![Participant {
                nickname: "名".repeat(100),
                ..Default::default()
            }],
            controlling: 0,
        };
        let decoded = decode_participants(&long.encode_to_vec()).unwrap();
        assert_eq!(decoded.participants[0].nickname.chars().count(), MAX_TEXT);
    }
}
