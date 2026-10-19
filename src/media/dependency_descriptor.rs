//! Dependency Descriptor RTP header extension (AV1 RTP specification,
//! appendix A), written for browser viewers so that libwebrtc decides
//! decodability from declared frame dependencies instead of sequence
//! continuity: a recovery frame that depends only on a long-term reference
//! decodes after a loss (`docs/protocol/rfi-ltr-brief.md` §4.1).
//!
//! One decode target and one layer; template 0 is a keyframe, template 1
//! depends on the previous frame; a recovery frame uses template 1 with a
//! custom frame diff to its reference. No chains, no resolutions.

use std::collections::VecDeque;

pub(crate) const URI: &str = "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension";
/// Largest descriptor written: a keyframe's first packet with the structure.
pub(crate) const MAX_SIZE: usize = 8;

const KEY_TEMPLATE: u64 = 0;
const DELTA_TEMPLATE: u64 = 1;
/// Decode target indication "switch" (table A.1).
const SWITCH: u64 = 2;

/// What a frame depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dependency {
    /// Nothing: a keyframe (its first packet carries the structure).
    Keyframe,
    /// The frame this many frames back (1: the previous frame).
    Back(u16),
}

#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    used: usize,
}

impl Bits {
    fn put(&mut self, value: u64, width: usize) {
        for bit in (0..width).rev() {
            if self.used % 8 == 0 {
                self.bytes.push(0);
            }
            if value >> bit & 1 == 1 {
                *self.bytes.last_mut().expect("pushed above") |= 0x80 >> (self.used % 8);
            }
            self.used += 1;
        }
    }
}

/// The descriptor of packet `index` of the `count` packets of frame `number`.
pub(crate) fn encode(number: u16, dependency: Dependency, index: usize, count: usize) -> Vec<u8> {
    let mut bits = Bits::default();
    bits.put(u64::from(index == 0), 1);
    bits.put(u64::from(index + 1 == count), 1);
    bits.put(
        if dependency == Dependency::Keyframe {
            KEY_TEMPLATE
        } else {
            DELTA_TEMPLATE
        },
        6,
    );
    bits.put(u64::from(number), 16);
    match dependency {
        // The structure only on the first packet (libwebrtc drops it
        // elsewhere), and it marks the frame as a keyframe.
        Dependency::Keyframe if index == 0 => {
            // template_dependency_structure_present_flag only.
            bits.put(0b10000, 5);
            structure(&mut bits);
        }
        Dependency::Back(back) if back > 1 => {
            // custom_fdiffs_flag only.
            bits.put(0b00010, 5);
            let minus_one = u64::from(back - 1);
            let size = match minus_one {
                0..=0xF => 1,
                0x10..=0xFF => 2,
                _ => 3,
            };
            bits.put(size, 2);
            bits.put(minus_one, 4 * size as usize);
            bits.put(0, 2);
        }
        _ => {}
    }
    bits.bytes
}

fn structure(bits: &mut Bits) {
    bits.put(0, 6); // template_id_offset
    bits.put(0, 5); // dt_cnt_minus_one: one decode target
    // template_layers: both templates in spatial and temporal layer 0.
    bits.put(0, 2);
    bits.put(3, 2);
    // template_dtis
    bits.put(SWITCH, 2);
    bits.put(SWITCH, 2);
    // template_fdiffs: the keyframe has none, the delta frame [1].
    bits.put(0, 1);
    bits.put(1, 1);
    bits.put(0, 4);
    bits.put(0, 1);
    // template_chains: chain_cnt = ns(2) = 0.
    bits.put(0, 1);
    // resolutions_present_flag
    bits.put(0, 1);
}

/// Descriptor frame numbers of one RTP stream: consecutive per frame sent,
/// so they never go back when the encoder (and its `frame-meta` numbering)
/// restarts.
#[derive(Default)]
pub(crate) struct Numbering {
    next: u16,
    /// (`frame-meta` epoch, frame id, descriptor number) of recent frames.
    sent: VecDeque<(u8, u16, u16)>,
}

impl Numbering {
    /// Number and dependency of the next frame sent on this stream.
    pub(crate) fn frame(
        &mut self,
        keyframe: bool,
        meta: Option<crate::media::frame_meta::FrameMeta>,
    ) -> (u16, Dependency) {
        let number = self.next;
        self.next = self.next.wrapping_add(1);
        let dependency = if keyframe {
            Dependency::Keyframe
        } else {
            match meta.filter(|meta| meta.recovery()).and_then(|meta| {
                self.sent
                    .iter()
                    .rev()
                    .find(|(epoch, id, _)| *epoch == meta.epoch && *id == meta.ref_id)
            }) {
                Some((_, _, reference)) => Dependency::Back(number.wrapping_sub(*reference)),
                None => Dependency::Back(1),
            }
        };
        if let Some(meta) = meta {
            self.sent.push_back((meta.epoch, meta.frame_id, number));
            if self.sent.len() > 4096 {
                self.sent.pop_front();
            }
        }
        (number, dependency)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parts of a descriptor these tests check, read as the
    /// specification's syntax reads them.
    #[derive(Debug, PartialEq)]
    struct Parsed {
        start: bool,
        end: bool,
        template: u64,
        number: u64,
        structure: bool,
        fdiffs: Vec<u64>,
    }

    struct Reader<'a> {
        bytes: &'a [u8],
        at: usize,
    }

