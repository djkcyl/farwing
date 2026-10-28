//! FILE v2 wire contract (`docs/protocol/v2-design.md` §8.1).
//!
//! A `FILE` frame (session channel) carries one [`FileMessage`]. Requests
//! carry a non-zero FPP request ID and are answered with the same ID and the
//! `RESPONSE` flag; `ItemDone` pushes carry ID 0. A message larger than
//! [`FRAGMENT`] is split over consecutive frames of the same ID, all but the
//! last flagged `MORE` (§5.1). File data travels as `TRANSFER{kind: FILE}`
//! paced by `FLOW{channel: BULK, kind: FILE}` on the bulk channel.
use super::model::{FileEntry, FileInfo, entry};
use crate::i18n::t;
use crate::protocol::peer::{FrameType, Header, flags};
use crate::transport::transfer::{self, Chunk, Flow};
use anyhow::{Context as _, Result, ensure};
use prost::Message as _;
use std::collections::HashMap;

/// Largest body of one `FILE` frame (§5.3: frames stay within 64 KiB).
pub(crate) const FRAGMENT: usize = 64 * 1024 - farwing_protocol::peer::HEADER_LEN;
/// Largest reassembled `FILE` message.
pub(crate) const MAX_MESSAGE: usize = 32 * 1024 * 1024;
/// Partially reassembled messages one receiver keeps at a time.
const MAX_PARTIAL: usize = 16;

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct FileMessage {
    #[prost(
        oneof = "Body",
        tags = "1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 20, 21, 22, 23, 24, 25, 26, 30"
    )]
    pub body: Option<Body>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub(crate) enum Body {
    // ------------------------------------------------ requests (C→H)
    #[prost(message, tag = "1")]
    List(List),
    #[prost(message, tag = "2")]
    MakeDir(MakeDir),
    #[prost(message, tag = "3")]
    Rename(Rename),
    #[prost(message, tag = "4")]
    Remove(Remove),
    #[prost(message, tag = "5")]
    Exists(Exists),
    #[prost(message, tag = "6")]
    Upload(Upload),
    #[prost(message, tag = "7")]
    UploadStart(UploadStart),
    #[prost(message, tag = "8")]
    Download(Download),
    #[prost(message, tag = "9")]
    DownloadStart(DownloadStart),
    #[prost(message, tag = "10")]
    Cancel(Cancel),
    // ----------------------------------------------- responses (H→C)
    #[prost(message, tag = "20")]
    Listing(Listing),
    #[prost(message, tag = "21")]
    Done(Done),
    #[prost(message, tag = "22")]
    Existing(Existing),
    #[prost(message, tag = "23")]
    Plan(Plan),
    #[prost(message, tag = "24")]
    Manifest(Manifest),
    #[prost(message, tag = "25")]
    Accepted(Accepted),
    #[prost(message, tag = "26")]
    Failed(Failed),
    // ------------------------------------------------- pushes (H→C)
    #[prost(message, tag = "30")]
    ItemDone(ItemDone),
}

impl Body {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::List(_) => "List",
            Self::MakeDir(_) => "MakeDir",
            Self::Rename(_) => "Rename",
            Self::Remove(_) => "Remove",
            Self::Exists(_) => "Exists",
            Self::Upload(_) => "Upload",
            Self::UploadStart(_) => "UploadStart",
            Self::Download(_) => "Download",
            Self::DownloadStart(_) => "DownloadStart",
            Self::Cancel(_) => "Cancel",
            Self::Listing(_) => "Listing",
            Self::Done(_) => "Done",
            Self::Existing(_) => "Existing",
            Self::Plan(_) => "Plan",
            Self::Manifest(_) => "Manifest",
            Self::Accepted(_) => "Accepted",
            Self::Failed(_) => "Failed",
            Self::ItemDone(_) => "ItemDone",
        }
    }
}

/// [`Entry::kind`] values.
pub(crate) mod kind {
    pub const FILE: i32 = 1;
    pub const DIR: i32 = 2;
    pub const DRIVE: i32 = 3;
    pub const SHORTCUT: i32 = 4;
}

/// [`Upload::conflict`] values; the same numbers as a task record's policy.
pub(crate) mod conflict {
    pub const OVERWRITE: i32 = 1;
    pub const RENAME: i32 = 2;
    pub const SKIP: i32 = 3;
}

