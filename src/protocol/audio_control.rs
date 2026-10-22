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
    /// `reason`: one of [`reason`]; the receiver words it.
    Rejected {
        request: u64,
        reason: String,
    },
}

/// Why a quality request was not applied.
pub(crate) mod reason {
    /// Not one of the offered rates.
    pub const INVALID_QUALITY: &str = "invalid_quality";
    /// Desktop audio is not allowed in this session.
    pub const NOT_ALLOWED: &str = "not_allowed";
    /// The session has no audio track.
    pub const NOT_NEGOTIATED: &str = "not_negotiated";
}

/// A rejection's reason in the interface language.
pub(crate) fn reason_text(reason: &str) -> &'static str {
    let t = crate::i18n::t();
    match reason {
        reason::INVALID_QUALITY => t.session.audio_quality_required,
        reason::NOT_ALLOWED => t.features.audio_not_allowed,
        reason::NOT_NEGOTIATED => t.features.audio_track_not_negotiated,
        _ => t.features.audio_quality_not_applied,
    }
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
