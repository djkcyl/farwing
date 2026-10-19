// SPDX-License-Identifier: MIT
// Derived from oxideav-h264 0.1.8, Copyright (c) 2026 Karpelès Lab Inc.
// Retained syntax/metadata primitives used by this client; see COPYING.OxideAV in this crate.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefMarking {
    ShortTerm,
    LongTerm,
    Unused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PicStructure {
    TopField,
    BottomField,
    Frame,
    FieldPair,
}

impl PicStructure {
    pub fn is_field(self) -> bool {
        matches!(self, PicStructure::TopField | PicStructure::BottomField)
    }

    pub fn is_bottom(self) -> bool {
        matches!(self, PicStructure::BottomField)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldParity {
    Top,
    Bottom,
}

#[derive(Debug, Clone)]
pub struct DpbEntry {
    pub frame_num: u32,
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    pub pic_order_cnt: i32,
    pub structure: PicStructure,
    pub marking: RefMarking,
    pub long_term_frame_idx: u32,
    pub dpb_key: u32,
    pub field_markings: [RefMarking; 2],
}

impl DpbEntry {
    pub fn pic_num(
        &self,
        current_frame_num: u32,
        max_frame_num: u32,
        current_is_field: bool,
        current_bottom: bool,
    ) -> i32 {
        // eq. 8-27 — FrameNumWrap = FrameNum - MaxFrameNum when
        // FrameNum > frame_num (current), else FrameNum.
        let frame_num_wrap = if self.frame_num > current_frame_num {
            self.frame_num as i64 - max_frame_num as i64
        } else {
            self.frame_num as i64
        };

        if !current_is_field {
            // Frame picture — eq. 8-28: PicNum = FrameNumWrap.
            frame_num_wrap as i32
        } else {
            // Field picture — eq. 8-30 / 8-31. Same parity gets the
            // "+1" boost, opposite parity doesn't. For a reference
            // frame used as a field reference, both parities are
            // considered: we pick the one whose parity matches the
            // referencing field's parity. For field pair / single
            // field DPB entries, the entry's own structure determines
            // parity.
            let same_parity = match self.structure {
                PicStructure::TopField => !current_bottom,
                PicStructure::BottomField => current_bottom,
                // Frame / field pair: treat as if its parity matches
                // the current field (both fields available).
                PicStructure::Frame | PicStructure::FieldPair => true,
            };
            if same_parity {
                (2 * frame_num_wrap + 1) as i32
            } else {
                (2 * frame_num_wrap) as i32
            }
        }
    }

    fn field_pic_num(
        &self,
        field_parity: FieldParity,
        current_frame_num: u32,
        max_frame_num: u32,
        current_bottom: bool,
    ) -> i32 {
        let frame_num_wrap = if self.frame_num > current_frame_num {
            self.frame_num as i64 - max_frame_num as i64
        } else {
            self.frame_num as i64
        };
        let same_parity = (field_parity == FieldParity::Bottom) == current_bottom;
        if same_parity {
            (2 * frame_num_wrap + 1) as i32
        } else {
            (2 * frame_num_wrap) as i32
        }
    }

    fn field_long_term_pic_num(&self, field_parity: FieldParity, current_bottom: bool) -> i32 {
        let same_parity = (field_parity == FieldParity::Bottom) == current_bottom;
        if same_parity {
            (2 * self.long_term_frame_idx + 1) as i32
        } else {
            (2 * self.long_term_frame_idx) as i32
        }
    }

    pub fn long_term_pic_num(&self, current_is_field: bool, current_bottom: bool) -> i32 {
        if !current_is_field {
            // eq. 8-29
            self.long_term_frame_idx as i32
        } else {
            let same_parity = match self.structure {
                PicStructure::TopField => !current_bottom,
                PicStructure::BottomField => current_bottom,
                PicStructure::Frame | PicStructure::FieldPair => true,
            };
            if same_parity {
                // eq. 8-32
                (2 * self.long_term_frame_idx + 1) as i32
            } else {
                // eq. 8-33
                (2 * self.long_term_frame_idx) as i32
            }
        }
    }

    fn is_short_term(&self) -> bool {
        matches!(self.marking, RefMarking::ShortTerm)
    }

    pub fn is_long_term(&self) -> bool {
        matches!(self.marking, RefMarking::LongTerm)
    }

    fn field_marking(&self, parity: FieldParity) -> RefMarking {
        self.field_markings[usize::from(parity == FieldParity::Bottom)]
    }

    fn set_field_marking(&mut self, parity: FieldParity, m: RefMarking) {
        self.field_markings[usize::from(parity == FieldParity::Bottom)] = m;
        self.marking = match self.structure {
            PicStructure::TopField => self.field_markings[0],
            PicStructure::BottomField => self.field_markings[1],
            PicStructure::Frame | PicStructure::FieldPair => {
                if self.field_markings[0] == self.field_markings[1] {
                    self.field_markings[0]
                } else {
                    RefMarking::Unused
                }
            }
        };
    }

    fn mark_all(&mut self, m: RefMarking) {
        self.marking = m;
        self.sync_field_markings();
    }

    pub fn sync_field_markings(&mut self) {
        self.field_markings = match self.structure {
            PicStructure::TopField => [self.marking, RefMarking::Unused],
            PicStructure::BottomField => [RefMarking::Unused, self.marking],
            PicStructure::Frame | PicStructure::FieldPair => [self.marking, self.marking],
        };
    }

    fn any_field_is(&self, m: RefMarking) -> bool {
        [FieldParity::Top, FieldParity::Bottom]
            .into_iter()
            .any(|p| self.has_field(p) && self.field_marking(p) == m)
    }

    pub fn is_any_field_ref(&self) -> bool {
        self.any_field_is(RefMarking::ShortTerm) || self.any_field_is(RefMarking::LongTerm)
    }

    fn has_field_marked(&self, parity: FieldParity, m: RefMarking) -> bool {
        self.has_field(parity) && self.field_marking(parity) == m
    }

    fn has_field(&self, parity: FieldParity) -> bool {
        match self.structure {
            PicStructure::Frame | PicStructure::FieldPair => true,
            PicStructure::TopField => parity == FieldParity::Top,
            PicStructure::BottomField => parity == FieldParity::Bottom,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmcoOp {
    MarkShortTermUnused(u32),
    MarkLongTermUnused(u32),
    AssignLongTerm(u32, u32),
    SetMaxLongTermIdx(u32),
    MarkAllUnused,
    AssignCurrentLongTerm(u32),
}

pub fn sliding_window_marking(
    dpb: &mut [DpbEntry],
    max_num_ref_frames: u32,
    current_frame_num: u32,
    max_frame_num: u32,
    current_entry: Option<&DpbEntry>,
) {
    // §8.2.5.3 first branch — when the current picture is a coded
    // field that is the SECOND field (in decoding order) of a
    // complementary reference field pair whose first field is marked
    // "used for short-term reference", the current picture joins the
    // pair's marking and NO eviction takes place (the pair occupies
    // one frame slot that the first field already claimed).
    if let Some(cur) = current_entry {
        if cur.structure.is_field() {
            let first_field_is_short_term_ref = dpb.iter().any(|e| {
                e.structure.is_field()
                    && e.frame_num == cur.frame_num
                    && e.structure.is_bottom() != cur.structure.is_bottom()
                    && e.is_short_term()
            });
            if first_field_is_short_term_ref {
                return;
            }
        }
    }

    // §8.2.5.3 step 1 — numShortTerm / numLongTerm count reference
    // FRAMES, complementary reference field PAIRS and non-paired
    // reference fields (a pair of coded fields is ONE unit, not two).
    // Round 436: the per-entry count evicted a stored field pair's
    // fields after only `max_num_ref_frames / 2` frames — invisible to
    // P-field chains (which reference the immediately previous frame)
    // but fatal to B fields, whose two anchors must both survive.
    let units = ref_units(dpb);
    let num_short = units
        .iter()
        .filter(|u| {
            u.iter()
                .any(|&i| dpb[i].any_field_is(RefMarking::ShortTerm))
        })
        .count() as u32;
    let num_long = units
        .iter()
        .filter(|u| u.iter().any(|&i| dpb[i].any_field_is(RefMarking::LongTerm)))
        .count() as u32;
    let cap = max_num_ref_frames.max(1);

    if num_short + num_long < cap {
        return; // No eviction needed.
    }
    if num_short == 0 {
        return; // Nothing short-term to evict (spec's "numShortTerm > 0"
        // precondition — a conformant stream shouldn't hit this
        // branch, but we guard anyway).
    }

    // §8.2.5.3 step 2 — the short-term unit with the smallest
    // FrameNumWrap is marked "unused for reference"; when it is a
    // frame or a complementary field pair, BOTH fields are marked.
    let fnw = |frame_num: u32| -> i64 {
        if frame_num > current_frame_num {
            frame_num as i64 - max_frame_num as i64
        } else {
            frame_num as i64
        }
    };
    let best = units
        .iter()
        .filter(|u| {
            u.iter()
                .any(|&i| dpb[i].any_field_is(RefMarking::ShortTerm))
        })
        .min_by_key(|u| fnw(dpb[u[0]].frame_num));
    if let Some(unit) = best {
        for &i in unit {
            dpb[i].mark_all(RefMarking::Unused);
        }
    }
}

fn ref_units(dpb: &[DpbEntry]) -> Vec<Vec<usize>> {
    let mut units: Vec<Vec<usize>> = Vec::new();
    let mut used = vec![false; dpb.len()];
    for i in 0..dpb.len() {
        if used[i] || !dpb[i].is_any_field_ref() {
            continue;
        }
        used[i] = true;
        let mut unit = vec![i];
        if dpb[i].structure.is_field() {
            // §3 — complementary field pair: the opposite-parity field
            // of the same frame (shared `frame_num`).
            for (j, e) in dpb.iter().enumerate().skip(i + 1) {
                if !used[j]
                    && e.is_any_field_ref()
                    && e.structure.is_field()
                    && e.frame_num == dpb[i].frame_num
                    && e.structure.is_bottom() != dpb[i].structure.is_bottom()
                {
                    used[j] = true;
                    unit.push(j);
                    break;
                }
            }
        }
        units.push(unit);
    }
    units
}

pub fn apply_mmco(
    dpb: &mut [DpbEntry],
    ops: &[MmcoOp],
    current_entry_ref: &mut DpbEntry,
    current_frame_num: u32,
    max_frame_num: u32,
) -> bool {
    let mut mmco5 = false;

    // §7.4.3 — CurrPicNum.
    let current_is_field = current_entry_ref.structure.is_field();
    let current_bottom = current_entry_ref.structure.is_bottom();
    let curr_pic_num: i32 = if current_is_field {
        (2 * current_frame_num + 1) as i32
    } else {
        current_frame_num as i32
    };

    // §8.2.5.4.1/.4.3 field forms — locate the short-term reference
    // FIELD whose eq. 8-30/8-31 PicNum equals `pic_num_x`, returning
    // `(entry index, parity)`.
    let find_st_field = |dpb: &[DpbEntry], pic_num_x: i32| -> Option<(usize, FieldParity)> {
        for (i, e) in dpb.iter().enumerate() {
            for parity in [FieldParity::Top, FieldParity::Bottom] {
                if e.has_field_marked(parity, RefMarking::ShortTerm)
                    && e.field_pic_num(parity, current_frame_num, max_frame_num, current_bottom)
                        == pic_num_x
                {
                    return Some((i, parity));
                }
            }
        }
        None
    };

    // §8.2.5.4.3/.4.6 — shared eviction pre-pass: a LongTermFrameIdx
    // being (re)assigned first unmarks its previous holder(s):
    //   * a long-term FRAME or long-term complementary field PAIR with
    //     that index: the frame/pair AND both fields become "unused
    //     for reference";
    //   * a long-term reference FIELD with that index: becomes "unused
    //     for reference" UNLESS it is part of the complementary field
    //     pair that includes the picture gaining the index (for MMCO 3
    //     the picNumX target — `spare` names its (entry, parity) and
    //     `spare_frame_num` its frame; for MMCO 6 the current picture
    //     — `spare_frame_num` names the current frame).
    let evict_ltfi = |dpb: &mut [DpbEntry],
                      ltfi: u32,
                      spare: Option<(usize, FieldParity)>,
                      spare_frame_num: Option<u32>| {
        for (j, e) in dpb.iter_mut().enumerate() {
            if e.long_term_frame_idx != ltfi {
                continue;
            }
            let top_lt = e.has_field_marked(FieldParity::Top, RefMarking::LongTerm);
            let bot_lt = e.has_field_marked(FieldParity::Bottom, RefMarking::LongTerm);
            if !top_lt && !bot_lt {
                continue;
            }
            if top_lt && bot_lt {
                // Long-term frame / complementary pair holder.
                e.mark_all(RefMarking::Unused);
                continue;
            }
            // Single long-term field holder.
            let lt_parity = if top_lt {
                FieldParity::Top
            } else {
                FieldParity::Bottom
            };
            // Pair exception — the protected field is the other parity
            // of the SAME Frame/FieldPair entry as the target …
            if let Some((si, sp)) = spare {
                if si == j && sp != lt_parity {
                    continue;
                }
            }
            // … or a separate coded-field entry of the same frame
            // (shared `frame_num`) as the picture gaining the index.
            if let Some(sfn) = spare_frame_num {
                if e.structure.is_field() && e.frame_num == sfn {
                    continue;
                }
            }
            e.set_field_marking(lt_parity, RefMarking::Unused);
        }
    };

    for op in ops {
        match *op {
            MmcoOp::MarkShortTermUnused(diff) => {
                // §8.2.5.4.1 eq. 8-39.
                let pic_num_x = curr_pic_num - (diff as i32 + 1);
                if current_is_field {
                    // Field form — only the named FIELD is marked
                    // "unused for reference"; the frame-level marking
                    // drops but the other field's marking is not
                    // changed (`set_field_marking`).
                    if let Some((i, parity)) = find_st_field(dpb, pic_num_x) {
                        dpb[i].set_field_marking(parity, RefMarking::Unused);
                    }
                } else {
                    // Frame form — the frame / complementary pair AND
                    // both fields. A pair stored as two coded-field
                    // entries shares `frame_num`, so both match.
                    for e in dpb.iter_mut() {
                        if e.is_short_term()
                            && e.pic_num(current_frame_num, max_frame_num, false, false)
                                == pic_num_x
                        {
                            e.mark_all(RefMarking::Unused);
                        }
                    }
                }
            }
            MmcoOp::MarkLongTermUnused(ltpn) => {
                // §8.2.5.4.2.
                if current_is_field {
                    // Field form — eq. 8-32/8-33 LongTermPicNum names
                    // one FIELD.
                    let mut found: Option<(usize, FieldParity)> = None;
                    'outer: for (i, e) in dpb.iter().enumerate() {
                        for parity in [FieldParity::Top, FieldParity::Bottom] {
                            if e.has_field_marked(parity, RefMarking::LongTerm)
                                && e.field_long_term_pic_num(parity, current_bottom) == ltpn as i32
                            {
                                found = Some((i, parity));
                                break 'outer;
                            }
                        }
                    }
                    if let Some((i, parity)) = found {
                        dpb[i].set_field_marking(parity, RefMarking::Unused);
                    }
                } else {
                    for e in dpb.iter_mut() {
                        if e.is_long_term() && e.long_term_pic_num(false, false) == ltpn as i32 {
                            e.mark_all(RefMarking::Unused);
                        }
                    }
                }
            }
            MmcoOp::AssignLongTerm(diff, ltfi) => {
                // §8.2.5.4.3.
                let pic_num_x = curr_pic_num - (diff as i32 + 1);
                if current_is_field {
                    let target = find_st_field(dpb, pic_num_x);
                    let spare_frame_num = target.map(|(i, _)| dpb[i].frame_num);
                    evict_ltfi(dpb, ltfi, target, spare_frame_num);
                    if let Some((i, parity)) = target {
                        // Promote the FIELD. When the other field of
                        // the same frame is already long-term with
                        // this index the frame/pair marking follows
                        // (`set_field_marking` derives it; a pair
                        // stored as two coded-field entries is
                        // long-term through both entries).
                        dpb[i].set_field_marking(parity, RefMarking::LongTerm);
                        dpb[i].long_term_frame_idx = ltfi;
                    }
                } else {
                    evict_ltfi(dpb, ltfi, None, None);
                    for e in dpb.iter_mut() {
                        if e.is_short_term()
                            && e.pic_num(current_frame_num, max_frame_num, false, false)
                                == pic_num_x
                        {
                            e.mark_all(RefMarking::LongTerm);
                            e.long_term_frame_idx = ltfi;
                        }
                    }
                }
            }
            MmcoOp::SetMaxLongTermIdx(max_plus1) => {
                // §8.2.5.4.4 — MaxLongTermFrameIdx = max_plus1 - 1 when
                // max_plus1 > 0; "no long-term frame indices" when 0.
                // Every field/frame marked long-term with an index
                // above the new maximum becomes unused.
                let clear_all = max_plus1 == 0;
                let max_ltfi = max_plus1.saturating_sub(1);
                for e in dpb.iter_mut() {
                    if !e.any_field_is(RefMarking::LongTerm) {
                        continue;
                    }
                    if clear_all || e.long_term_frame_idx > max_ltfi {
                        for parity in [FieldParity::Top, FieldParity::Bottom] {
                            if e.has_field_marked(parity, RefMarking::LongTerm) {
                                e.set_field_marking(parity, RefMarking::Unused);
                            }
                        }
                    }
                }
            }
            MmcoOp::MarkAllUnused => {
                // §8.2.5.4.5. Also sets MaxLongTermFrameIdx to "no
                // long-term frame indices" — we express that implicitly
                // by leaving no long-term entries in the DPB; a caller
                // that tracks `MaxLongTermFrameIdx` separately should
                // reset it when `mmco5_triggered == true`.
                for e in dpb.iter_mut() {
                    e.mark_all(RefMarking::Unused);
                }
                mmco5 = true;
            }
            MmcoOp::AssignCurrentLongTerm(ltfi) => {
                // §8.2.5.4.6 — mark the *current* picture as long-term.
                // A previous holder of the index is unmarked first,
                // except a long-term reference FIELD that is part of
                // the complementary field pair including the CURRENT
                // picture (the second-field pair-completion case).
                let spare_frame_num = if current_is_field {
                    Some(current_entry_ref.frame_num)
                } else {
                    None
                };
                evict_ltfi(dpb, ltfi, None, spare_frame_num);
                current_entry_ref.marking = RefMarking::LongTerm;
                current_entry_ref.long_term_frame_idx = ltfi;
                current_entry_ref.sync_field_markings();
                if current_is_field {
                    // Pair completion — when the first field of the
                    // current frame is already long-term, the pair
                    // shares LongTermFrameIdx.
                    for e in dpb.iter_mut() {
                        if e.structure.is_field()
                            && e.frame_num == current_entry_ref.frame_num
                            && e.structure.is_bottom() != current_bottom
                            && e.any_field_is(RefMarking::LongTerm)
                        {
                            e.long_term_frame_idx = ltfi;
                        }
                    }
                }
            }
        }
    }

    mmco5
}

#[allow(clippy::too_many_arguments)]
pub fn perform_marking(
    dpb: &mut [DpbEntry],
    current_entry: &mut DpbEntry,
    max_num_ref_frames: u32,
    is_idr: bool,
    long_term_reference_flag_for_idr: bool,
    _no_output_of_prior_pics_flag: bool,
    adaptive_ops: Option<&[MmcoOp]>,
    current_frame_num: u32,
    max_frame_num: u32,
) -> bool {
    if is_idr {
        // §8.2.5.1 — all reference pictures are marked as "unused for
        // reference". Then, based on long_term_reference_flag, either
        // mark the IDR itself as short-term (and reset MaxLongTermFrameIdx
        // to "no long-term frame indices") or as long-term with
        // LongTermFrameIdx = 0 and MaxLongTermFrameIdx = 0.
        for e in dpb.iter_mut() {
            e.mark_all(RefMarking::Unused);
        }
        if long_term_reference_flag_for_idr {
            current_entry.marking = RefMarking::LongTerm;
            current_entry.long_term_frame_idx = 0;
        } else {
            current_entry.marking = RefMarking::ShortTerm;
        }
        current_entry.sync_field_markings();
        return false;
    }

    match adaptive_ops {
        Some(ops) => {
            let mmco5 = apply_mmco(dpb, ops, current_entry, current_frame_num, max_frame_num);
            // §8.2.5.1 step 3 — if current not marked as long-term by
            // MMCO 6, mark it as short-term.
            if !matches!(current_entry.marking, RefMarking::LongTerm) {
                current_entry.marking = RefMarking::ShortTerm;
            }
            current_entry.sync_field_markings();
            mmco5
        }
        None => {
            sliding_window_marking(
                dpb,
                max_num_ref_frames,
                current_frame_num,
                max_frame_num,
                Some(current_entry),
            );
            // §8.2.5.1 step 3.
            current_entry.marking = RefMarking::ShortTerm;
            current_entry.sync_field_markings();
            false
        }
    }
}