/// [`Failed::code`] and [`ItemDone::error_code`] values.
pub(crate) mod code {
    pub const OK: i32 = 0;
    pub const PROTOCOL: i32 = 1;
    pub const DENIED: i32 = 2;
    pub const NOT_FOUND: i32 = 3;
    pub const EXISTS: i32 = 4;
    pub const NO_SPACE: i32 = 5;
    pub const IO: i32 = 6;
    pub const BUSY: i32 = 7;
    pub const CHANGED: i32 = 8;
    pub const CANCELLED: i32 = 9;
}

/// [`ItemDone::entry`] of a failure that ends the whole transfer.
pub(crate) const WHOLE_TRANSFER: u32 = u32::MAX;

pub(crate) fn describe(code: i32) -> String {
    let text = &t().features;
    match code {
        code::OK => text.transfer_code_ok,
        code::PROTOCOL => text.transfer_code_protocol,
        code::DENIED => text.transfer_code_denied,
        code::NOT_FOUND => text.transfer_code_not_found,
        code::EXISTS => text.transfer_name_exists,
        code::NO_SPACE => text.transfer_code_no_space,
        code::IO => text.transfer_code_io,
        code::BUSY => text.transfer_code_busy,
        code::CHANGED => text.transfer_code_changed,
        code::CANCELLED => text.transfer_cancelled,
        _ => return (text.transfer_code_other)(code),
    }
    .into()
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct List {
    /// Absolute directory; empty lists the drives and common locations.
    #[prost(string, tag = "1")]
    pub path: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Entry {
    #[prost(string, tag = "1")]
    pub name: String,
    /// One of [`kind`].
    #[prost(int32, tag = "2")]
    pub kind: i32,
    #[prost(uint64, tag = "3")]
    pub size: u64,
    #[prost(uint64, tag = "4")]
    pub mtime_ns: u64,
    #[prost(bool, tag = "5")]
    pub hidden: bool,
    /// Absolute path of a drive or common location (root listing only);
    /// other entries are `Listing::path` joined with `name`.
    #[prost(string, tag = "6")]
    pub path: String,
    /// Common-location icon (root listing only).
    #[prost(string, tag = "7")]
    pub icon: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Listing {
    /// The listed directory as the host resolved it; empty for the roots.
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(message, repeated, tag = "2")]
    pub entries: Vec<Entry>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct MakeDir {
    #[prost(string, tag = "1")]
    pub parent: String,
    #[prost(string, tag = "2")]
    pub name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Rename {
    #[prost(string, tag = "1")]
    pub path: String,
    /// New name within the same directory.
    #[prost(string, tag = "2")]
    pub name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Remove {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(bool, tag = "2")]
    pub recursive: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Done {
    #[prost(string, tag = "1")]
    pub path: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Exists {
    #[prost(string, tag = "1")]
    pub dir: String,
    #[prost(string, repeated, tag = "2")]
    pub names: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Existing {
    #[prost(string, repeated, tag = "1")]
    pub present: Vec<String>,
    /// Names another transfer is writing right now.
    #[prost(string, repeated, tag = "2")]
    pub writing: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Item {
    /// `/`-separated path relative to the transfer root.
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(uint64, tag = "2")]
    pub size: u64,
    #[prost(uint64, tag = "3")]
    pub mtime_ns: u64,
    /// An empty directory: no data, its entry is one empty `last` chunk.
    #[prost(bool, tag = "4")]
    pub dir: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Upload {
    #[prost(uint32, tag = "1")]
    pub transfer: u32,
    /// Absolute destination directory on the host.
    #[prost(string, tag = "2")]
    pub dir: String,
    /// Top-level folder created under `dir`; empty for single files.
    #[prost(string, tag = "3")]
    pub folder: String,
    #[prost(message, repeated, tag = "4")]
    pub items: Vec<Item>,
    /// One of [`conflict`].
    #[prost(int32, tag = "5")]
    pub conflict: i32,
    /// The controller's task key; names the host's partial files and record.
    #[prost(string, tag = "6")]
    pub key: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct ItemPlan {
    /// Bytes of the item the receiver already holds.
    #[prost(uint64, tag = "1")]
    pub offset: u64,
    /// SHA-256 of the last (at most 1 MiB) bytes before `offset`.
    #[prost(bytes = "vec", tag = "2")]
    pub tail_sha256: Vec<u8>,
    /// Not transferred (already saved, or skipped by the conflict policy).
    #[prost(bool, tag = "3")]
    pub skip: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Plan {
    #[prost(uint32, tag = "1")]
    pub transfer: u32,
    #[prost(message, repeated, tag = "2")]
    pub items: Vec<ItemPlan>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct UploadStart {
    #[prost(uint32, tag = "1")]
    pub transfer: u32,
    /// Start of every item after the controller checked the tails.
    #[prost(uint64, repeated, tag = "2")]
    pub offsets: Vec<u64>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Download {
    #[prost(uint32, tag = "1")]
    pub transfer: u32,
    /// Absolute host paths sharing one parent directory.
    #[prost(string, repeated, tag = "2")]
    pub paths: Vec<String>,
    #[prost(string, tag = "3")]
    pub key: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Manifest {
    #[prost(uint32, tag = "1")]
    pub transfer: u32,
    #[prost(string, tag = "2")]
    pub folder: String,
    #[prost(message, repeated, tag = "3")]
    pub items: Vec<Item>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct DownloadStart {
    #[prost(uint32, tag = "1")]
    pub transfer: u32,
    #[prost(message, repeated, tag = "2")]
    pub items: Vec<ItemPlan>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Accepted {
    #[prost(uint64, repeated, tag = "1")]
    pub offsets: Vec<u64>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Cancel {
    /// The running transfer to stop; 0 when none is running.
    #[prost(uint32, tag = "1")]
    pub transfer: u32,
    /// Task key whose host-side partial files to delete with `discard`.
    #[prost(string, tag = "2")]
    pub key: String,
    /// Delete the partial files instead of keeping them for a resume.
    #[prost(bool, tag = "3")]
    pub discard: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct ItemDone {
    #[prost(uint32, tag = "1")]
    pub transfer: u32,
    /// Manifest index, or [`WHOLE_TRANSFER`].
    #[prost(uint32, tag = "2")]
    pub entry: u32,
    /// One of [`code`]; [`code::OK`] once an uploaded item is saved.
    #[prost(int32, tag = "3")]
    pub error_code: i32,
    #[prost(string, tag = "4")]
    pub message: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Failed {
    /// One of [`code`].
    #[prost(int32, tag = "1")]
    pub code: i32,
    #[prost(string, tag = "2")]
    pub message: String,
}

/// A failed request or item, as an error value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Failure {
    pub code: i32,
    pub message: String,
}

impl Failure {
    pub(crate) fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// A failure the other side reported, worded here from its code. Its
    /// own text (in its language) is kept only where the code says little
    /// (I/O and protocol errors), after ours.
    pub(crate) fn received(code: i32, detail: &str) -> Self {
        let detail: String = detail.trim().chars().take(300).collect();
        let message = if matches!(code, code::IO | code::PROTOCOL) && !detail.is_empty() {
            (t().features.transfer_failure_detail)(&describe(code), &detail)
        } else {
            String::new()
        };
        Self { code, message }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.is_empty() {
            f.write_str(&describe(self.code))
        } else {
            f.write_str(&self.message)
        }
    }
}

impl std::error::Error for Failure {}

impl From<Failed> for Failure {
    fn from(value: Failed) -> Self {
        Self::received(value.code, &value.message)
    }
}

// ------------------------------------------------------------- model glue

impl From<&FileInfo> for Item {
    fn from(info: &FileInfo) -> Self {
        Self {
            path: info.rel_path.clone(),
            size: info.size,
            mtime_ns: info.mtime_ns(),
            dir: info.directory,
        }
    }
}

impl From<&Item> for FileInfo {
    fn from(item: &Item) -> Self {
        if item.dir {
            FileInfo::directory(item.path.clone())
        } else {
            FileInfo::file(item.path.clone(), item.size, item.mtime_ns)
        }
    }
}

/// A listing row of the host's own file system.
pub(crate) fn entry_of(value: &FileEntry, roots: bool) -> Entry {
    Entry {
        name: value.name.clone(),
        kind: match value.entry_type {
            entry::DIRECTORY => kind::DIR,
            entry::DRIVE => kind::DRIVE,
            entry::FILE => kind::FILE,
            _ => kind::SHORTCUT,
        },
        size: value.size,
        mtime_ns: value.modified_ns,
        hidden: value.hidden,
        path: if roots {
            value.full_path.clone()
        } else {
            String::new()
        },
        icon: if roots {
            value.icon_type.clone()
        } else {
            String::new()
        },
    }
}

// ------------------------------------------------------------- frames

fn frames(flags: u8, request_id: u32, body: Body) -> Vec<Vec<u8>> {
    let bytes = FileMessage { body: Some(body) }.encode_to_vec();
    if bytes.len() <= FRAGMENT {
        return vec![farwing_protocol::peer::encode(
            FrameType::File,
            flags,
            request_id,
            &bytes,
        )];
    }
    let count = bytes.len().div_ceil(FRAGMENT);
    bytes
        .chunks(FRAGMENT)
        .enumerate()
        .map(|(index, part)| {
            let more = if index + 1 < count { flags::MORE } else { 0 };
            farwing_protocol::peer::encode(FrameType::File, flags | more, request_id, part)
        })
        .collect()
}

/// The frames of request `request_id` (non-zero).
pub(crate) fn request(request_id: u32, body: Body) -> Vec<Vec<u8>> {
    debug_assert_ne!(request_id, 0);
    frames(0, request_id, body)
}

/// The frames answering request `request_id`.
pub(crate) fn response(request_id: u32, body: Body) -> Vec<Vec<u8>> {
    frames(flags::RESPONSE, request_id, body)
}

/// A failed answer to request `request_id`.
pub(crate) fn failed(request_id: u32, failure: &Failure) -> Vec<Vec<u8>> {
    response(
        request_id,
        Body::Failed(Failed {
            code: failure.code,
            message: failure.message.clone(),
        }),
    )
}

/// An `ItemDone` push.
pub(crate) fn item_done(value: ItemDone) -> Vec<u8> {
    crate::protocol::peer::message(
        FrameType::File,
        &FileMessage {
            body: Some(Body::ItemDone(value)),
        },
    )
}

/// A `TRANSFER` frame of a file stream.
pub(crate) fn chunk(value: &Chunk) -> Vec<u8> {
    crate::protocol::peer::message(FrameType::Transfer, value)
}

/// A `FLOW` frame granting `credit` bytes of file stream `transfer`.
pub(crate) fn flow(transfer: u32, credit: u64) -> Vec<u8> {
    crate::protocol::peer::message(
        FrameType::Flow,
        &Flow {
            channel: transfer::channel::BULK,
            stream: transfer,
            credit,
            kind: transfer::kind::FILE,
        },
    )
}

/// A file frame, by type.
pub(crate) enum Frame<'a> {
    File(Header, &'a [u8]),
    Chunk(Chunk),
    Flow(Flow),
}

/// Classify `bytes`; `None` for frames that are not part of file transfer
/// (including other streams' `TRANSFER`/`FLOW`).
pub(crate) fn classify(bytes: &[u8]) -> Result<Option<Frame<'_>>> {
    let Ok((header, body)) = farwing_protocol::peer::decode(bytes) else {
        return Ok(None);
    };
    Ok(match header.frame_type() {
        Some(FrameType::File) => Some(Frame::File(header, body)),
        Some(FrameType::Transfer) => {
            let chunk = Chunk::decode(body).context(t().features.transfer_invalid_chunk)?;
            (chunk.kind == transfer::kind::FILE).then_some(Frame::Chunk(chunk))
        }
        Some(FrameType::Flow) => {
            let flow = Flow::decode(body).context(t().features.transfer_invalid_flow)?;
            (flow.kind == transfer::kind::FILE && flow.channel == transfer::channel::BULK)
                .then_some(Frame::Flow(flow))
        }
        _ => None,
    })
}

/// Whether `bytes` belongs to file transfer, without decoding chunk data.
pub(crate) fn claims(bytes: &[u8]) -> bool {
    match farwing_protocol::peer::decode(bytes) {
        Ok((header, _)) => match header.frame_type() {
            Some(FrameType::File) => true,
            Some(FrameType::Transfer | FrameType::Flow) => {
                transfer::kind_of(bytes) == Some(transfer::kind::FILE)
            }
            _ => false,
        },
        Err(_) => false,
    }
}

/// A complete `FILE` message.
#[derive(Debug)]
pub(crate) struct Received {
    pub request_id: u32,
    pub response: bool,
    pub body: Body,
}

/// Joins `MORE` fragments of `FILE` messages, keyed by request ID and
/// direction. Fragments of one message arrive in order (the session channel
/// is ordered); fragments of different messages may interleave.
#[derive(Default)]
pub(crate) struct Reassembly(HashMap<(u32, bool), Vec<u8>>);

impl Reassembly {
    /// Feed one `FILE` frame; returns the message once it is complete.
    pub(crate) fn push(&mut self, header: &Header, body: &[u8]) -> Result<Option<Received>> {
        let more = header.flags & flags::MORE != 0;
        let key = (header.request_id, header.is_response());
        let whole;
        let body = if more || self.0.contains_key(&key) {
            ensure!(header.request_id != 0, "{}", t().features.transfer_push_fragmented);
            if !self.0.contains_key(&key) {
                ensure!(self.0.len() < MAX_PARTIAL, "{}", t().features.transfer_too_many_partial_messages);
            }
            let buffer = self.0.entry(key).or_default();
            if buffer.len() + body.len() > MAX_MESSAGE {
                self.0.remove(&key);
                anyhow::bail!("{}", t().features.transfer_message_too_large);
            }
            buffer.extend_from_slice(body);
            if more {
                return Ok(None);
            }
            whole = self.0.remove(&key).unwrap_or_default();
            whole.as_slice()
        } else {
            body
        };
        decode_message(header, body).map(Some)
    }
}

fn decode_message(header: &Header, body: &[u8]) -> Result<Received> {
    let response = header.is_response();
    let body = if header.is_error() {
        // A generic FPP failure (e.g. an unsupported request) reads as `Failed`.
        let error = farwing_protocol::peer::PeerError::decode(body).context(t().features.transfer_invalid_error_reply)?;
        use farwing_protocol::peer::error_code as peer;
        Body::Failed(Failed {
            code: match error.code.as_str() {
                peer::FORBIDDEN => code::DENIED,
                peer::NOT_FOUND => code::NOT_FOUND,
                peer::BUSY | peer::UNAVAILABLE => code::BUSY,
                peer::INTERNAL => code::IO,
                _ => code::PROTOCOL,
            },
            message: error.message,
        })
    } else {
        FileMessage::decode(body)
            .context(t().features.transfer_invalid_message)?
            .body
            .context(t().features.transfer_message_unsupported)?
    };
    Ok(Received {
        request_id: header.request_id,
        response,
        body,
    })
}

/// Decode a single (unfragmented) `FILE` frame; `None` for other frames.
#[cfg(test)]
pub(crate) fn decode(bytes: &[u8]) -> Result<Option<Received>> {
    let Some(Frame::File(header, body)) = classify(bytes)? else {
        return Ok(None);
    };
    Reassembly::default().push(&header, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(reassembly: &mut Reassembly, frames: &[Vec<u8>]) -> Vec<Received> {
        frames
            .iter()
            .filter_map(|frame| match classify(frame).unwrap() {
                Some(Frame::File(header, body)) => reassembly.push(&header, body).unwrap(),
                _ => panic!("not a FILE frame"),
            })
            .collect()
    }

    #[test]
    fn requests_and_responses_round_trip() {
        let frames = request(
            7,
            Body::Upload(Upload {
                transfer: 3,
                dir: "D:\\in".into(),
                folder: "docs".into(),
                items: vec![
                    Item {
                        path: "a/b.txt".into(),
                        size: 5,
                        mtime_ns: 1_700_000_000_000_000_100,
                        dir: false,
                    },
                    Item {
                        path: "empty".into(),
                        dir: true,
                        ..Default::default()
                    },
                ],
                conflict: conflict::RENAME,
                key: "k".into(),
            }),
        );
        assert_eq!(frames.len(), 1);
        let received = decode(&frames[0]).unwrap().unwrap();
        assert_eq!(received.request_id, 7);
        assert!(!received.response);
        let Body::Upload(upload) = received.body else {
            panic!("wrong body")
        };
        assert_eq!(upload.items.len(), 2);
        assert!(upload.items[1].dir);
        let info = FileInfo::from(&upload.items[0]);
        assert_eq!(info.modified_ns, 1_700_000_000_000_000_100);
        assert_eq!(Item::from(&info), upload.items[0]);

        let frames = response(
            7,
            Body::Plan(Plan {
                transfer: 3,
                items: vec![ItemPlan {
                    offset: 4,
                    tail_sha256: vec![1; 32],
                    skip: false,
                }],
            }),
        );
        let received = decode(&frames[0]).unwrap().unwrap();
        assert!(received.response && received.request_id == 7);
        assert!(matches!(received.body, Body::Plan(p) if p.items[0].offset == 4));

        let push = item_done(ItemDone {
            transfer: 3,
            entry: 1,
            error_code: code::OK,
            message: String::new(),
        });
        let received = decode(&push).unwrap().unwrap();
        assert_eq!(received.request_id, 0);
        assert!(!received.response);
        assert!(matches!(received.body, Body::ItemDone(d) if d.entry == 1));
    }

    #[test]
    fn failures_and_peer_errors_decode_as_failed() {
        let frames = failed(9, &Failure::new(code::EXISTS, "已存在"));
        let received = decode(&frames[0]).unwrap().unwrap();
        assert!(matches!(&received.body, Body::Failed(f) if f.code == code::EXISTS));
        let error = crate::protocol::peer::framed(
            FrameType::File,
            flags::RESPONSE | flags::ERROR,
            9,
            &farwing_protocol::peer::PeerError {
                code: farwing_protocol::peer::error_code::UNSUPPORTED.into(),
                message: "unsupported".into(),
            },
        );
        let received = decode(&error).unwrap().unwrap();
        assert!(matches!(&received.body, Body::Failed(f) if f.code == code::PROTOCOL));
        assert_eq!(
            Failure::new(code::NO_SPACE, "").to_string(),
            describe(code::NO_SPACE)
        );
    }

    #[test]
    fn large_messages_are_fragmented_and_reassembled() {
        let entries = (0..5000)
            .map(|n| Entry {
                name: format!("file-{n:05}-with-a-long-enough-name.bin"),
                kind: kind::FILE,
                size: n,
                mtime_ns: n * 1_000,
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let frames = response(
            11,
            Body::Listing(Listing {
                path: "C:\\big".into(),
                entries: entries.clone(),
            }),
        );
        assert!(frames.len() > 2);
        assert!(frames.iter().all(|f| f.len() <= 64 * 1024));
        // Another message interleaves between the fragments.
        let mut interleaved = frames.clone();
        interleaved.insert(1, response(12, Body::Done(Done::default())).remove(0));
        let mut reassembly = Reassembly::default();
        let received = feed(&mut reassembly, &interleaved);
        assert_eq!(received.len(), 2);
        assert_eq!(received[0].request_id, 12);
        assert!(matches!(&received[1].body, Body::Listing(l) if l.entries == entries));
        assert!(reassembly.0.is_empty());
    }

    #[test]
    fn data_frames_are_classified_by_kind() {
        let data = chunk(&Chunk {
            stream: 4,
            kind: transfer::kind::FILE,
            offset: 0,
            data: bytes::Bytes::from_static(b"abc"),
            last: true,
            entry: 2,
        });
        assert!(claims(&data));
        assert!(matches!(classify(&data).unwrap(), Some(Frame::Chunk(c)) if c.entry == 2 && c.last));
        let other = crate::protocol::peer::message(
            FrameType::Transfer,
            &Chunk {
                kind: transfer::kind::CLIPBOARD,
                ..Default::default()
            },
        );
        assert!(!claims(&other));
        assert!(classify(&other).unwrap().is_none());
        let credit = flow(4, 9 * 1024 * 1024);
        assert!(claims(&credit));
        assert!(matches!(classify(&credit).unwrap(), Some(Frame::Flow(f)) if f.stream == 4 && f.credit == 9 * 1024 * 1024));
        assert!(!claims(&crate::protocol::peer::hello_request(crate::protocol::peer::Role::Controller)));
        assert!(!claims(b"not a frame"));
    }
}
