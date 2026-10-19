//! `HOST_EVENT` (host → controller, session channel): host lifecycle events
//! that end or interrupt the session for a while (`docs/protocol/v2-design.md`
//! §10), and `PERMISSIONS` (host → controller): the capabilities in effect for
//! this participant (§3.4).

use crate::protocol::peer::{self, FrameType};
use anyhow::Result;
use prost::Message as _;

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct HostEvent {
    /// One of [`kind`].
    #[prost(int32, tag = "1")]
    pub kind: i32,
    /// How long the host expects to be unavailable; 0 when unknown.
    #[prost(uint32, tag = "2")]
    pub expected_downtime_ms: u32,
    #[prost(string, tag = "3")]
    pub message: String,
}

#[allow(dead_code)]
pub(crate) mod kind {
    /// The host installs an update and restarts Farwing.
    pub const UPDATING: i32 = 1;
    pub const RESTARTING: i32 = 2;
    pub const SLEEPING: i32 = 3;
    pub const SHUTTING_DOWN: i32 = 4;
    /// The interactive Windows session changed (user switch, remote desktop).
    pub const SESSION_SWITCH: i32 = 5;
    pub const DISPLAY_CHANGING: i32 = 6;
}

pub(crate) fn encode(kind: i32, expected_downtime_ms: u32, message: impl Into<String>) -> Vec<u8> {
    peer::message(
        FrameType::HostEvent,
        &HostEvent {
            kind,
            expected_downtime_ms,
            message: message.into(),
        },
    )
}

pub(crate) fn decode(body: &[u8]) -> Result<HostEvent> {
    Ok(HostEvent::decode(body)?)
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Permissions {
    /// Session capability names (`farwing_protocol::caps`).
    #[prost(string, repeated, tag = "1")]
    pub grants: Vec<String>,
}

pub(crate) fn encode_permissions(grants: Vec<String>) -> Vec<u8> {
    peer::message(FrameType::Permissions, &Permissions { grants })
}

pub(crate) fn decode_permissions(body: &[u8]) -> Result<Vec<String>> {
    Ok(farwing_protocol::caps::normalize(&Permissions::decode(body)?.grants))
}
