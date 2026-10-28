//! Native drag sessions (DRAG v2, `docs/protocol/v2-design.md` §8.3): `DRAG`
//! frames on the session channel. The dragged files are exchanged by a
//! private CLIPBOARD v2 exchange whose messages travel inside `DRAG` frames
//! (`Payload::Files`) and whose contents are `TRANSFER{kind: DRAG}` chunks
//! on the bulk channel.
use anyhow::{Result, ensure};
use prost::Message;
use serde::{Deserialize, Serialize};
use crate::protocol::peer::{self, FrameType};
/// A `DRAG` frame stays within one data-channel message (§5.3).
pub(crate) const MAX_PACKET: usize = 64 * 1024;
/// Largest side of a drag image, so its pixels fit one frame.
pub(crate) const MAX_IMAGE: u32 = 120;
pub(crate) const COPY: u32 = 1;
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Packet {
    #[prost(fixed64, tag = "1")]
    pub token: u64,
    #[prost(fixed64, tag = "2")]
    pub drag: u64,
    #[prost(uint64, tag = "3")]
    pub sequence: u64,
    #[prost(
        oneof = "Payload",
        tags = "4,5,6,7,8,9,11,12,13,14,15,16,20,21,22,23,24"
    )]
    pub payload: Option<Payload>,
}
#[derive(Clone, PartialEq, prost::Oneof)]
pub(crate) enum Payload {
    #[prost(uint32, tag = "4")]
    Hello(u32),
    #[prost(uint32, tag = "5")]
    HelloAck(u32),
    #[prost(message, tag = "6")]
    Begin(Begin),
    #[prost(message, tag = "7")]
    Position(Point),
    #[prost(message, tag = "8")]
    Commit(Point),
    #[prost(bool, tag = "9")]
    Cancel(bool),
    #[prost(message, tag = "11")]
    Feedback(Feedback),
    #[prost(message, tag = "12")]
    Finished(Finished),
    #[prost(message, tag = "13")]
    Probe(Point),
    #[prost(uint64, tag = "14")]
    Progress(u64),
    #[prost(bool, tag = "15")]
    SourceReleased(bool),
    #[prost(bool, tag = "16")]
    Available(bool),
    #[prost(message, tag = "20")]
    Image(DragImage),
    #[prost(message, tag = "21")]
    Resume(Point),
    #[prost(bool, tag = "22")]
    Resumed(bool),
    #[prost(bool, tag = "23")]
    HandoffDone(bool),
    /// A complete `CLIPBOARD` frame of this drag's file exchange.
    #[prost(bytes, tag = "24")]
    Files(Vec<u8>),
}
#[derive(Clone, PartialEq, Message)]
pub(crate) struct DragImage {
    #[prost(uint32, tag = "1")]
    pub width: u32,
    #[prost(uint32, tag = "2")]
    pub height: u32,
    /// Top-down BGRA, straight alpha. Empty means no source image available.
    #[prost(bytes, tag = "3")]
    pub pixels: Vec<u8>,
}
impl DragImage {
    pub fn valid(&self) -> bool {
        (self.width == 0 && self.height == 0 && self.pixels.is_empty())
            || ((1..=MAX_IMAGE).contains(&self.width)
                && (1..=MAX_IMAGE).contains(&self.height)
                && self.pixels.len() == self.width as usize * self.height as usize * 4)
    }
}
#[derive(Clone, Copy, PartialEq, Message, Serialize, Deserialize)]
pub(crate) struct Point {
    #[prost(int32, tag = "1")]
    pub screen: i32,
    #[prost(double, tag = "2")]
    pub x: f64,
    #[prost(double, tag = "3")]
    pub y: f64,
}
impl Point {
    pub fn valid(&self) -> bool {
        self.screen >= 0
            && self.x.is_finite()
            && self.y.is_finite()
            && (0.0..=1.0).contains(&self.x)
            && (0.0..=1.0).contains(&self.y)
    }
}
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Begin {
    #[prost(message, optional, tag = "1")]
    pub point: Option<Point>,
    #[prost(bool, tag = "2")]
    pub reverse: bool,
    #[prost(bool, tag = "3")]
    pub preserve: bool,
}
#[derive(Clone, Copy, PartialEq, Message)]
pub(crate) struct Feedback {
    #[prost(bool, tag = "1")]
    pub ready: bool,
    #[prost(uint32, tag = "2")]
    pub effect: u32,
}
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Finished {
    /// 1: drag released into Drop; 2: consumer finished; 3: cancelled; 4: failed.
    #[prost(uint32, tag = "1")]
    pub stage: u32,
    /// Why it failed: one of [`reason`] (or empty); the receiver words it.
    #[prost(string, tag = "2")]
    pub reason: String,
    #[prost(uint64, tag = "3")]
    pub bytes_read: u64,
}
/// [`Finished::reason`] values.
pub(crate) mod reason {
    pub const REMOTE_DISABLED: &str = "remote_disabled";
    pub const DIRECTION_DENIED: &str = "direction_denied";
    pub const OUT_NOT_ALLOWED: &str = "out_not_allowed";
    pub const LAYOUT_CHANGED: &str = "layout_changed";
    pub const PREPARE_TIMEOUT: &str = "prepare_timeout";
    pub const RETURN_TIMEOUT: &str = "return_timeout";
    /// Anything else; the sender logs why.
    pub const FAILED: &str = "failed";
}

