//! Farwing Peer Protocol (FPP) glue: frame helpers, the feature sets and the
//! `HELLO` handshake shared by the controller and the host.
//!
//! Every data-channel message is a typed frame (`farwing_protocol::peer`).
//! Feature codecs produce complete frames; channel dispatchers route by frame
//! type, never by sniffing payload contents.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
pub(crate) use farwing_protocol::peer::feature;
pub(crate) use farwing_protocol::peer::{FrameType, Header, Hello, Role, channel, flags};
use prost::Message as _;

/// Encode a frame without a request ID.
pub(crate) fn frame(kind: FrameType, body: &[u8]) -> Vec<u8> {
    farwing_protocol::peer::encode(kind, 0, 0, body)
}

/// Encode a frame carrying a protobuf body.
pub(crate) fn message(kind: FrameType, body: &impl prost::Message) -> Vec<u8> {
    framed(kind, 0, 0, body)
}

/// Encode a frame with flags and a request ID carrying a protobuf body.
pub(crate) fn framed(kind: FrameType, flags: u8, request_id: u32, body: &impl prost::Message) -> Vec<u8> {
    let mut output = Vec::with_capacity(farwing_protocol::peer::HEADER_LEN + body.encoded_len());
    farwing_protocol::peer::write_header(&mut output, kind as u16, flags, request_id);
    body.encode(&mut output)
        .expect("Vec<u8> has unlimited capacity");
    output
}

/// Largest body of one fragment of a `MORE`-split message (§5.1).
pub(crate) const FRAGMENT: usize = 60 * 1024;

/// Encode `body` as one frame, or as several with `MORE` set on all but the
/// last when it exceeds [`FRAGMENT`]. Only requests and responses (non-zero
/// `request_id`) may be split.
pub(crate) fn fragments(kind: FrameType, flags: u8, request_id: u32, body: &[u8]) -> Vec<Vec<u8>> {
    if body.len() <= FRAGMENT {
        return vec![farwing_protocol::peer::encode(kind, flags, request_id, body)];
    }
    debug_assert_ne!(request_id, 0, "pushes are never fragmented");
    let count = body.len().div_ceil(FRAGMENT);
    body.chunks(FRAGMENT)
        .enumerate()
        .map(|(index, part)| {
            let more = if index + 1 < count { flags::MORE } else { 0 };
            farwing_protocol::peer::encode(kind, flags | more, request_id, part)
        })
        .collect()
}

/// Joins `MORE` fragments, keyed by request ID and direction. Fragments of
/// one message arrive in order on an ordered channel; fragments of different
/// messages may interleave.
pub(crate) struct Reassembly {
    partial: std::collections::HashMap<(u32, bool), Vec<u8>>,
    limit: usize,
}

impl Reassembly {
    /// `limit`: largest complete message body.
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            partial: Default::default(),
            limit,
        }
    }
    /// Feed one frame; returns the whole body once its last fragment arrived.
    pub(crate) fn push(&mut self, header: &Header, body: &[u8]) -> Result<Option<Vec<u8>>> {
        let more = header.flags & flags::MORE != 0;
        let key = (header.request_id, header.is_response());
        if !more && !self.partial.contains_key(&key) {
            return Ok(Some(body.to_vec()));
        }
        anyhow::ensure!(header.request_id != 0, "推送消息不能分片");
        if !self.partial.contains_key(&key) {
            anyhow::ensure!(self.partial.len() < 16, "未完成的分片消息过多");
        }
        let buffer = self.partial.entry(key).or_default();
        if buffer.len() + body.len() > self.limit {
            self.partial.remove(&key);
            anyhow::bail!("分片消息过大");
        }
        buffer.extend_from_slice(body);
        if more {
            return Ok(None);
        }
        Ok(self.partial.remove(&key))
    }
    pub(crate) fn clear(&mut self) {
        self.partial.clear();
    }
}

/// A `RESPONSE|ERROR` frame with a [`farwing_protocol::peer::PeerError`] body.
pub(crate) fn error_response(kind: FrameType, request_id: u32, code: &str, message: &str) -> Vec<u8> {
    framed(
        kind,
        flags::RESPONSE | flags::ERROR,
        request_id,
        &farwing_protocol::peer::PeerError {
            code: code.to_owned(),
            message: message.to_owned(),
            detail: String::new(),
        },
    )
}

/// Decode any frame.
pub(crate) fn decode(bytes: &[u8]) -> Result<(Header, &[u8])> {
    farwing_protocol::peer::decode(bytes).map_err(|error| anyhow!("{error}"))
}

