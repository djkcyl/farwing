//! Touch and pen input (peer frame types `TOUCH` 0x0104 and `PEN` 0x0105,
//! controller → host on the control channel; `docs/protocol/touch-pen-brief.md`
//! §5).
//!
//! Positions are normalized to `0.0..=1.0` of the target screen like
//! `MouseAbsolute`, and `screen_id` is always written. A TOUCH frame lists
//! every active contact of the participant, each with its own phase; an
//! empty list cancels all. A PEN frame carries 1–16 samples, oldest first.

use anyhow::{Context as _, Result};
use prost::Message as _;

use crate::protocol::peer::{self, FrameType};

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Touch {
    #[prost(int32, optional, tag = "1")]
    pub screen_id: Option<i32>,
    #[prost(message, repeated, tag = "2")]
    pub contacts: Vec<Contact>,
    /// Controller monotonic clock in µs (wraps); diagnostics only.
    #[prost(uint32, tag = "3")]
    pub time_us: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Contact {
    /// 1..=255, assigned by the controller; reusable after UP/CANCEL.
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(enumeration = "TouchPhase", tag = "2")]
    pub phase: i32,
    #[prost(float, tag = "3")]
    pub x: f32,
    #[prost(float, tag = "4")]
    pub y: f32,
    /// (0, 1]; 0 = not reported.
    #[prost(float, tag = "5")]
    pub pressure: f32,
    /// Contact size normalized to the screen; both 0 = not reported.
    #[prost(float, tag = "6")]
    pub width: f32,
    #[prost(float, tag = "7")]
    pub height: f32,
    /// 0..359 degrees, clockwise from the x axis.
    #[prost(uint32, tag = "8")]
    pub orientation: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub(crate) enum TouchPhase {
    Unspecified = 0,
    Down = 1,
    Move = 2,
    Up = 3,
    Cancel = 4,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Pen {
    #[prost(int32, optional, tag = "1")]
    pub screen_id: Option<i32>,
    /// Oldest first; the last one is the current state.
    #[prost(message, repeated, tag = "2")]
    pub samples: Vec<PenSample>,
    #[prost(uint32, tag = "3")]
    pub time_us: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct PenSample {
    #[prost(enumeration = "PenPhase", tag = "1")]
    pub phase: i32,
    #[prost(float, tag = "2")]
    pub x: f32,
    #[prost(float, tag = "3")]
    pub y: f32,
    #[prost(float, tag = "4")]
    pub pressure: f32,
    /// −90..=90 degrees, tilted right positive.
    #[prost(sint32, tag = "5")]
    pub tilt_x: i32,
    /// −90..=90 degrees, tilted towards the user positive.
    #[prost(sint32, tag = "6")]
    pub tilt_y: i32,
    /// 0..359 degrees, clockwise.
    #[prost(uint32, tag = "7")]
    pub twist: u32,
    /// bit 0: barrel button; bit 1: eraser end.
    #[prost(uint32, tag = "8")]
    pub buttons: u32,
    /// How much earlier than `time_us` this sample was taken.
    #[prost(uint32, tag = "9")]
    pub age_us: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub(crate) enum PenPhase {
    Unspecified = 0,
    Hover = 1,
    Down = 2,
    Move = 3,
    Up = 4,
    Leave = 5,
    Cancel = 6,
}

pub(crate) fn encode_touch(touch: &Touch) -> Vec<u8> {
    peer::message(FrameType::Touch, touch)
}

pub(crate) fn encode_pen(pen: &Pen) -> Vec<u8> {
    peer::message(FrameType::Pen, pen)
}

/// A decoded TOUCH or PEN frame.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Message {
    Touch(Touch),
    Pen(Pen),
}

/// Decode a TOUCH or PEN frame; `None` for other frames.
pub(crate) fn decode(bytes: &[u8]) -> Result<Option<Message>> {
    if let Some(body) = peer::body_of(FrameType::Touch, bytes) {
        return Touch::decode(body).context("触摸消息无效").map(|t| Some(Message::Touch(t)));
    }
    if let Some(body) = peer::body_of(FrameType::Pen, bytes) {
        return Pen::decode(body).context("笔消息无效").map(|p| Some(Message::Pen(p)));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn body(frame: &[u8]) -> String {
        hex(&frame[farwing_protocol::peer::HEADER_LEN..])
    }

    fn contact(id: u32, phase: TouchPhase, x: f32, y: f32) -> Contact {
        Contact {
            id,
            phase: phase as i32,
            x,
            y,
            ..Default::default()
        }
    }

    fn sample(phase: PenPhase, x: f32, y: f32) -> PenSample {
        PenSample {
            phase: phase as i32,
            x,
            y,
            ..Default::default()
        }
    }

    // Byte examples of `docs/protocol/touch-pen-brief.md` §5.9.
    #[test]
    fn touch_bytes() {
        let frame = encode_touch(&Touch {
            screen_id: Some(0),
            contacts: vec![contact(1, TouchPhase::Down, 0.5, 0.25)],
            time_us: 0,
        });
        assert_eq!(hex(&frame[..farwing_protocol::peer::HEADER_LEN]), "46 57 02 00 01 04 00 00 00 00");
        assert_eq!(body(&frame), "08 00 12 0e 08 01 10 01 1d 00 00 00 3f 25 00 00 80 3e");
        let two = encode_touch(&Touch {
            screen_id: Some(0),
            contacts: vec![
                contact(1, TouchPhase::Move, 0.75, 0.25),
                contact(2, TouchPhase::Down, 0.25, 0.5),
            ],
            time_us: 0,
        });
        assert_eq!(
            body(&two),
            "08 00 12 0e 08 01 10 02 1d 00 00 40 3f 25 00 00 80 3e 12 0e 08 02 10 01 1d 00 00 80 3e 25 00 00 00 3f"
        );
        assert_eq!(
            body(&encode_touch(&Touch {
                screen_id: Some(0),
                contacts: vec![],
                time_us: 0
            })),
            "08 00"
        );
        let rich = encode_touch(&Touch {
            screen_id: Some(1),
            contacts: vec![Contact {
                id: 3,
                phase: TouchPhase::Down as i32,
                x: 0.5,
                y: 0.5,
                pressure: 0.5,
                width: 0.01,
                height: 0.02,
                orientation: 90,
            }],
            time_us: 1_000_000,
        });
        assert_eq!(
            body(&rich),
            "08 01 12 1f 08 03 10 01 1d 00 00 00 3f 25 00 00 00 3f 2d 00 00 00 3f 35 0a d7 23 3c 3d 0a d7 a3 3c 40 5a 18 c0 84 3d"
        );
        assert_eq!(decode(&rich).unwrap(), Some(Message::Touch(Touch::decode(&rich[10..]).unwrap())));
    }

    #[test]
    fn pen_bytes() {
        let hover = encode_pen(&Pen {
            screen_id: Some(0),
            samples: vec![PenSample {
                tilt_x: 30,
                tilt_y: -15,
                ..sample(PenPhase::Hover, 0.5, 0.5)
            }],
            time_us: 0,
        });
        assert_eq!(hex(&hover[..farwing_protocol::peer::HEADER_LEN]), "46 57 02 00 01 05 00 00 00 00");
        assert_eq!(body(&hover), "08 00 12 10 08 01 15 00 00 00 3f 1d 00 00 00 3f 28 3c 30 1d");
        let moves = encode_pen(&Pen {
            screen_id: Some(0),
            samples: vec![
                PenSample {
                    pressure: 0.5,
                    age_us: 4167,
                    ..sample(PenPhase::Move, 0.5, 0.5)
                },
                PenSample {
                    pressure: 0.75,
                    ..sample(PenPhase::Move, 0.75, 0.5)
                },
            ],
            time_us: 0,
        });
        assert_eq!(
            body(&moves),
            "08 00 12 14 08 03 15 00 00 00 3f 1d 00 00 00 3f 25 00 00 00 3f 48 c7 20 12 11 08 03 15 00 00 40 3f 1d 00 00 00 3f 25 00 00 40 3f"
        );
        let eraser = encode_pen(&Pen {
            screen_id: Some(0),
            samples: vec![PenSample {
                pressure: 0.5,
                buttons: 2,
                ..sample(PenPhase::Down, 0.5, 0.5)
            }],
            time_us: 0,
        });
        assert_eq!(
            body(&eraser),
            "08 00 12 13 08 02 15 00 00 00 3f 1d 00 00 00 3f 25 00 00 00 3f 40 02"
        );
        let leave = encode_pen(&Pen {
            screen_id: Some(0),
            samples: vec![sample(PenPhase::Leave, 0.0, 0.0)],
            time_us: 0,
        });
        assert_eq!(body(&leave), "08 00 12 02 08 05");
        assert!(matches!(decode(&leave).unwrap(), Some(Message::Pen(_))));
    }

    #[test]
    fn other_frames_are_not_pointer_input() {
        let bytes = peer::frame(FrameType::Input, &[]);
        assert!(decode(&bytes).unwrap().is_none());
    }
}
