//! Annotation on the host's screens (peer frame type `ANNOTATION`, session
//! channel, host feature `annotation`). Every [`Op`] but `Reset` is a
//! controller request the host answers with an empty response or a
//! `PeerError` with a [`reason`]; `Reset` is the host's push that it dropped
//! its overlay.
use crate::protocol::peer::{self, FrameType};
use anyhow::{Result, ensure};
use prost::Message as _;

/// Largest `ANNOTATION` frame.
pub(crate) const MAX_FRAME: usize = 512 * 1024;
/// Most points one `Draw` carries.
pub(crate) const MAX_DRAW_POINTS: usize = 4096;
/// Most points one stroke holds.
pub(crate) const MAX_STROKE_POINTS: usize = 32_768;

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Annotation {
    #[prost(oneof = "Op", tags = "1, 2, 3, 4, 5, 6, 7, 8")]
    pub op: Option<Op>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub(crate) enum Op {
    /// The overlay on or off; off removes everything.
    #[prost(bool, tag = "1")]
    Show(bool),
    /// Points added to a stroke; the first creates it.
    #[prost(message, tag = "2")]
    Draw(Stroke),
    /// A whole stroke, replacing the one with its ID.
    #[prost(message, tag = "3")]
    Put(Stroke),
    /// Removes one stroke.
    #[prost(message, tag = "4")]
    Erase(StrokeId),
    /// Removes every stroke; `true` also the whiteboards.
    #[prost(bool, tag = "5")]
    Clear(bool),
    /// A whiteboard behind the ink on a screen, or none.
    #[prost(message, tag = "6")]
    Board(Board),
    /// Whole strokes put at once: all or none.
    #[prost(message, tag = "7")]
    Restore(Strokes),
    /// H→C push: the host dropped its overlay; annotation is off.
    #[prost(message, tag = "8")]
    Reset(Reset),
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Stroke {
    /// Non-zero; unique per screen.
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(int32, tag = "2")]
    pub screen: i32,
    #[prost(message, repeated, tag = "3")]
    pub points: Vec<Point>,
    /// Line width in pixels at 100 % scale, up to 64.
    #[prost(float, tag = "4")]
    pub width: f32,
    /// 0xAARRGGBB.
    #[prost(fixed32, tag = "5")]
    pub color: u32,
}

/// A point on the screen, 0..=1 on both axes.
#[derive(Clone, Copy, PartialEq, prost::Message)]
pub(crate) struct Point {
    #[prost(float, tag = "1")]
    pub x: f32,
    #[prost(float, tag = "2")]
    pub y: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct StrokeId {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(int32, tag = "2")]
    pub screen: i32,
}

#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct Board {
    #[prost(int32, tag = "1")]
    pub screen: i32,
    /// 0xFFRRGGBB; none removes the whiteboard.
    #[prost(fixed32, optional, tag = "2")]
    pub color: Option<u32>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Strokes {
    #[prost(message, repeated, tag = "1")]
    pub strokes: Vec<Stroke>,
}

#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct Reset {}

impl Point {
    pub fn valid(&self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && (0.0..=1.0).contains(&self.x)
            && (0.0..=1.0).contains(&self.y)
    }
}

impl Stroke {
    /// A non-empty stroke of at most `points` valid points and a valid width.
    pub fn valid(&self, points: usize) -> bool {
        self.id != 0
            && !self.points.is_empty()
            && self.points.len() <= points
            && self.width.is_finite()
            && self.width > 0.0
            && self.width <= 64.0
            && self.points.iter().all(Point::valid)
    }
}

/// Why the host did not apply a request: its `PeerError` code and reason.
pub(crate) mod reason {
    use farwing_protocol::peer::error_code;
    /// No user is signed in on the host.
    pub const SIGNED_OUT: (&str, &str) = (error_code::UNAVAILABLE, "signed_out");
    /// The host's desktop is locked.
    pub const LOCKED: (&str, &str) = (error_code::UNAVAILABLE, "locked");
    /// Annotation is not allowed for this participant now.
    pub const NOT_ALLOWED: (&str, &str) = (error_code::FORBIDDEN, "not_allowed");
    /// The overlay is not on.
    pub const OFF: (&str, &str) = (error_code::UNAVAILABLE, "off");
    /// A malformed request, an unknown screen or over a limit.
    pub const INVALID: (&str, &str) = (error_code::BAD_REQUEST, "invalid");
    /// The host could not draw.
    pub const FAILED: (&str, &str) = (error_code::INTERNAL, "failed");
}

pub(crate) type Received = peer::Received<Annotation>;

pub(crate) fn decode(bytes: &[u8]) -> Result<Option<Received>> {
    peer::receive(FrameType::Annotation, bytes, MAX_FRAME)
}

pub(crate) fn is_packet(bytes: &[u8]) -> bool {
    peer::is_type(bytes, &[FrameType::Annotation])
}

/// Request `request_id` carrying `op`.
pub(crate) fn request(request_id: u32, op: Op) -> Result<Vec<u8>> {
    let message = Annotation { op: Some(op) };
    ensure!(
        message.encoded_len() + farwing_protocol::peer::HEADER_LEN <= MAX_FRAME,
        "annotation request too large"
    );
    Ok(peer::framed(FrameType::Annotation, 0, request_id, &message))
}

/// The host's answer to request `request_id`.
pub(crate) fn response(request_id: u32, result: std::result::Result<(), (&str, &str)>) -> Vec<u8> {
    let empty = Annotation::default();
    peer::answer(FrameType::Annotation, request_id, result.map(|()| &empty))
}

/// The host's push that its overlay is gone.
pub(crate) fn reset() -> Vec<u8> {
    peer::message(
        FrameType::Annotation,
        &Annotation {
            op: Some(Op::Reset(Reset {})),
        },
    )
}