/// The body of a frame of type `kind`, or `None` when `bytes` is another
/// frame type (or not a frame at all).
pub(crate) fn body_of(kind: FrameType, bytes: &[u8]) -> Option<&[u8]> {
    match farwing_protocol::peer::decode(bytes) {
        Ok((header, body)) if header.kind == kind as u16 => Some(body),
        _ => None,
    }
}

/// Whether `bytes` is a frame of one of `kinds`.
pub(crate) fn is_type(bytes: &[u8], kinds: &[FrameType]) -> bool {
    farwing_protocol::peer::is_frame(bytes)
        && farwing_protocol::peer::decode(bytes)
            .is_ok_and(|(header, _)| kinds.iter().any(|kind| header.kind == *kind as u16))
}

/// The in-session features a peer declared in its `HELLO`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PeerFeatures(BTreeMap<String, u32>);

impl PeerFeatures {
    pub(crate) fn new(features: BTreeMap<String, u32>) -> Self {
        Self(features)
    }
    /// Whether the peer declared `name`.
    pub(crate) fn has(&self, name: &str) -> bool {
        self.0.get(name).is_some_and(|version| *version > 0)
    }
    /// Whether a `HELLO` was received at all.
    pub(crate) fn known(&self) -> bool {
        !self.0.is_empty()
    }
}

fn set(names: &[&str]) -> BTreeMap<String, u32> {
    names.iter().map(|name| ((*name).to_owned(), 1)).collect()
}

/// Features implemented by the controller (viewer). `touch` and `pen`:
/// it sends TOUCH / PEN frames to hosts that declare them.
pub(crate) fn controller_features() -> BTreeMap<String, u32> {
    let mut features = set(&[
        feature::TOUCH,
        feature::PEN,
        feature::VIDEO_H264,
        feature::VIDEO_HEVC,
        feature::VIDEO_AV1,
        feature::VIDEO_444,
        feature::VIDEO_HDR,
        feature::DISPLAY_CONFIG,
        feature::DISPLAY_VIRTUAL,
        feature::DISPLAY_EXCLUSIVE,
        feature::DISPLAY_GEOMETRY,
        feature::AUDIO_QUALITY,
        feature::AUDIO_MICROPHONE,
        feature::CURSOR_SHAPE,
        feature::CLIPBOARD,
        feature::CLIPBOARD_FILES,
        feature::FILES,
        feature::DRAG,
        feature::ANNOTATION,
        feature::PORTS_TCP,
        feature::PORTS_UDP,
        feature::DIAGNOSTICS,
        feature::PERMISSIONS,
        feature::PARTICIPANTS,
        feature::CURSOR_PARTICIPANTS,
        feature::HOST_EVENT,
        feature::FCT,
    ]);
    if !RECOVERY_DISABLED.load(std::sync::atomic::Ordering::Relaxed) {
        features.insert(feature::VIDEO_RECOVERY.to_owned(), 1);
    }
    features
}

