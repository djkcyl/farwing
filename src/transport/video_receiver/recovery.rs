//! What the controller has decoded, in `frame-meta` terms, and whether a
//! recovery frame may skip a gap (`docs/protocol/rfi-ltr-brief.md` §3.2).

use crate::media::frame_meta::FrameMeta;

/// At most this many long-term references are tracked (slots 0–3).
const SLOTS: usize = 4;

#[derive(Debug, Default)]
pub(crate) struct RecoveryState {
    epoch: Option<u8>,
    /// `D`: newest `frame_id` of the continuously decoded chain.
    decoded: Option<u16>,
    /// `L`: decoded long-term references by slot.
    held: [Option<u16>; SLOTS],
    /// Slot order, newest mark first.
    order: Vec<usize>,
}

/// What the receiver tells the host after a decoded frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub epoch: u8,
    pub decoded: u16,
    /// Newest first.
    pub held: Vec<u16>,
}

fn newer(id: u16, than: u16) -> bool {
    let distance = id.wrapping_sub(than);
    distance != 0 && distance < 0x8000
}

impl RecoveryState {
    /// A frame decoded. True when it marked a long-term reference, which the
    /// host should hear about.
    pub(crate) fn decoded(&mut self, meta: FrameMeta) -> bool {
        if meta.keyframe() || self.epoch != Some(meta.epoch) {
            self.held = [None; SLOTS];
            self.order.clear();
            self.epoch = Some(meta.epoch);
        }
        self.decoded = Some(meta.frame_id);
        if meta.marks_ltr() {
            let slot = usize::from(meta.ltr_slot());
            self.held[slot] = Some(meta.frame_id);
            self.order.retain(|s| *s != slot);
            self.order.insert(0, slot);
            return true;
        }
        false
    }

    /// The decoder reset: its references are gone.
    pub(crate) fn decoder_reset(&mut self) {
        self.held = [None; SLOTS];
        self.order.clear();
    }

    /// Whether recovery frame `meta` only depends on frames this decoder has.
    pub(crate) fn admits(&self, meta: &FrameMeta) -> bool {
        if !meta.recovery() || self.epoch != Some(meta.epoch) {
            return false;
        }
        if meta.refs_ltr() {
            self.held.contains(&Some(meta.ref_id))
        } else {
            self.decoded
                .is_some_and(|decoded| !newer(meta.ref_id, decoded))
        }
    }

    pub(crate) fn snapshot(&self) -> Option<Snapshot> {
        Some(Snapshot {
            epoch: self.epoch?,
            decoded: self.decoded?,
            held: self.order.iter().filter_map(|slot| self.held[*slot]).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::frame_meta::flags;

    fn meta(flags: u8, frame_id: u16, ref_id: u16) -> FrameMeta {
        FrameMeta {
            flags,
            epoch: 3,
            frame_id,
            ref_id,
        }
    }

    #[test]
    fn tracks_decoded_chain_and_long_term_references() {
        let mut state = RecoveryState::default();
        assert!(state.snapshot().is_none());
        assert!(state.decoded(meta(flags::KEY, 1000, 1000).with_ltr_slot(0)));
        assert!(!state.decoded(meta(0, 1001, 1000)));
        assert!(state.decoded(meta(0, 1240, 1239).with_ltr_slot(1)));
        assert_eq!(
            state.snapshot(),
            Some(Snapshot {
                epoch: 3,
                decoded: 1240,
                held: vec![1240, 1000],
            })
        );
        // A newer mark replaces the slot's old reference.
        state.decoded(meta(0, 1300, 1299).with_ltr_slot(0));
        assert_eq!(state.snapshot().unwrap().held, vec![1300, 1240]);
        // Recovery frames are admitted only on what is held.
        assert!(state.admits(&meta(flags::RECOVERY | flags::REF_LTR, 1350, 1240)));
        assert!(!state.admits(&meta(flags::RECOVERY | flags::REF_LTR, 1350, 1000)));
        assert!(!state.admits(&meta(flags::REF_LTR, 1350, 1240)));
        // Invalidation mode: depends on frames not newer than D.
        assert!(state.admits(&meta(flags::RECOVERY, 1350, 1300)));
        assert!(!state.admits(&meta(flags::RECOVERY, 1350, 1301)));
        // Another epoch, or a decoder reset, admits nothing.
        let mut other = meta(flags::RECOVERY | flags::REF_LTR, 1350, 1240);
        other.epoch = 4;
        assert!(!state.admits(&other));
        state.decoder_reset();
        assert!(!state.admits(&meta(flags::RECOVERY | flags::REF_LTR, 1350, 1240)));
        // A keyframe starts over with only its own mark.
        state.decoded(meta(flags::KEY, 1400, 1400));
        assert_eq!(state.snapshot().unwrap().held, Vec::<u16>::new());
    }
}