fn reason_texts() -> [(&'static str, &'static str); 6] {
    let text = &crate::i18n::t().features;
    [
        (reason::REMOTE_DISABLED, text.drag_remote_disabled),
        (reason::DIRECTION_DENIED, text.drag_direction_denied),
        (reason::OUT_NOT_ALLOWED, text.drag_out_not_allowed),
        (reason::LAYOUT_CHANGED, text.drag_layout_changed),
        (reason::PREPARE_TIMEOUT, text.drag_prepare_timeout),
        (reason::RETURN_TIMEOUT, text.drag_return_timeout),
    ]
}
fn text(reason: &str) -> &'static str {
    reason_texts()
        .into_iter()
        .find(|(key, _)| *key == reason)
        .map_or(crate::i18n::t().viewer.drag_failed, |(_, text)| text)
}

/// A drag refused for one of the [`reason`]s; both sides word it from the
/// reason.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Refused(pub &'static str);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(text(self.0))
    }
}
impl std::error::Error for Refused {}

/// The reason to send for a drag that ended with `error`, and this side's
/// detail when the reason does not cover it.
pub(crate) fn failure(error: &anyhow::Error) -> (&'static str, String) {
    match error.downcast_ref::<Refused>() {
        Some(refused) => (refused.0, String::new()),
        None => (reason::FAILED, error.to_string()),
    }
}

/// A received reason in the interface language; `None` for none.
pub(crate) fn reason_text(reason: &str) -> Option<String> {
    if reason.is_empty() {
        return None;
    }
    Some(text(reason).to_owned())
}

pub(crate) fn is_packet(bytes: &[u8]) -> bool {
    peer::is_type(bytes, &[FrameType::Drag])
}
pub(crate) fn decode(bytes: &[u8]) -> Result<Option<Packet>> {
    if !is_packet(bytes) {
        return Ok(None);
    }
    ensure!(bytes.len() <= MAX_PACKET, "拖放消息过大");
    let (_, body) = peer::decode(bytes)?;
    let packet = Packet::decode(body)?;
    validate(&packet)?;
    Ok(Some(packet))
}
pub(crate) fn encode(packet: Packet) -> Result<Vec<u8>> {
    validate(&packet)?;
    ensure!(
        packet.encoded_len() + farwing_protocol::peer::HEADER_LEN <= MAX_PACKET,
        "拖放消息过大"
    );
    Ok(peer::message(FrameType::Drag, &packet))
}
fn validate(packet: &Packet) -> Result<()> {
    ensure!(packet.token != 0, "拖放连接标识无效");
    let payload = packet
        .payload
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("拖放消息类型缺失"))?;
    if matches!(
        payload,
        Payload::Hello(_) | Payload::HelloAck(_) | Payload::Available(_)
    ) {
        ensure!(packet.drag == 0, "拖放握手带有活动任务");
    } else {
        ensure!(packet.drag != 0 && packet.sequence != 0, "拖放会话标识无效");
    }
    match payload {
        Payload::Begin(v) => ensure!(v.point.is_some_and(|p| p.valid()), "拖放起点无效"),
        Payload::Position(p) | Payload::Commit(p) | Payload::Probe(p) | Payload::Resume(p) => {
            ensure!(p.valid(), "拖放坐标无效")
        }
        Payload::Finished(v) => ensure!(
            (1..=4).contains(&v.stage) && v.reason.len() <= 64,
            "拖放结果无效"
        ),
        Payload::Feedback(v) => ensure!(v.effect & !COPY == 0, "拖放效果不支持"),
        Payload::Image(v) => ensure!(v.valid(), "拖放图像无效"),
        Payload::Files(v) => ensure!(
            crate::features::clipboard::wire::is_frame(v),
            "拖放文件消息无效"
        ),
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_send_their_reason_and_other_errors_their_detail() {
        let refused = anyhow::anyhow!(Refused(reason::LAYOUT_CHANGED)).context("while moving");
        assert_eq!(failure(&refused), (reason::LAYOUT_CHANGED, String::new()));
        let other = anyhow::anyhow!("disk full");
        assert_eq!(failure(&other), (reason::FAILED, "disk full".to_owned()));
        assert_eq!(
            reason_text(reason::LAYOUT_CHANGED).as_deref(),
            Some(crate::i18n::t().features.drag_layout_changed)
        );
    }

    #[test]
    fn drag_frames_stay_small_and_carry_clipboard_messages() {
        let files = crate::features::clipboard::wire::push(
            crate::features::clipboard::wire::Body::Release(
                crate::features::clipboard::wire::Release { offer: 2 },
            ),
        );
        let frame = encode(Packet {
            token: 1,
            drag: 7,
            sequence: 3,
            payload: Some(Payload::Files(files.clone())),
        })
        .unwrap();
        let packet = decode(&frame).unwrap().unwrap();
        assert_eq!(packet.payload, Some(Payload::Files(files)));
        // Raw bytes that are not a CLIPBOARD frame are rejected.
        assert!(
            encode(Packet {
                token: 1,
                drag: 7,
                sequence: 4,
                payload: Some(Payload::Files(vec![1, 2, 3])),
            })
            .is_err()
        );
        // The largest drag image fits one frame.
        let image = DragImage {
            width: MAX_IMAGE,
            height: MAX_IMAGE,
            pixels: vec![0; (MAX_IMAGE * MAX_IMAGE * 4) as usize],
        };
        let frame = encode(Packet {
            token: 1,
            drag: 7,
            sequence: 5,
            payload: Some(Payload::Image(image)),
        })
        .unwrap();
        assert!(frame.len() <= MAX_PACKET);
    }
}
