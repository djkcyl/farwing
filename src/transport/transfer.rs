//! Unified bulk transfer (`docs/protocol/v2-design.md` §8): `TRANSFER` data
//! frames on the bulk channel, paced by cumulative `FLOW` credits that the
//! receiver grants once data is delivered (written to disk, handed to the
//! clipboard). A sender never exceeds its credit; excess data is a protocol
//! error at the receiver.

use anyhow::{Result, ensure};
use std::time::{Duration, Instant};

/// Largest data payload of one `TRANSFER` frame.
pub(crate) const CHUNK: usize = 64 * 1024 - 64;
/// Credit a receiver grants before any data is delivered.
pub(crate) const INITIAL_WINDOW: u64 = 4 * 1024 * 1024;
/// Upper bound of the adaptive receive window.
pub(crate) const MAX_WINDOW: u64 = 64 * 1024 * 1024;

/// What a stream carries; it selects the owner on the receiving side.
/// CLIPBOARD, DRAG and DIAGNOSTICS are reserved for their v2 data planes.
#[allow(dead_code)]
pub(crate) mod kind {
    pub const FILE: i32 = 1;
    pub const CLIPBOARD: i32 = 2;
    pub const DRAG: i32 = 3;
    pub const DIAGNOSTICS: i32 = 4;
}

/// Body of a `TRANSFER` frame.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Chunk {
    #[prost(uint32, tag = "1")]
    pub stream: u32,
    /// One of [`kind`].
    #[prost(int32, tag = "2")]
    pub kind: i32,
    /// Position of `data` within the current entry.
    #[prost(uint64, tag = "3")]
    pub offset: u64,
    #[prost(bytes = "bytes", tag = "4")]
    pub data: bytes::Bytes,
    /// The entry ends after `data`.
    #[prost(bool, tag = "5")]
    pub last: bool,
    /// Index of the entry (file) within the stream's manifest; entries are
    /// sent one after another.
    #[prost(uint32, tag = "6")]
    pub entry: u32,
}

/// Body of a `FLOW` frame: the receiver allows the sender to have sent up to
/// `credit` bytes of `stream` in total (all entries together).
#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct Flow {
    /// One of [`channel`].
    #[prost(int32, tag = "1")]
    pub channel: i32,
    #[prost(uint32, tag = "2")]
    pub stream: u32,
    #[prost(uint64, tag = "3")]
    pub credit: u64,
    /// The stream's [`kind`] on the bulk channel; selects the owner.
    #[prost(int32, tag = "4")]
    pub kind: i32,
}

pub(crate) mod channel {
    pub const BULK: i32 = 1;
    pub const TUNNEL: i32 = 2;
}

/// The stream kind of a `TRANSFER` or `FLOW` frame; `None` for other frames.
pub(crate) fn kind_of(bytes: &[u8]) -> Option<i32> {
    use crate::protocol::peer::{self, FrameType};
    use prost::Message as _;
    if let Some(body) = peer::body_of(FrameType::Transfer, bytes) {
        // `kind` is field 2; decoding the whole chunk is cheap next to the copy.
        return Chunk::decode(body).ok().map(|chunk| chunk.kind);
    }
    let body = peer::body_of(FrameType::Flow, bytes)?;
    Flow::decode(body).ok().map(|flow| flow.kind)
}

/// Sending side of one stream: how much may still be sent.
#[derive(Debug, Default)]
pub(crate) struct Credit {
    granted: u64,
    sent: u64,
}

