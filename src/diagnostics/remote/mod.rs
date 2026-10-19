//! Remote diagnostic bundle (DIAGNOSTICS v2, `docs/protocol/v2-design.md`
//! §8.4): `DIAGNOSTICS` frames on the session channel; the archive streams as
//! `TRANSFER{kind: DIAGNOSTICS}` on the bulk channel, paced by the
//! controller's `FLOW` credits. The host only ever sends its own redacted
//! bundle; no remote paths or arbitrary file requests are accepted.
mod controller;
mod host;
pub(crate) mod service;
use super::bundle;
use crate::protocol::peer::{self, FrameType};
use anyhow::{Context, Result};
pub(crate) use controller::{Controller, Phase, Snapshot};
pub(crate) use host::Host;
use prost::Message;
use std::sync::{Mutex, MutexGuard};

/// A controller gives up when the host stays silent this long.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Push (host → controller): whether this participant may fetch a bundle.
/// Sent when the session channel opens and whenever it changes.
#[derive(Clone, PartialEq, Message)]
struct Status {
    #[prost(bool, tag = "1")]
    allowed: bool,
}
/// Request (controller → host): prepare a bundle and send it on `stream`
/// → [`Ready`], or `RESPONSE|ERROR`.
#[derive(Clone, PartialEq, Message)]
struct Request {
    #[prost(uint32, tag = "1")]
    stream: u32,
}
/// Push (host → controller) while the bundle is prepared.
#[derive(Clone, PartialEq, Message)]
struct Progress {
    #[prost(uint32, tag = "1")]
    stream: u32,
    #[prost(string, tag = "2")]
    stage: String,
    #[prost(uint64, tag = "3")]
    completed: u64,
    #[prost(uint64, tag = "4")]
    total: u64,
}
/// Response to [`Request`]: the archive follows on the request's stream.
#[derive(Clone, PartialEq, Message)]
struct Ready {
    #[prost(uint64, tag = "1")]
    size: u64,
    /// Lowercase hex SHA-256 of the archive.
    #[prost(string, tag = "2")]
    sha256: String,
    #[prost(uint32, tag = "3")]
    logs: u32,
    #[prost(uint32, tag = "4")]
    warnings: u32,
}
/// Push (controller → host): stop preparing or sending `stream`.
#[derive(Clone, PartialEq, Message)]
struct Cancel {
    #[prost(uint32, tag = "1")]
    stream: u32,
}
/// Push (host → controller): the transfer of `stream` stopped after
/// [`Ready`] (read error, permission revoked).
#[derive(Clone, PartialEq, Message)]
struct Failed {
    #[prost(uint32, tag = "1")]
    stream: u32,
    #[prost(string, tag = "2")]
    message: String,
}
#[derive(Clone, PartialEq, prost::Oneof)]
enum Body {
    #[prost(message, tag = "1")]
    Status(Status),
    #[prost(message, tag = "2")]
    Request(Request),
    #[prost(message, tag = "3")]
    Progress(Progress),
    #[prost(message, tag = "4")]
    Ready(Ready),
    #[prost(message, tag = "5")]
    Cancel(Cancel),
    #[prost(message, tag = "6")]
    Failed(Failed),
}
#[derive(Clone, PartialEq, Message)]
struct DiagnosticsMessage {
    #[prost(oneof = "Body", tags = "1, 2, 3, 4, 5, 6")]
    body: Option<Body>,
}

fn frame(flags: u8, request_id: u32, body: Body) -> Vec<u8> {
    peer::framed(
        FrameType::Diagnostics,
        flags,
        request_id,
        &DiagnosticsMessage { body: Some(body) },
    )
}

/// A received `DIAGNOSTICS` message.
#[derive(Debug, PartialEq)]
enum Received {
    Push(Body),
    Request(u32, Body),
    Response(u32, Body),
    Failure(u32, String),
}

fn decode(bytes: &[u8]) -> Result<Option<Received>> {
    let Some(body) = peer::body_of(FrameType::Diagnostics, bytes) else {
        return Ok(None);
    };
    let (header, _) = peer::decode(bytes)?;
    if header.is_response() && header.is_error() {
        let error = farwing_protocol::peer::PeerError::decode(body)
            .context("无效的诊断错误回复")?;
        return Ok(Some(Received::Failure(header.request_id, error.message)));
    }
    let message = DiagnosticsMessage::decode(body)
        .context("无效的诊断消息")?
        .body
        .context("诊断消息没有受支持的内容")?;
    Ok(Some(match (header.request_id, header.is_response()) {
        (0, _) => Received::Push(message),
        (id, false) => Received::Request(id, message),
        (id, true) => Received::Response(id, message),
    }))
}

/// Whether `bytes` is diagnostics traffic: a `DIAGNOSTICS` frame or a
/// `TRANSFER`/`FLOW` frame of a diagnostics stream.
pub(crate) fn claims(bytes: &[u8]) -> bool {
    peer::is_type(bytes, &[FrameType::Diagnostics])
        || crate::transport::transfer::kind_of(bytes)
            == Some(crate::transport::transfer::kind::DIAGNOSTICS)
}

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::peer::flags;

    #[test]
    fn messages_round_trip_and_ignore_other_frames() {
        let request = frame(0, 4, Body::Request(Request { stream: 9 }));
        assert_eq!(
            decode(&request).unwrap(),
            Some(Received::Request(4, Body::Request(Request { stream: 9 })))
        );
        let ready = Body::Ready(Ready {
            size: 10,
            sha256: "ab".repeat(32),
            logs: 2,
            warnings: 1,
        });
        assert_eq!(
            decode(&frame(flags::RESPONSE, 4, ready.clone())).unwrap(),
            Some(Received::Response(4, ready))
        );
        let refused = peer::error_response(FrameType::Diagnostics, 4, "forbidden", "no");
        assert_eq!(decode(&refused).unwrap(), Some(Received::Failure(4, "no".into())));
        let status = frame(0, 0, Body::Status(Status { allowed: true }));
        assert_eq!(
            decode(&status).unwrap(),
            Some(Received::Push(Body::Status(Status { allowed: true })))
        );
        assert!(claims(&status));
        assert_eq!(decode(&peer::frame(FrameType::File, &[])).unwrap(), None);
    }
}
