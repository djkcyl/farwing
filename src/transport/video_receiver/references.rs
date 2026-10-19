//! Frame dependencies and decode-order admission.
use super::packets::{EncodedFrame, SequenceUnwrapper};
use super::{DECODED_HISTORY_SIZE, FRAME_BUFFER_MAX_SIZE, MAX_STASHED_FRAMES, ahead_of};
use std::collections::{BTreeMap, HashSet, VecDeque};

pub(super) struct GoP {
    pub(super) key_sequence: u16,
    pub(super) last_picture_sequence: u16,
    pub(super) last_with_padding: u16,
}

pub(super) struct SeqNumOnlyRefFinder {
    pub(super) gops: Vec<GoP>,
    pub(super) stashed: VecDeque<EncodedFrame>,
    pub(super) stashed_padding: HashSet<u16>,
    pub(super) unwrapper: SequenceUnwrapper,
    /// First sequence number of a recovery frame just released: older
    /// stashed frames can never decode.
    released_recovery: Option<u16>,
}

/// Whether a recovery frame only depends on frames the decoder holds.
pub(super) type Admits<'a> = &'a dyn Fn(&crate::media::frame_meta::FrameMeta) -> bool;

pub(super) struct FrameInfo {
    pub(super) frame: EncodedFrame,
    pub(super) continuous: bool,
}

pub(super) struct FrameBuffer {
    pub(super) frames: BTreeMap<i64, FrameInfo>,
    pub(super) decoded: HashSet<i64>,
    pub(super) decoded_order: VecDeque<i64>,
    pub(super) last_decoded_id: Option<i64>,
    pub(super) last_decoded_timestamp: Option<u32>,
    pub(super) last_continuous_frame_id: Option<i64>,
    pub(super) keyframe_required: bool,
    pub(super) dropped: u64,
}

pub(super) enum FrameDecision {
    HandOff(EncodedFrame),
    Stash(EncodedFrame),
    Drop,
}

#[derive(Default)]
pub(super) struct ReferenceResult {
    pub(super) frames: Vec<EncodedFrame>,
    pub(super) stashed: u64,
    pub(super) dropped: u64,
}

impl SeqNumOnlyRefFinder {
    pub(super) fn new() -> Self {
        Self::with_unwrap_anchor_opt(None)
    }

    pub(super) fn with_unwrap_anchor(anchor: i64) -> Self {
        Self::with_unwrap_anchor_opt(Some(anchor))
    }

    pub(super) fn with_unwrap_anchor_opt(anchor: Option<i64>) -> Self {
        Self {
            gops: Vec::new(),
            stashed: VecDeque::new(),
            stashed_padding: HashSet::new(),
            unwrapper: SequenceUnwrapper { last: anchor },
            released_recovery: None,
        }
    }

    pub(super) fn manage(&mut self, frame: EncodedFrame, admits: Admits<'_>) -> ReferenceResult {
        let decision = self.manage_internal(frame, admits);
        let mut output = ReferenceResult::default();
        match decision {
            FrameDecision::HandOff(frame) => {
                output.frames.push(frame);
                self.retry_stashed(&mut output, admits);
            }
            FrameDecision::Stash(frame) => {
                if self.stashed.len() > MAX_STASHED_FRAMES {
                    self.stashed.pop_back();
                    output.dropped += 1;
                }
                self.stashed.push_front(frame);
                output.stashed += 1;
            }
            FrameDecision::Drop => output.dropped += 1,
        }
        output
    }