static RECOVERY_DISABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `dev video --no-recovery`: this controller does not declare
/// `video.recovery` (keyframes only), for A/B measurements.
pub(crate) fn disable_controller_recovery() {
    RECOVERY_DISABLED.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn controller_recovery() -> bool {
    !RECOVERY_DISABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Features implemented by the host. `touch` and `pen` only where Windows
/// offers synthetic pointers (`docs/protocol/touch-pen-brief.md` §5.4).
pub(crate) fn host_features() -> BTreeMap<String, u32> {
    let mut features = host_static_features();
    if crate::platform::windows::input::pointer::available() {
        features.insert(feature::TOUCH.to_owned(), 1);
        features.insert(feature::PEN.to_owned(), 1);
    }
    features
}

fn host_static_features() -> BTreeMap<String, u32> {
    set(&[
        feature::VIDEO_H264,
        feature::VIDEO_HEVC,
        feature::VIDEO_AV1,
        feature::VIDEO_444,
        feature::VIDEO_HDR,
        feature::DISPLAY_CONFIG,
        feature::DISPLAY_VIRTUAL,
        feature::DISPLAY_EXCLUSIVE,
        feature::DISPLAY_GEOMETRY,
        feature::AUDIO_QUALITY,
        feature::AUDIO_MICROPHONE,
        feature::CURSOR_SHAPE,
        feature::CLIPBOARD,
        feature::CLIPBOARD_FILES,
        feature::FILES,
        feature::DRAG,
        feature::ANNOTATION,
        feature::PORTS_TCP,
        feature::PORTS_UDP,
        feature::DIAGNOSTICS,
        feature::PERMISSIONS,
        feature::PARTICIPANTS,
        feature::CURSOR_PARTICIPANTS,
        feature::HOST_EVENT,
        feature::FCT,
        feature::VIDEO_RECOVERY,
    ])
}

fn hello(role: Role, profile: Option<farwing_protocol::peer::Profile>) -> Hello {
    let mut hello = Hello::new(
        role,
        env!("CARGO_PKG_VERSION"),
        match role {
            Role::Host => host_features(),
            _ => controller_features(),
        },
    );
    hello.platform = farwing_protocol::Platform::Windows.as_str().to_owned();
    hello.profile = profile;
    hello
}

/// `HELLO` request.
pub(crate) fn hello_request(role: Role) -> Vec<u8> {
    message(FrameType::Hello, &hello(role, None))
}

/// `HELLO` request of a controller with its participant profile.
pub(crate) fn hello_request_with(profile: farwing_protocol::peer::Profile) -> Vec<u8> {
    message(FrameType::Hello, &hello(Role::Controller, Some(profile)))
}

/// `HELLO|RESPONSE`.
pub(crate) fn hello_response(role: Role, request_id: u32) -> Vec<u8> {
    framed(FrameType::Hello, flags::RESPONSE, request_id, &hello(role, None))
}

/// A decoded peer `HELLO`.
pub(crate) struct ReceivedHello {
    pub hello: Hello,
    pub response: bool,
    pub request_id: u32,
}

impl ReceivedHello {
    pub(crate) fn features(&self) -> PeerFeatures {
        PeerFeatures::new(self.hello.features.clone())
    }
}

pub(crate) fn decode_hello(bytes: &[u8]) -> Result<Option<ReceivedHello>> {
    let (header, body) = decode(bytes)?;
    if header.frame_type() != Some(FrameType::Hello) {
        return Ok(None);
    }
    let hello = Hello::decode(body)?;
    anyhow::ensure!(
        hello.compatible(),
        "对端 Farwing 协议版本不兼容（{}..={}）",
        hello.min_protocol_version,
        hello.protocol_version
    );
    Ok(Some(ReceivedHello {
        hello,
        response: header.is_response(),
        request_id: header.request_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragments_reassemble_in_order_and_interleaved() {
        let large: Vec<u8> = (0..FRAGMENT * 2 + 100).map(|i| i as u8).collect();
        let first = fragments(FrameType::Clipboard, flags::RESPONSE, 7, &large);
        assert_eq!(first.len(), 3);
        let second = fragments(FrameType::Clipboard, 0, 7, b"small");
        assert_eq!(second.len(), 1);
        let mut reassembly = Reassembly::new(1 << 20);
        let mut done = Vec::new();
        // A request with the same ID in the other direction interleaves.
        for frame in [&first[0], &second[0], &first[1], &first[2]] {
            let (header, body) = decode(frame).unwrap();
            if let Some(whole) = reassembly.push(&header, body).unwrap() {
                done.push((header.is_response(), whole));
            }
        }
        assert_eq!(done[0], (false, b"small".to_vec()));
        assert_eq!(done[1], (true, large));
        let mut tight = Reassembly::new(FRAGMENT);
        let (header, body) = decode(&first[0]).unwrap();
        tight.push(&header, body).unwrap();
        let (header, body) = decode(&first[1]).unwrap();
        assert!(tight.push(&header, body).is_err());
    }

    #[test]
    fn hello_round_trip() {
        let request = hello_request_with(farwing_protocol::peer::Profile {
            nickname: "alice".into(),
            color: 0x3080ff,
        });
        let received = decode_hello(&request).unwrap().unwrap();
        assert!(!received.response);
        assert!(received.features().has(feature::DISPLAY_CONFIG));
        assert_eq!(received.hello.platform, "windows");
        assert_eq!(received.hello.profile.unwrap().nickname, "alice");
        let response = hello_response(Role::Host, 3);
        let received = decode_hello(&response).unwrap().unwrap();
        assert!(received.response && received.request_id == 3);
        assert!(received.features().has(feature::VIDEO_HDR));
        // Touch and pen only where synthetic pointers exist.
        assert_eq!(
            received.features().has(feature::TOUCH),
            crate::platform::windows::input::pointer::available()
        );
        assert!(body_of(FrameType::Hello, &response).is_some());
        assert!(body_of(FrameType::Session, &response).is_none());
    }
}
