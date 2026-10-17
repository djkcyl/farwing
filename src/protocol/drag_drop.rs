//! Negotiated OpenUUYC drag sessions. Legacy clipboard actions never enter this namespace.
use anyhow::{Result, ensure};
use prost::Message;
use serde::{Deserialize, Serialize};
const MAGIC: &[u8] = b"OUDD\x01";
pub(crate) const MAX_PACKET: usize = 524288;
pub(crate) const COPY: u32 = 1;
pub(crate) const NATIVE_RETURN: u32 = 2;
/// 收得下拖动预览图（`Payload::Image`）。它和 `NATIVE_RETURN` 是两回事：前者说
/// 「认不认识、要不要这张图」，后者说「能不能把拖出去的东西再拖回来」。不认识的
/// 对端会忽略这一位，所以加它不破坏线上兼容。
///
/// 它是本分支加的，上游还没登记，所以放在一个高位上：上游往后按顺序加的位（4、8……）
/// 不会撞上它。撞上了，发送端就会把上游主控当成「收得下图」，又回到「未协商拖放
/// 图像」整次失败的老路。
pub(crate) const DRAG_IMAGE: u32 = 1 << 16;
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Packet {
    #[prost(fixed64, tag = "1")]
    pub token: u64,
    #[prost(fixed64, tag = "2")]
    pub drag: u64,
    #[prost(uint64, tag = "3")]
    pub sequence: u64,
    // Optional envelope capability: v1 peers ignore it and keep ordinary drag.
    #[prost(uint32, tag = "17")]
    pub capabilities: u32,
    #[prost(
        oneof = "Payload",
        tags = "4,5,6,7,8,9,10,11,12,13,14,15,16,20,21,22,23"
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
    #[prost(bytes, tag = "10")]
    Data(Vec<u8>),
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
            || ((1..=128).contains(&self.width)
                && (1..=128).contains(&self.height)
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
    bytes.starts_with(MAGIC)
}
pub(crate) fn decode(bytes: &[u8]) -> Result<Option<Packet>> {
    if !is_packet(bytes) {
        return Ok(None);
    }
    ensure!(bytes.len() < MAX_PACKET, "拖放消息过大");
    let packet = Packet::decode(&bytes[MAGIC.len()..])?;
    validate(&packet)?;
    Ok(Some(packet))
}
pub(crate) fn encode(packet: Packet) -> Result<Vec<u8>> {
    validate(&packet)?;
    ensure!(
        packet.encoded_len() + MAGIC.len() < MAX_PACKET,
        "拖放消息过大"
    );
    let mut bytes = Vec::with_capacity(packet.encoded_len() + MAGIC.len());
    bytes.extend_from_slice(MAGIC);
    packet.encode(&mut bytes)?;
    Ok(bytes)
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
        Payload::Data(v) => ensure!(!v.is_empty() && v.len() <= 512000 + 1024, "拖放文件帧无效"),
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 能力位各占一位，而且经得起一来一回的编解码。
    #[test]
    fn capability_bits_round_trip() {
        assert_eq!(NATIVE_RETURN & DRAG_IMAGE, 0);
        let bytes = encode(Packet {
            token: 1,
            drag: 0,
            sequence: 0,
            capabilities: DRAG_IMAGE | NATIVE_RETURN,
            payload: Some(Payload::Hello(1)),
        })
        .unwrap();
        let packet = decode(&bytes).unwrap().unwrap();
        assert_eq!(packet.capabilities & DRAG_IMAGE, DRAG_IMAGE);
        assert_eq!(packet.capabilities & NATIVE_RETURN, NATIVE_RETURN);
        assert_eq!(packet.capabilities & !(DRAG_IMAGE | NATIVE_RETURN), 0);
    }
}