    pub(super) fn manage_internal(&mut self, mut frame: EncodedFrame, admits: Admits<'_>) -> FrameDecision {
        if frame.keyframe {
            if let Some(gop) = self
                .gops
                .iter_mut()
                .find(|gop| gop.key_sequence == frame.last_sequence_number)
            {
                gop.last_picture_sequence = frame.last_sequence_number;
                gop.last_with_padding = frame.last_sequence_number;
            } else {
                self.gops.push(GoP {
                    key_sequence: frame.last_sequence_number,
                    last_picture_sequence: frame.last_sequence_number,
                    last_with_padding: frame.last_sequence_number,
                });
            }
        }
        if self.gops.is_empty() {
            return FrameDecision::Stash(frame);
        }
        let keep_key = self
            .find_gop_index(frame.last_sequence_number)
            .map(|index| self.gops[index].key_sequence);
        if self.gops.len() > 1 {
            self.gops.retain(|gop| {
                Some(gop.key_sequence) == keep_key
                    || ahead_of(gop.key_sequence, frame.last_sequence_number)
                    || frame.last_sequence_number.wrapping_sub(gop.key_sequence) <= 100
            });
        }
        let Some(gop_index) = self.find_gop_index(frame.last_sequence_number) else {
            return FrameDecision::Drop;
        };
        let gop = &mut self.gops[gop_index];
        if !frame.keyframe && frame.first_sequence_number.wrapping_sub(1) != gop.last_with_padding {
            // A recovery frame after a loss only needs references the
            // decoder holds: it starts a new chain, as a keyframe would.
            if frame.frame_meta.as_ref().is_some_and(|meta| admits(meta)) {
                self.gops.push(GoP {
                    key_sequence: frame.last_sequence_number,
                    last_picture_sequence: frame.last_sequence_number,
                    last_with_padding: frame.last_sequence_number,
                });
                frame.id = self.unwrapper.unwrap(frame.last_sequence_number);
                self.released_recovery = Some(frame.first_sequence_number);
                self.update_last_picture_with_padding(frame.last_sequence_number);
                return FrameDecision::HandOff(frame);
            }
            return FrameDecision::Stash(frame);
        }
        let last_reference = gop.last_picture_sequence;
        if !frame.keyframe {
            let reference = self.unwrapper.unwrap(last_reference);
            frame.references.push(reference);
        }
        frame.id = self.unwrapper.unwrap(frame.last_sequence_number);
        if ahead_of(frame.last_sequence_number, gop.last_picture_sequence) {
            gop.last_picture_sequence = frame.last_sequence_number;
            gop.last_with_padding = frame.last_sequence_number;
        }
        self.update_last_picture_with_padding(frame.last_sequence_number);
        FrameDecision::HandOff(frame)
    }

    pub(super) fn find_gop_index(&self, sequence_number: u16) -> Option<usize> {
        self.gops
            .iter()
            .enumerate()
            .filter(|(_, gop)| !ahead_of(gop.key_sequence, sequence_number))
            .min_by_key(|(_, gop)| sequence_number.wrapping_sub(gop.key_sequence))
            .map(|(index, _)| index)
    }

    pub(super) fn padding_received(&mut self, sequence_number: u16) -> ReferenceResult {
        self.stashed_padding.retain(|value| {
            ahead_of(*value, sequence_number) || sequence_number.wrapping_sub(*value) <= 100
        });
        self.stashed_padding.insert(sequence_number);
        self.update_last_picture_with_padding(sequence_number);
        let mut output = ReferenceResult::default();
        self.retry_stashed(&mut output, &|_| false);
        output
    }

    pub(super) fn update_last_picture_with_padding(&mut self, sequence_number: u16) {
        let Some(gop_index) = self.find_gop_index(sequence_number) else {
            return;
        };
        loop {
            let next = self.gops[gop_index].last_with_padding.wrapping_add(1);
            if !self.stashed_padding.remove(&next) {
                break;
            }
            self.gops[gop_index].last_with_padding = next;
        }
        if sequence_number.wrapping_sub(self.gops[gop_index].key_sequence) > 10_000
            && !ahead_of(self.gops[gop_index].key_sequence, sequence_number)
        {
            let last_picture_sequence = self.gops[gop_index].last_picture_sequence;
            let last_with_padding = self.gops[gop_index].last_with_padding;
            self.gops.clear();
            self.gops.push(GoP {
                key_sequence: sequence_number,
                last_picture_sequence,
                last_with_padding,
            });
        }
    }

    pub(super) fn retry_stashed(&mut self, output: &mut ReferenceResult, admits: Admits<'_>) {
        loop {
            let mut progress = false;
            let mut remaining = VecDeque::new();
            while let Some(frame) = self.stashed.pop_front() {
                match self.manage_internal(frame, admits) {
                    FrameDecision::HandOff(frame) => {
                        output.frames.push(frame);
                        progress = true;
                    }
                    FrameDecision::Stash(frame) => remaining.push_back(frame),
                    FrameDecision::Drop => output.dropped += 1,
                }
            }
            self.stashed = remaining;
            if let Some(sequence) = self.released_recovery.take() {
                output.dropped += self.stashed.len() as u64;
                self.clear_to(sequence);
                output.dropped -= self.stashed.len() as u64;
            }
            if !progress {
                break;
            }
        }
    }

    pub(super) fn clear_to(&mut self, sequence_number: u16) {
        self.stashed
            .retain(|frame| !ahead_of(sequence_number, frame.first_sequence_number));
    }
}

impl FrameBuffer {
    pub(super) fn new() -> Self {
        Self {
            frames: BTreeMap::new(),
            decoded: HashSet::new(),
            decoded_order: VecDeque::with_capacity(DECODED_HISTORY_SIZE),
            last_decoded_id: None,
            last_decoded_timestamp: None,
            last_continuous_frame_id: None,
            keyframe_required: true,
            dropped: 0,
        }
    }

