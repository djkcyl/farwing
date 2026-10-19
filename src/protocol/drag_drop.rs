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
pub(crate) const NATIVE_RETURN: u32 = 2;
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Packet {
    #[prost(fixed64, tag = "1")]
    pub token: u64,
    #[prost(fixed64, tag = "2")]
    pub drag: u64,
    #[prost(uint64, tag = "3")]
    pub sequence: u64,
    // Optional envelope capability.
    #[prost(uint32, tag = "17")]
    pub capabilities: u32,
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
    #[prost(string, tag = "2")]
    pub message: String,
    #[prost(uint64, tag = "3")]
    pub bytes_read: u64,
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
            (1..=4).contains(&v.stage) && v.message.chars().count() <= 240,
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
            capabilities: NATIVE_RETURN,
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
                capabilities: 0,
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
            capabilities: 0,
            payload: Some(Payload::Image(image)),
        })
        .unwrap();
        assert!(frame.len() <= MAX_PACKET);
    }
}
