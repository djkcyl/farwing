//! Desktop-audio quality control (peer frame type `AUDIO`, JSON body).
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Message {
    Hello {
        revision: u64,
        kbps: u32,
        effective_bps: u32,
    },
    Set {
        request: u64,
        kbps: u32,
    },
    Applied {
        request: u64,
        revision: u64,
        kbps: u32,
        effective_bps: u32,
    },
    Rejected {
        request: u64,
        message: String,
    },
}
use crate::protocol::peer::{self, FrameType};

pub(crate) fn decode(bytes: &[u8]) -> Result<Option<Message>> {
    let Some(body) = peer::body_of(FrameType::Audio, bytes) else {
        return Ok(None);
    };
    anyhow::ensure!(body.len() <= 2048, "audio quality message too large");
    Ok(Some(serde_json::from_slice(body)?))
}
pub(crate) fn encode(message: Message) -> Vec<u8> {
    peer::frame(
        FrameType::Audio,
        &serde_json::to_vec(&message).expect("audio control is serializable"),
    )
}