    pub(super) fn insert(&mut self, frame: EncodedFrame) -> Option<i64> {
        let mut references = HashSet::with_capacity(frame.references.len());
        if frame
            .references
            .iter()
            .any(|reference| *reference >= frame.id || !references.insert(*reference))
        {
            self.dropped += 1;
            return self.last_continuous_frame_id;
        }
        if self.last_decoded_id.is_some_and(|last| frame.id <= last) {
            let newer_keyframe = frame.keyframe
                && self
                    .last_decoded_timestamp
                    .is_some_and(|last| rtp_timestamp_ahead_of(frame.timestamp, last));
            if newer_keyframe {
                self.clear();
            } else {
                self.dropped += 1;
                return self.last_continuous_frame_id;
            }
        }
        if self.frames.contains_key(&frame.id) {
            return self.last_continuous_frame_id;
        }
        if self.frames.len() == FRAME_BUFFER_MAX_SIZE {
            if frame.keyframe {
                self.dropped += self.frames.len() as u64;
                self.clear();
            } else {
                self.dropped += 1;
                return self.last_continuous_frame_id;
            }
        }
        let continuous = self.references_continuous(&frame);
        let id = frame.id;
        self.frames.insert(id, FrameInfo { frame, continuous });
        self.propagate_continuity(id);
        self.last_continuous_frame_id
    }

    pub(super) fn references_continuous(&self, frame: &EncodedFrame) -> bool {
        frame.references.iter().all(|reference| {
            self.decoded.contains(reference)
                || self
                    .frames
                    .get(reference)
                    .is_some_and(|value| value.continuous)
        })
    }

    pub(super) fn propagate_continuity(&mut self, start: i64) {
        let ids = self
            .frames
            .range(start..)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in ids {
            let continuous = self
                .frames
                .get(&id)
                .is_some_and(|info| info.continuous || self.references_continuous(&info.frame));
            if continuous && let Some(info) = self.frames.get_mut(&id) {
                info.continuous = true;
                self.last_continuous_frame_id = Some(
                    self.last_continuous_frame_id
                        .map_or(id, |last| last.max(id)),
                );
            }
        }
    }

    pub(super) fn extract_next_decodable(&mut self) -> Option<EncodedFrame> {
        loop {
            let id = self.frames.iter().find_map(|(id, info)| {
                (info.continuous
                    && info
                        .frame
                        .references
                        .iter()
                        .all(|reference| self.decoded.contains(reference)))
                .then_some(*id)
            })?;
            let frame = self.frames.remove(&id)?.frame;
            if self.keyframe_required && !frame.keyframe {
                self.dropped += 1;
                continue;
            }
            if frame.keyframe {
                self.keyframe_required = false;
            }
            self.mark_decoded(id, frame.timestamp);
            return Some(frame);
        }
    }

    pub(super) fn next_and_last_decodable(&self) -> Option<(i64, i64)> {
        let mut decodable = self.frames.iter().filter_map(|(id, info)| {
            (info.continuous
                && info
                    .frame
                    .references
                    .iter()
                    .all(|reference| self.decoded.contains(reference)))
            .then_some(*id)
        });
        let first = decodable.next()?;
        Some((first, decodable.next_back().unwrap_or(first)))
    }

    pub(super) fn drop_next_decodable(&mut self) -> usize {
        let Some((next, _)) = self.next_and_last_decodable() else {
            return 0;
        };
        let remove = self
            .frames
            .range(..=next)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        let mut dropped = 0;
        for id in remove {
            if self.frames.remove(&id).is_some() {
                self.dropped += 1;
                dropped += 1;
            }
        }
        self.recompute_last_continuous();
        dropped
    }

    pub(super) fn recompute_last_continuous(&mut self) {
        self.last_continuous_frame_id = self
            .frames
            .iter()
            .filter_map(|(id, info)| info.continuous.then_some(*id))
            .next_back();
    }

    pub(super) fn mark_decoded(&mut self, id: i64, timestamp: u32) {
        self.decoded.insert(id);
        self.decoded_order.push_back(id);
        self.last_decoded_id = Some(id);
        self.last_decoded_timestamp = Some(timestamp);
        while self.decoded_order.len() > DECODED_HISTORY_SIZE {
            if let Some(oldest) = self.decoded_order.pop_front() {
                self.decoded.remove(&oldest);
            }
        }
        self.propagate_continuity(id);
    }

    pub(super) fn clear(&mut self) {
        self.frames.clear();
        self.decoded.clear();
        self.decoded_order.clear();
        self.last_decoded_id = None;
        self.last_decoded_timestamp = None;
        self.last_continuous_frame_id = None;
        self.keyframe_required = true;
    }
}

pub(super) fn rtp_timestamp_ahead_of(newer: u32, older: u32) -> bool {
    let distance = newer.wrapping_sub(older);
    distance != 0 && distance < 0x8000_0000
}
