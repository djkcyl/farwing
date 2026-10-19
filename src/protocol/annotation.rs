//! Native annotation messages (peer frame type `ANNOTATION`) on the session channel.
use crate::features::stream_control::annotation::wire::{PbDrawPoint, PbDrawRequest, PbDrawStroke};
use anyhow::{Result, ensure};
use prost::Message;
use serde::{Deserialize, Serialize};

use crate::protocol::peer::{self, FrameType};
pub(crate) const MAX_PACKET: usize = 512 * 1024;
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Packet {
    #[prost(fixed64, tag = "1")]
    pub token: u64,
    #[prost(int64, tag = "2")]
    pub request: i64,
    #[prost(oneof = "Payload", tags = "3,4,5")]
    pub payload: Option<Payload>,
}
#[derive(Clone, PartialEq, prost::Oneof)]
pub(crate) enum Payload {
    #[prost(uint32, tag = "3")]
    Hello(u32),
    #[prost(message, tag = "4")]
    Command(Command),
    #[prost(int32, tag = "5")]
    Result(i32),
}
#[derive(Clone, PartialEq, Message, Serialize, Deserialize)]
pub(crate) struct Command {
    #[prost(oneof = "Operation", tags = "1,2,3,4,5,6,7")]
    pub operation: Option<Operation>,
}
#[derive(Clone, PartialEq, prost::Oneof, Serialize, Deserialize)]
pub(crate) enum Operation {
    #[prost(message, tag = "1")]
    Draw(PbDrawRequest),
    #[prost(message, tag = "2")]
    Replace(Object),
    #[prost(message, tag = "3")]
    Batch(Batch),
    #[prost(message, tag = "4")]
    Board(Board),
    /// false clears ordinary ink; true also clears background and transient tools.
    #[prost(bool, tag = "5")]
    Clear(bool),
    #[prost(message, tag = "6")]
    Laser(Laser),
    #[prost(message, tag = "7")]
    Click(Click),
}
#[derive(Clone, PartialEq, Message, Serialize, Deserialize)]
pub(crate) struct Batch {
    #[prost(message, repeated, tag = "1")]
    pub commands: Vec<Command>,
}
#[derive(Clone, PartialEq, Message, Serialize, Deserialize)]
pub(crate) struct Object {
    #[prost(message, optional, tag = "1")]
    pub stroke: Option<PbDrawStroke>,
    #[prost(bool, tag = "2")]
    pub transient: bool,
}
#[derive(Clone, PartialEq, Message, Serialize, Deserialize)]
pub(crate) struct Board {
    #[prost(int32, tag = "1")]
    pub screen: i32,
    #[prost(fixed32, optional, tag = "2")]
    pub color: Option<u32>,
}
#[derive(Clone, PartialEq, Message, Serialize, Deserialize)]
pub(crate) struct LaserSample {
    #[prost(message, optional, tag = "1")]
    pub point: Option<PbDrawPoint>,
    #[prost(uint32, tag = "2")]
    pub age_ms: u32,
}
#[derive(Clone, PartialEq, Message, Serialize, Deserialize)]
pub(crate) struct Laser {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(int32, tag = "2")]
    pub screen: i32,
    #[prost(float, tag = "3")]
    pub width: f32,
    #[prost(fixed32, tag = "4")]
    pub color: u32,
    #[prost(uint32, tag = "5")]
    pub tail_ms: u32,
    #[prost(message, repeated, tag = "6")]
    pub samples: Vec<LaserSample>,
}
#[derive(Clone, PartialEq, Message, Serialize, Deserialize)]
pub(crate) struct Click {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(int32, tag = "2")]
    pub screen: i32,
    #[prost(message, optional, tag = "3")]
    pub center: Option<PbDrawPoint>,
    #[prost(message, optional, tag = "4")]
    pub radii: Option<PbDrawPoint>,
    #[prost(float, tag = "5")]
    pub width: f32,
    #[prost(fixed32, tag = "6")]
    pub color: u32,
}
impl Command {
    pub fn new(operation: Operation) -> Self {
        Self {
            operation: Some(operation),
        }
    }
}
pub(crate) fn decode(bytes: &[u8]) -> Result<Option<Packet>> {
    let Some(body) = peer::body_of(FrameType::Annotation, bytes) else {
        return Ok(None);
    };
    ensure!(bytes.len() <= MAX_PACKET, "批注扩展消息过大");
    let packet = Packet::decode(body)?;
    ensure!(
        packet.token != 0 && packet.payload.is_some(),
        "批注扩展头无效"
    );
    Ok(Some(packet))
}
pub(crate) fn is_packet(bytes: &[u8]) -> bool {
    peer::is_type(bytes, &[FrameType::Annotation])
}
pub(crate) fn encode(packet: Packet) -> Result<Vec<u8>> {
    ensure!(
        packet.encoded_len() + farwing_protocol::peer::HEADER_LEN <= MAX_PACKET,
        "批注扩展消息过大"
    );
    Ok(peer::message(FrameType::Annotation, &packet))
}