impl Credit {
    pub(crate) fn new(initial: u64) -> Self {
        Self {
            granted: initial,
            sent: 0,
        }
    }
    /// Bytes that may be sent now.
    pub(crate) fn available(&self) -> u64 {
        self.granted.saturating_sub(self.sent)
    }
    /// A `FLOW` arrived; credits only grow.
    pub(crate) fn grant(&mut self, cumulative: u64) -> bool {
        if cumulative > self.granted {
            self.granted = cumulative;
            true
        } else {
            false
        }
    }
    /// Account `len` sent bytes, which must fit the credit.
    pub(crate) fn spend(&mut self, len: usize) -> Result<()> {
        ensure!(len as u64 <= self.available(), "{}", crate::i18n::t().session.transfer_window_exceeded);
        self.sent += len as u64;
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn sent(&self) -> u64 {
        self.sent
    }
}

/// Receiving side of one stream: checks the sender against its credit and
/// decides when to grant more.
#[derive(Debug)]
pub(crate) struct Window {
    window: u64,
    granted: u64,
    received: u64,
    delivered: u64,
    /// Start and delivered bytes of the current rate sample.
    sample: (Instant, u64),
}

impl Default for Window {
    fn default() -> Self {
        Self::new()
    }
}

impl Window {
    pub(crate) fn new() -> Self {
        Self {
            window: INITIAL_WINDOW,
            granted: INITIAL_WINDOW,
            received: 0,
            delivered: 0,
            sample: (Instant::now(), 0),
        }
    }
    /// A chunk arrived at `offset` with `len` bytes; it must continue the
    /// stream and stay within the credit.
    pub(crate) fn receive(&mut self, offset: u64, len: usize) -> Result<()> {
        ensure!(offset == self.received, "{}", crate::i18n::t().session.transfer_not_contiguous);
        let end = self.received + len as u64;
        ensure!(end <= self.granted, "{}", crate::i18n::t().session.transfer_peer_exceeded_window);
        self.received = end;
        Ok(())
    }
    pub(crate) fn received(&self) -> u64 {
        self.received
    }
    /// `len` bytes were delivered. Returns the new cumulative credit when it
    /// is worth sending a `FLOW` (a quarter window was freed).
    pub(crate) fn deliver(&mut self, len: usize) -> Option<u64> {
        self.delivered = (self.delivered + len as u64).min(self.received);
        self.adapt(Instant::now());
        let target = self.delivered + self.window;
        (target >= self.granted + self.window / 4).then(|| {
            self.granted = target;
            target
        })
    }
    /// Grow the window while the sender is limited by it: when a whole
    /// window was delivered within a short sample, a larger one is needed to
    /// cover the bandwidth-delay product (at least twice, up to
    /// [`MAX_WINDOW`]).
    fn adapt(&mut self, now: Instant) {
        let (started, base) = self.sample;
        let elapsed = now.saturating_duration_since(started);
        if self.delivered - base >= self.window {
            if elapsed < Duration::from_millis(500) {
                self.window = (self.window * 2).min(MAX_WINDOW);
            }
            self.sample = (now, self.delivered);
        } else if elapsed >= Duration::from_secs(2) {
            self.sample = (now, self.delivered);
        }
    }
    #[cfg(test)]
    pub(crate) fn window(&self) -> u64 {
        self.window
    }
}

/// Split `data` of `entry` starting at `offset` into `TRANSFER` chunks of
/// at most [`CHUNK`] bytes; `last` marks the final chunk of the entry.
#[cfg(test)]
pub(crate) fn chunks(
    stream: u32,
    kind: i32,
    entry: u32,
    offset: u64,
    data: bytes::Bytes,
    last: bool,
) -> Vec<Chunk> {
    if data.is_empty() {
        return vec![Chunk {
            stream,
            kind,
            offset,
            data,
            last,
            entry,
        }];
    }
    let count = data.len().div_ceil(CHUNK);
    (0..count)
        .map(|index| {
            let start = index * CHUNK;
            let end = (start + CHUNK).min(data.len());
            Chunk {
                stream,
                kind,
                offset: offset + start as u64,
                data: data.slice(start..end),
                last: last && index + 1 == count,
                entry,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_limits_the_sender() {
        let mut credit = Credit::new(100);
        credit.spend(60).unwrap();
        assert_eq!(credit.available(), 40);
        assert!(credit.spend(41).is_err());
        assert!(!credit.grant(90));
        assert!(credit.grant(200));
        credit.spend(140).unwrap();
        assert_eq!(credit.sent(), 200);
    }

    #[test]
    fn window_rejects_gaps_and_overruns_and_grants_after_delivery() {
        let mut window = Window::new();
        assert!(window.receive(1, 10).is_err());
        window.receive(0, CHUNK).unwrap();
        assert!(window.receive(CHUNK as u64, INITIAL_WINDOW as usize).is_err());
        // Small deliveries do not produce a FLOW for every chunk.
        assert_eq!(window.deliver(CHUNK), None);
        let mut offset = CHUNK as u64;
        let mut granted = None;
        while granted.is_none() {
            window.receive(offset, CHUNK).unwrap();
            offset += CHUNK as u64;
            granted = window.deliver(CHUNK);
        }
        assert!(granted.unwrap() >= INITIAL_WINDOW + INITIAL_WINDOW / 4);
    }

    #[test]
    fn window_grows_when_delivery_is_fast() {
        let mut window = Window::new();
        let mut offset = 0;
        while offset < 3 * INITIAL_WINDOW {
            window.receive(offset, CHUNK).unwrap();
            offset += CHUNK as u64;
            window.deliver(CHUNK);
        }
        assert!(window.window() > INITIAL_WINDOW);
        assert!(window.window() <= MAX_WINDOW);
    }

    #[test]
    fn chunking_covers_the_data_and_marks_the_end() {
        let data = bytes::Bytes::from(vec![7u8; CHUNK * 2 + 5]);
        let parts = chunks(3, kind::FILE, 0, 100, data, true);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[1].offset, 100 + CHUNK as u64);
        assert_eq!(parts[2].data.len(), 5);
        assert!(parts[2].last && !parts[0].last);
        let empty = chunks(3, kind::FILE, 1, 9, bytes::Bytes::new(), true);
        assert_eq!(empty.len(), 1);
        assert!(empty[0].last);
    }
}
