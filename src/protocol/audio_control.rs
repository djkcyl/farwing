//! Desktop-audio quality (peer frame type `AUDIO`). The host pushes
//! [`AudioState`]; a controller's [`AudioRequest`] is answered with the
//! state it leads to, or a `PeerError` with a [`reason`].
use crate::protocol::peer::{self, FrameType};
use anyhow::Result;

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct AudioMessage {
    #[prost(oneof = "Body", tags = "1, 2")]
    pub body: Option<Body>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub(crate) enum Body {
    /// The desktop audio rate (push, and the answer to a request).
    #[prost(message, tag = "1")]
    State(AudioState),
    /// A rate for this participant (C→H request).
    #[prost(message, tag = "2")]
    Request(AudioRequest),
}

#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct AudioState {
    /// Grows with every change; older states are dropped.
    #[prost(uint64, tag = "1")]
    pub revision: u64,
    #[prost(uint32, tag = "2")]
    pub kbps: u32,
    /// What the encoder runs at (bit/s).
    #[prost(uint32, tag = "3")]
    pub effective_bps: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct AudioRequest {
    #[prost(uint32, tag = "1")]
    pub kbps: u32,
}

/// Why a rate was refused: its `PeerError` code and reason.
pub(crate) mod reason {
    use farwing_protocol::peer::error_code;
    /// Not one of the offered rates.
    pub const INVALID_QUALITY: (&str, &str) = (error_code::BAD_REQUEST, "invalid_quality");
    /// Desktop audio is not allowed in this session.
    pub const NOT_ALLOWED: (&str, &str) = (error_code::FORBIDDEN, "not_allowed");
    /// The session has no audio track.
    pub const NOT_NEGOTIATED: (&str, &str) = (error_code::UNAVAILABLE, "not_negotiated");
}

/// A refusal's reason in the interface language.
pub(crate) fn reason_text(why: &str) -> &'static str {
    let t = crate::i18n::t();
    if why == reason::INVALID_QUALITY.1 {
        t.session.audio_quality_required
    } else if why == reason::NOT_ALLOWED.1 {
        t.features.audio_not_allowed
    } else if why == reason::NOT_NEGOTIATED.1 {
        t.features.audio_track_not_negotiated
    } else {
        t.features.audio_quality_not_applied
    }
}

pub(crate) type Received = peer::Received<AudioMessage>;

pub(crate) fn decode(bytes: &[u8]) -> Result<Option<Received>> {
    peer::receive(FrameType::Audio, bytes, 2048)
}

/// The host's state push.
pub(crate) fn push(state: AudioState) -> Vec<u8> {
    peer::message(
        FrameType::Audio,
        &AudioMessage {
            body: Some(Body::State(state)),
        },
    )
}

pub(crate) fn request(request_id: u32, kbps: u32) -> Vec<u8> {
    peer::framed(
        FrameType::Audio,
        0,
        request_id,
        &AudioMessage {
            body: Some(Body::Request(AudioRequest { kbps })),
        },
    )
}

/// The answer to request `request_id`: the state it led to, or why not.
pub(crate) fn response(
    request_id: u32,
    result: std::result::Result<AudioState, (&str, &str)>,
) -> Vec<u8> {
    let result = result.map(|state| AudioMessage {
        body: Some(Body::State(state)),
    });
    peer::answer(FrameType::Audio, request_id, result.as_ref().map_err(|e| *e))
}
