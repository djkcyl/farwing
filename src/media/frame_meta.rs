//! Per-frame identity on the first RTP packet of a video frame
//! (`urn:farwing:rtp-hdrext:frame-meta`, `docs/protocol/rfi-ltr-brief.md`
//! §6.2): encoder epoch, frame number and the frame it depends on, so a
//! receiver can tell that a frame after a loss only needs older good frames.

pub(crate) const URI: &str = "urn:farwing:rtp-hdrext:frame-meta";
/// Encoded size of the extension element's value.
pub(crate) const SIZE: usize = 6;

pub(crate) mod flags {
    /// A keyframe (IDR / key frame).
    pub const KEY: u8 = 0x01;
    /// A recovery frame: depends only on `ref_id` and older frames.
    pub const RECOVERY: u8 = 0x02;
    /// This frame is kept as a long-term reference in slot `(flags >> 4) & 3`.
    pub const MARK_LTR: u8 = 0x04;
    /// With RECOVERY: `ref_id` is a long-term reference.
    pub const REF_LTR: u8 = 0x08;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameMeta {
    pub flags: u8,
    /// Encoder epoch: a new encoder, format or group leader starts a new one.
    pub epoch: u8,
    /// Frame number in encoding order within the epoch (wraps).
    pub frame_id: u16,
    /// Ordinary frames: `frame_id - 1`; keyframes: `frame_id`; recovery
    /// frames: the newest frame this and later frames depend on.
    pub ref_id: u16,
}

impl FrameMeta {
    pub fn keyframe(&self) -> bool {
        self.flags & flags::KEY != 0
    }
    pub fn recovery(&self) -> bool {
        self.flags & flags::RECOVERY != 0
    }
    pub fn marks_ltr(&self) -> bool {
        self.flags & flags::MARK_LTR != 0
    }
    pub fn refs_ltr(&self) -> bool {
        self.flags & flags::REF_LTR != 0
    }
    /// The long-term slot this frame is marked into.
    pub fn ltr_slot(&self) -> u8 {
        (self.flags >> 4) & 3
    }
    pub fn with_ltr_slot(mut self, slot: u8) -> Self {
        self.flags = (self.flags & !0x30) | flags::MARK_LTR | ((slot & 3) << 4);
        self
    }

    pub fn encode(&self) -> [u8; SIZE] {
        let id = self.frame_id.to_be_bytes();
        let reference = self.ref_id.to_be_bytes();
        [self.flags & 0x3f, self.epoch, id[0], id[1], reference[0], reference[1]]
    }

    pub fn decode(value: &[u8]) -> Option<Self> {
        if value.len() != SIZE || value[0] & 0xc0 != 0 {
            return None;
        }
        Some(Self {
            flags: value[0],
            epoch: value[1],
            frame_id: u16::from_be_bytes([value[2], value[3]]),
            ref_id: u16::from_be_bytes([value[4], value[5]]),
        })
    }
}

/// Epochs come from one process-wide sequence, so a new group leader never
/// reuses the epoch of the encoder it replaces.
static NEXT_EPOCH: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Numbers the frames of one encoder on the host.
#[derive(Debug, Default)]
pub(crate) struct Counter {
    epoch: u8,
    next: u16,
    started: bool,
}

impl Counter {
    /// A new encoder (or format, or group leader): a new epoch.
    pub fn restart(&mut self) {
        self.epoch = NEXT_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.started = true;
        self.next = 0;
    }

    /// The current epoch.
    pub fn epoch(&self) -> u8 {
        self.epoch
    }

    /// The identity of the next output frame.
    pub fn next(&mut self, keyframe: bool) -> FrameMeta {
        if !self.started {
            self.restart();
        }
        let frame_id = self.next;
        self.next = self.next.wrapping_add(1);
        FrameMeta {
            flags: if keyframe { flags::KEY } else { 0 },
            epoch: self.epoch,
            frame_id,
            ref_id: if keyframe { frame_id } else { frame_id.wrapping_sub(1) },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
    }

    // Byte examples of `docs/protocol/rfi-ltr-brief.md` §6.2 (values only).
    #[test]
    fn brief_examples() {
        let key = FrameMeta {
            flags: flags::KEY,
            epoch: 3,
            frame_id: 1000,
            ref_id: 1000,
        }
        .with_ltr_slot(0);
        assert_eq!(hex(&key.encode()), "05 03 03 e8 03 e8");
        let p = FrameMeta {
            flags: 0,
            epoch: 3,
            frame_id: 1201,
            ref_id: 1200,
        };
        assert_eq!(hex(&p.encode()), "00 03 04 b1 04 b0");
        let mark = FrameMeta {
            flags: 0,
            epoch: 3,
            frame_id: 1240,
            ref_id: 1239,
        }
        .with_ltr_slot(1);
        assert_eq!(hex(&mark.encode()), "14 03 04 d8 04 d7");
        assert_eq!(mark.ltr_slot(), 1);
        let recovery = FrameMeta {
            flags: flags::RECOVERY | flags::REF_LTR,
            epoch: 3,
            frame_id: 1250,
            ref_id: 1180,
        };
        assert_eq!(hex(&recovery.encode()), "0a 03 04 e2 04 9c");
        for meta in [key, p, mark, recovery] {
            assert_eq!(FrameMeta::decode(&meta.encode()), Some(meta));
        }
        // Reserved bits and wrong sizes are rejected.
        assert_eq!(FrameMeta::decode(&[0x40, 0, 0, 0, 0, 0]), None);
        assert_eq!(FrameMeta::decode(&[0, 0, 0]), None);
    }

    #[test]
    fn counter_numbers_frames_per_epoch() {
        let mut counter = Counter::default();
        let first = counter.next(true);
        assert_eq!((first.frame_id, first.ref_id, first.keyframe()), (0, 0, true));
        let second = counter.next(false);
        assert_eq!((second.epoch, second.frame_id, second.ref_id, second.keyframe()), (first.epoch, 1, 0, false));
        counter.restart();
        let restarted = counter.next(true);
        assert_ne!(restarted.epoch, first.epoch);
        assert_eq!(restarted.frame_id, 0);
    }
}
