//! Port forwarding v2 frames (`docs/protocol/v2-design.md` §9): `PORT` on the
//! tunnel channel carries stream control and TCP data, `PORT_DGRAM` on the
//! unordered, unreliable dgram channel carries UDP datagrams, and `FLOW`
//! grants per-stream credit for TCP data.

use crate::protocol::peer::{self, FrameType};
use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use prost::Message as _;

/// Largest TCP payload of one `PORT` data message.
pub(crate) const DATA_LIMIT: usize = 64 * 1024 - 64;
/// Largest UDP payload (IPv4 maximum).
pub(crate) const DATAGRAM_LIMIT: usize = 65_507;
const HOST_LIMIT: usize = 253;

pub(crate) mod proto {
    pub const TCP: i32 = 1;
    pub const UDP: i32 = 2;
}

/// Outcome of an `Open` and reason of a `Reset`.
pub(crate) mod code {
    pub const OK: i32 = 0;
    pub const REFUSED: i32 = 1;
    pub const UNREACHABLE: i32 = 2;
    pub const FORBIDDEN: i32 = 3;
    pub const TIMEOUT: i32 = 4;
    pub const DNS_FAILED: i32 = 5;
    /// Too many streams, or a protocol violation.
    pub const LIMIT: i32 = 6;
    pub const PROTOCOL: i32 = 7;
    /// The forwarding ended normally on one side (closed, cancelled).
    pub const CLOSED: i32 = 8;
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct PortMessage {
    #[prost(oneof = "Body", tags = "1, 2, 3, 4, 5")]
    pub body: Option<Body>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub(crate) enum Body {
    /// Controller → host: connect to `host:port`. The controller allocates
    /// the stream number; the host resolves names itself.
    #[prost(message, tag = "1")]
    Open(Open),
    #[prost(message, tag = "2")]
    OpenResult(OpenResult),
    #[prost(message, tag = "3")]
    Data(Data),
    /// This side will send no more data on the stream (TCP half-close).
    #[prost(uint32, tag = "4")]
    ShutdownWrite(u32),
    #[prost(message, tag = "5")]
    Reset(Reset),
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Open {
    #[prost(uint32, tag = "1")]
    pub stream: u32,
    /// One of [`proto`].
    #[prost(int32, tag = "2")]
    pub proto: i32,
    #[prost(string, tag = "3")]
    pub host: String,
    #[prost(uint32, tag = "4")]
    pub port: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct OpenResult {
    #[prost(uint32, tag = "1")]
    pub stream: u32,
    /// One of [`code`].
    #[prost(int32, tag = "2")]
    pub code: i32,
    #[prost(string, tag = "3")]
    pub message: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Data {
    #[prost(uint32, tag = "1")]
    pub stream: u32,
    #[prost(bytes = "bytes", tag = "2")]
    pub data: Bytes,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Reset {
    #[prost(uint32, tag = "1")]
    pub stream: u32,
    /// One of [`code`].
    #[prost(int32, tag = "2")]
    pub code: i32,
    #[prost(string, tag = "3")]
    pub message: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Datagram {
    #[prost(uint32, tag = "1")]
    pub stream: u32,
    #[prost(bytes = "bytes", tag = "2")]
    pub data: Bytes,
}

impl Body {
    pub(crate) fn stream(&self) -> u32 {
        match self {
            Self::Open(open) => open.stream,
            Self::OpenResult(result) => result.stream,
            Self::Data(data) => data.stream,
            Self::ShutdownWrite(stream) => *stream,
            Self::Reset(reset) => reset.stream,
        }
    }
}

impl Open {
    /// Check the request before anything is resolved or connected.
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(self.stream != 0, "无效转发编号");
        ensure!(
            matches!(self.proto, proto::TCP | proto::UDP),
            "不支持的转发协议"
        );
        ensure!((1..=65535).contains(&self.port), "无效目标端口");
        let host = self.host.trim();
        ensure!(
            !host.is_empty()
                && host.len() <= HOST_LIMIT
                && host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'_' | b'[' | b']')),
            "无效目标地址"
        );
        Ok(())
    }
}

pub(crate) fn encode(body: Body) -> Vec<u8> {
    peer::message(FrameType::Port, &PortMessage { body: Some(body) })
}

/// The body of a `PORT` frame, or `None` for other frames.
pub(crate) fn decode(bytes: &[u8]) -> Result<Option<Body>> {
    let Some(body) = peer::body_of(FrameType::Port, bytes) else {
        return Ok(None);
    };
    ensure!(body.len() <= DATA_LIMIT + 256, "端口转发消息过大");
    let message = PortMessage::decode(body).context("无效端口转发消息")?;
    let body = message.body.context("端口转发消息为空")?;
    if let Body::Data(data) = &body {
        ensure!(data.data.len() <= DATA_LIMIT, "端口转发数据块过大");
    }
    Ok(Some(body))
}

pub(crate) fn encode_datagram(stream: u32, data: Bytes) -> Vec<u8> {
    peer::message(FrameType::PortDatagram, &Datagram { stream, data })
}

pub(crate) fn decode_datagram(bytes: &[u8]) -> Result<Option<Datagram>> {
    let Some(body) = peer::body_of(FrameType::PortDatagram, bytes) else {
        return Ok(None);
    };
    let datagram = Datagram::decode(body).context("无效 UDP 数据报")?;
    ensure!(
        datagram.stream != 0 && datagram.data.len() <= DATAGRAM_LIMIT,
        "无效 UDP 数据报"
    );
    Ok(Some(datagram))
}

pub(crate) fn encode_flow(stream: u32, credit: u64) -> Vec<u8> {
    peer::message(
        FrameType::Flow,
        &crate::transport::transfer::Flow {
            channel: crate::transport::transfer::channel::TUNNEL,
            stream,
            credit,
            kind: 0,
        },
    )
}

/// A tunnel `FLOW` frame, or `None` for other frames.
pub(crate) fn decode_flow(bytes: &[u8]) -> Result<Option<crate::transport::transfer::Flow>> {
    let Some(body) = peer::body_of(FrameType::Flow, bytes) else {
        return Ok(None);
    };
    let flow = crate::transport::transfer::Flow::decode(body).context("无效流量窗口消息")?;
    Ok((flow.channel == crate::transport::transfer::channel::TUNNEL).then_some(flow))
}

/// Human-readable outcome of an `Open`.
pub(crate) fn describe(code: i32, message: &str) -> String {
    let reason = match code {
        code::OK => "成功",
        code::REFUSED => "目标拒绝连接",
        code::UNREACHABLE => "目标不可达",
        code::FORBIDDEN => "被控端未允许端口转发",
        code::TIMEOUT => "连接目标超时",
        code::DNS_FAILED => "无法解析目标地址",
        code::LIMIT => "被控端的转发连接数已达上限",
        code::PROTOCOL => "端口转发协议错误",
        code::CLOSED => "转发已关闭",
        _ => "目标连接失败",
    };
    if message.is_empty() {
        reason.to_owned()
    } else {
        format!("{reason}：{message}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_round_trip_and_validation() {
        let open = Open {
            stream: 7,
            proto: proto::TCP,
            host: "nas.local".into(),
            port: 445,
        };
        open.validate().unwrap();
        let bytes = encode(Body::Open(open.clone()));
        assert_eq!(decode(&bytes).unwrap(), Some(Body::Open(open.clone())));
        assert!(Open { port: 0, ..open.clone() }.validate().is_err());
        assert!(Open { host: "a b".into(), ..open.clone() }.validate().is_err());
        assert!(Open { proto: 9, ..open }.validate().is_err());
    }

    #[test]
    fn datagrams_and_flow_round_trip() {
        let bytes = encode_datagram(3, Bytes::from_static(b"ping"));
        let datagram = decode_datagram(&bytes).unwrap().unwrap();
        assert_eq!((datagram.stream, &datagram.data[..]), (3, &b"ping"[..]));
        let flow = decode_flow(&encode_flow(3, 1 << 20)).unwrap().unwrap();
        assert_eq!((flow.stream, flow.credit), (3, 1 << 20));
        assert!(decode(&bytes).unwrap().is_none());
    }
}