    impl Reader<'_> {
        fn f(&mut self, width: usize) -> u64 {
            let mut value = 0;
            for _ in 0..width {
                let bit = self.bytes[self.at / 8] >> (7 - self.at % 8) & 1;
                value = value << 1 | u64::from(bit);
                self.at += 1;
            }
            value
        }

        fn ns(&mut self, n: u64) -> u64 {
            let w = 64 - n.leading_zeros() as usize;
            let m = (1 << w) - n;
            let v = self.f(w - 1);
            if v < m {
                return v;
            }
            (v << 1) - m + self.f(1)
        }
    }

    /// `template_fdiffs` of the structure last read.
    fn parse(bytes: &[u8], templates: &mut Vec<Vec<u64>>) -> Parsed {
        let mut r = Reader { bytes, at: 0 };
        let start = r.f(1) == 1;
        let end = r.f(1) == 1;
        let template = r.f(6);
        let number = r.f(16);
        let (mut structure, mut custom_fdiffs) = (false, false);
        if bytes.len() > 3 {
            structure = r.f(1) == 1;
            let active = r.f(1) == 1;
            assert_eq!(r.f(1), 0, "custom dtis");
            custom_fdiffs = r.f(1) == 1;
            assert_eq!(r.f(1), 0, "custom chains");
            if structure {
                assert_eq!(r.f(6), 0, "template_id_offset");
                let dt_cnt = r.f(5) + 1;
                let mut count = 0;
                loop {
                    count += 1;
                    let next = r.f(2);
                    assert!(next == 0 || next == 3, "one layer");
                    if next == 3 {
                        break;
                    }
                }
                for _ in 0..count * dt_cnt {
                    assert_eq!(r.f(2), SWITCH);
                }
                templates.clear();
                for _ in 0..count {
                    let mut fdiffs = Vec::new();
                    while r.f(1) == 1 {
                        fdiffs.push(r.f(4) + 1);
                    }
                    templates.push(fdiffs);
                }
                assert_eq!(r.ns(dt_cnt + 1), 0, "no chains");
                assert_eq!(r.f(1), 0, "no resolutions");
            }
            assert!(!active);
        }
        let fdiffs = if custom_fdiffs {
            let mut fdiffs = Vec::new();
            loop {
                let size = r.f(2) as usize;
                if size == 0 {
                    break;
                }
                fdiffs.push(r.f(4 * size) + 1);
            }
            fdiffs
        } else {
            templates[template as usize].clone()
        };
        // Zero padding only.
        assert!(r.at <= bytes.len() * 8 && bytes.len() * 8 - r.at < 8);
        while r.at < bytes.len() * 8 {
            assert_eq!(r.f(1), 0);
        }
        Parsed {
            start,
            end,
            template,
            number,
            structure,
            fdiffs,
        }
    }

    #[test]
    fn keyframes_carry_the_structure_on_their_first_packet() {
        let mut templates = Vec::new();
        let first = encode(500, Dependency::Keyframe, 0, 3);
        assert!(first.len() <= MAX_SIZE);
        assert_eq!(
            parse(&first, &mut templates),
            Parsed {
                start: true,
                end: false,
                template: 0,
                number: 500,
                structure: true,
                fdiffs: vec![],
            }
        );
        assert_eq!(templates, vec![vec![], vec![1]]);
        let last = encode(500, Dependency::Keyframe, 2, 3);
        assert_eq!(last.len(), 3);
        let parsed = parse(&last, &mut templates);
        assert!(!parsed.start && parsed.end && !parsed.structure && parsed.template == 0);
    }

    #[test]
    fn delta_and_recovery_frames_declare_their_reference() {
        let mut templates = Vec::new();
        parse(&encode(1, Dependency::Keyframe, 0, 1), &mut templates);
        let delta = encode(2, Dependency::Back(1), 0, 1);
        assert_eq!(delta.len(), 3);
        assert_eq!(parse(&delta, &mut templates).fdiffs, vec![1]);
        for back in [2u16, 16, 17, 256, 257, 4000] {
            let bytes = encode(9000, Dependency::Back(back), 1, 2);
            assert!(bytes.len() <= MAX_SIZE);
            let parsed = parse(&bytes, &mut templates);
            assert_eq!((parsed.template, parsed.fdiffs), (1, vec![u64::from(back)]));
            assert!(!parsed.start && parsed.end);
        }
    }

    #[test]
    fn numbering_maps_recovery_references_to_descriptor_numbers() {
        use crate::media::frame_meta::{Counter, flags};
        let mut numbering = Numbering::default();
        let mut counter = Counter::default();
        let key = counter.next(true);
        assert_eq!(numbering.frame(true, Some(key)), (0, Dependency::Keyframe));
        let reference = counter.next(false);
        assert_eq!(numbering.frame(false, Some(reference)).1, Dependency::Back(1));
        for _ in 0..5 {
            numbering.frame(false, Some(counter.next(false)));
        }
        let mut recovery = counter.next(false);
        recovery.flags |= flags::RECOVERY | flags::REF_LTR;
        recovery.ref_id = reference.frame_id;
        assert_eq!(numbering.frame(false, Some(recovery)), (7, Dependency::Back(6)));
        // Unknown reference (never sent here): the previous frame.
        recovery.ref_id = reference.frame_id.wrapping_sub(100);
        assert_eq!(numbering.frame(false, Some(recovery)).1, Dependency::Back(1));
    }
}
