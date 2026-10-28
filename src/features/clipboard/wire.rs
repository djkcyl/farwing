//! CLIPBOARD v2 (`docs/protocol/v2-design.md` §8.2): the `CLIPBOARD` frame
//! body on the session channel. Requests carry a non-zero FPP request ID and
//! are answered with `RESPONSE` (or `RESPONSE|ERROR` + `PeerError`); offers
//! and stream control are pushes. Contents larger than [`INLINE`] travel as
//! `TRANSFER{kind: CLIPBOARD}` on the bulk channel, on the stream the
//! requester named, paced by its `FLOW` credits.
//!
//! The same messages describe the private file exchange of a native drag
//! (§8.3); its data uses `TRANSFER{kind: DRAG}`.
use crate::protocol::peer::{self, FrameType, Header, flags};
use crate::i18n::t;
use anyhow::{Context, Result};
use prost::Message;

/// Contents up to this size are answered inline in `Data`.
pub(crate) const INLINE: usize = 32 * 1024;
/// Largest reassembled `CLIPBOARD` message (a file list of
/// [`super::MAX_FILES`] entries).
pub(crate) const MAX_MESSAGE: usize = 48 * 1024 * 1024;

/// A clipboard format: the Windows format ID and, for registered formats,
/// its name (the receiver registers the name locally).
#[derive(Clone, PartialEq, Eq, Message)]
pub(crate) struct Format {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(string, tag = "2")]
    pub name: String,
}

/// One entry of an offered file list; directories have size 0.
#[derive(Clone, PartialEq, Eq, Message)]
pub(crate) struct FileItem {
    /// Relative path, `\` separated, starting with the top-level name.
    #[prost(string, tag = "1")]
    pub path: String,
    /// Windows file attributes (`0x10` directory).
    #[prost(uint32, tag = "2")]
    pub attributes: u32,
    /// Last write time as a Windows FILETIME (100 ns since 1601).
    #[prost(uint64, tag = "3")]
    pub mtime: u64,
    #[prost(uint64, tag = "4")]
    pub size: u64,
}

/// Push: new local clipboard contents, rendered on demand. `offer` numbers
/// the sender's offers; an empty format list clears the peer's copy.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Offer {
    #[prost(uint32, tag = "1")]
    pub offer: u32,
    #[prost(message, repeated, tag = "2")]
    pub formats: Vec<Format>,
}

/// Request: the contents of one offered format → [`Data`].
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Fetch {
    #[prost(uint32, tag = "1")]
    pub offer: u32,
    #[prost(message, optional, tag = "2")]
    pub format: Option<Format>,
    /// Stream the requester registered for contents above [`INLINE`].
    #[prost(uint32, tag = "3")]
    pub stream: u32,
}

/// Response to [`Fetch`] and [`ReadFile`]: `size` bytes, inline when they
/// fit [`INLINE`], otherwise as `TRANSFER` chunks on the request's stream.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Data {
    #[prost(uint64, tag = "1")]
    pub size: u64,
    #[prost(bytes = "vec", tag = "2")]
    pub inline: Vec<u8>,
}

/// Request: the file list of an offer → [`Files`] (may be `MORE`-split).
#[derive(Clone, PartialEq, Message)]
pub(crate) struct ListFiles {
    #[prost(uint32, tag = "1")]
    pub offer: u32,
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct Files {
    #[prost(message, repeated, tag = "1")]
    pub items: Vec<FileItem>,
}

/// Request: `length` bytes of file `index` (of the offer's list) from
/// `offset` → [`Data`].
#[derive(Clone, PartialEq, Message)]
pub(crate) struct ReadFile {
    #[prost(uint32, tag = "1")]
    pub offer: u32,
    #[prost(uint32, tag = "2")]
    pub index: u32,
    #[prost(uint64, tag = "3")]
    pub offset: u64,
    #[prost(uint32, tag = "4")]
    pub length: u32,
    #[prost(uint32, tag = "5")]
    pub stream: u32,
}

/// Push: the receiver no longer reads the files of `offer`.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Release {
    #[prost(uint32, tag = "1")]
    pub offer: u32,
}

/// Request (controller → host): offer the host's current clipboard now →
/// [`Offer`] (its formats may be empty). Lets participants without
/// automatic sync (not holding `control` in a shared session) copy the
/// host's clipboard on demand.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Pull {}

/// Push: the requester gave up `stream`; its sender stops.
#[derive(Clone, PartialEq, Message)]
pub(crate) struct Abort {
    #[prost(uint32, tag = "1")]
    pub stream: u32,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub(crate) enum Body {
    #[prost(message, tag = "1")]
    Offer(Offer),
    #[prost(message, tag = "2")]
    Fetch(Fetch),
    #[prost(message, tag = "3")]
    Data(Data),
    #[prost(message, tag = "4")]
    ListFiles(ListFiles),
    #[prost(message, tag = "5")]
    Files(Files),
    #[prost(message, tag = "6")]
    ReadFile(ReadFile),
    #[prost(message, tag = "7")]
    Release(Release),
    #[prost(message, tag = "8")]
    Abort(Abort),
    #[prost(message, tag = "9")]
    Pull(Pull),
}

#[derive(Clone, PartialEq, Message)]
pub(crate) struct ClipboardMessage {
    #[prost(oneof = "Body", tags = "1, 2, 3, 4, 5, 6, 7, 8, 9")]
    pub body: Option<Body>,
}

/// Frames of a request (non-zero `request_id`).
pub(crate) fn request(request_id: u32, body: Body) -> Vec<Vec<u8>> {
    debug_assert_ne!(request_id, 0);
    encode(0, request_id, body)
}

/// Frames answering `request_id`.
pub(crate) fn response(request_id: u32, body: Body) -> Vec<Vec<u8>> {
    encode(flags::RESPONSE, request_id, body)
}

/// A push frame.
pub(crate) fn push(body: Body) -> Vec<u8> {
    encode(0, 0, body).pop().expect("a push is one frame")
}

/// A failed answer to `request_id`: `reason` is one of [`reason`] (sent as
/// the error's message), worded by the receiver.
pub(crate) fn failure(request_id: u32, code: &str, reason: &str) -> Vec<u8> {
    peer::error_response(FrameType::Clipboard, request_id, code, reason)
}

/// Why a clipboard request was refused.
pub(crate) mod reason {
    /// No clipboard on this side (not running, or not this session's).
    pub const UNAVAILABLE: &str = "unavailable";
    pub const PAUSED: &str = "paused";
    /// Only the host answers a pull.
    pub const PULL_NOT_SERVED: &str = "pull_not_served";
    pub const INVALID_REQUEST: &str = "invalid_request";
    pub const FILES_OFF: &str = "files_off";
    /// The content is marked as not to be shared.
    pub const EXCLUDED: &str = "excluded";
    pub const EMPTY: &str = "empty";
    /// The offered format can no longer be read.
    pub const FORMAT_UNAVAILABLE: &str = "format_unavailable";
    /// The offered files are gone or can no longer be read.
    pub const FILES_UNAVAILABLE: &str = "files_unavailable";
    pub const FAILED: &str = "failed";
}

/// A refusal's reason in the interface language.
pub(crate) fn reason_text(reason: &str) -> &'static str {
    let text = &t().features;
    match reason {
        reason::UNAVAILABLE => text.clipboard_unavailable,
        reason::PAUSED => text.clipboard_paused,
        reason::PULL_NOT_SERVED => text.clipboard_pull_not_served,
        reason::INVALID_REQUEST => text.clipboard_invalid_request,
        reason::FILES_OFF => text.clipboard_files_off,
        reason::EXCLUDED => text.clipboard_excluded,
        reason::EMPTY => text.clipboard_empty,
        reason::FORMAT_UNAVAILABLE => text.clipboard_format_not_offered,
        reason::FILES_UNAVAILABLE => text.clipboard_files_unavailable,
        _ => text.clipboard_remote_failed,
    }
}

fn encode(flags: u8, request_id: u32, body: Body) -> Vec<Vec<u8>> {
    let bytes = ClipboardMessage { body: Some(body) }.encode_to_vec();
    peer::fragments(FrameType::Clipboard, flags, request_id, &bytes)
}

/// A complete received `CLIPBOARD` message.
#[derive(Debug, PartialEq)]
pub(crate) enum Received {
    Push(Body),
    Request(u32, Body),
    Response(u32, Body),
    /// `RESPONSE|ERROR`: code and message.
    Failure(u32, String, String),
}

/// Decode one complete message (after reassembly).
pub(crate) fn decode(header: &Header, body: &[u8]) -> Result<Received> {
    if header.is_response() && header.is_error() {
        let error = farwing_protocol::peer::PeerError::decode(body)
            .context(t().features.clipboard_invalid_error_reply)?;
        return Ok(Received::Failure(header.request_id, error.code, error.reason));
    }
    let message = ClipboardMessage::decode(body)
        .context(t().features.clipboard_invalid_message)?
        .body
        .context(t().features.clipboard_unsupported_message)?;
    Ok(match (header.request_id, header.is_response()) {
        (0, _) => Received::Push(message),
        (id, false) => Received::Request(id, message),
        (id, true) => Received::Response(id, message),
    })
}

/// Whether `bytes` is a `CLIPBOARD` frame.
pub(crate) fn is_frame(bytes: &[u8]) -> bool {
    peer::is_type(bytes, &[FrameType::Clipboard])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(frames: Vec<Vec<u8>>) -> Received {
        let mut reassembly = peer::Reassembly::new(MAX_MESSAGE);
        let mut out = None;
        for frame in &frames {
            let (header, body) = peer::decode(frame).unwrap();
            if let Some(whole) = reassembly.push(&header, body).unwrap() {
                out = Some(decode(&header, &whole).unwrap());
            }
        }
        out.unwrap()
    }

    #[test]
    fn messages_round_trip() {
        let offer = Body::Offer(Offer {
            offer: 3,
            formats: vec![
                Format { id: 13, name: String::new() },
                Format { id: 0xc0a0, name: "HTML Format".into() },
            ],
        });
        assert_eq!(one(vec![push(offer.clone())]), Received::Push(offer));
        let fetch = Body::Fetch(Fetch {
            offer: 3,
            format: Some(Format { id: 13, name: String::new() }),
            stream: 9,
        });
        assert_eq!(one(request(5, fetch.clone())), Received::Request(5, fetch));
        let data = Body::Data(Data { size: 4, inline: b"text".to_vec() });
        assert_eq!(one(response(5, data.clone())), Received::Response(5, data));
        assert_eq!(
            one(vec![failure(5, "not_found", "gone")]),
            Received::Failure(5, "not_found".into(), "gone".into())
        );
    }

    #[test]
    fn large_file_lists_are_split_into_small_frames() {
        let items = (0..5000)
            .map(|i| FileItem {
                path: format!("folder\\sub\\file-{i:05}-with-a-long-descriptive-name.bin"),
                attributes: 0x20,
                mtime: 133_000_000_000_000_000 + i,
                size: i * 1000,
            })
            .collect::<Vec<_>>();
        let frames = response(8, Body::Files(Files { items: items.clone() }));
        assert!(frames.len() > 1);
        assert!(frames.iter().all(|f| f.len() <= 64 * 1024));
        assert_eq!(one(frames), Received::Response(8, Body::Files(Files { items })));
    }
}
