//! Bounded normalized strokes, independent of HWNDs and connection transport.
use super::{Context, Outcome, Refusal};
use crate::protocol::annotation::{MAX_DRAW_POINTS, MAX_STROKE_POINTS, Op, Stroke};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub(crate) const MAX_POINTS: usize = 1_048_576;
pub(crate) const MAX_STROKES: usize = 1024;
/// Most strokes one `Restore` puts.
const MAX_RESTORE: usize = 512;

#[derive(Clone, Default)]
pub(crate) struct Model {
    pub enabled: bool,
    pub strokes: BTreeMap<(i32, u32), Arc<Stroke>>,
    pub revision: u64,
    pub dirty: BTreeSet<i32>,
    pub boards: BTreeMap<i32, u32>,
}

/// Whether `op` is a well-formed request (screens and limits are checked
/// when it is applied).
pub(super) fn valid(op: &Op) -> bool {
    match op {
        Op::Show(_) | Op::Clear(_) => true,
        Op::Draw(stroke) => stroke.valid(MAX_DRAW_POINTS),
        Op::Put(stroke) => stroke.valid(MAX_STROKE_POINTS),
        Op::Erase(stroke) => stroke.id != 0,
        Op::Board(board) => board.color.is_none_or(|c| c >> 24 == 255),
        Op::Restore(restore) => {
            !restore.strokes.is_empty()
                && restore.strokes.len() <= MAX_RESTORE
                && restore.strokes.iter().all(|s| s.valid(MAX_STROKE_POINTS))
        }
        Op::Reset(_) => false,
    }
}

impl Model {
    pub fn clear(&mut self) {
        self.dirty.extend(self.strokes.keys().map(|(id, _)| *id));
        self.enabled = false;
        self.strokes.clear();
        self.dirty.extend(self.boards.keys().copied());
        self.boards.clear();
        self.changed();
    }
    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
    fn points(&self) -> usize {
        self.strokes.values().map(|s| s.points.len()).sum()
    }
    pub fn reconcile(&mut self, old: &Context, new: &Context) {
        if old.screens == new.screens {
            return;
        }
        let same = |id: i32| {
            matches!(
                (
                    old.screens.iter().find(|s| s.id == id),
                    new.screens.iter().find(|s| s.id == id),
                ),
                (Some(a), Some(b))
                    if a.device_name == b.device_name && a.identity == b.identity && a.adapter == b.adapter
            )
        };
        self.strokes.retain(|(id, _), _| same(*id));
        self.boards.retain(|id, _| same(*id));
        self.dirty
            .extend(old.screens.iter().chain(&new.screens).map(|s| s.id));
        self.changed();
    }
    pub fn screen(context: &Context, explicit: Option<i32>) -> Option<i32> {
        if let Some(id) = explicit {
            return context.screens.iter().any(|s| s.id == id).then_some(id);
        }
        context
            .screens
            .iter()
            .find(|s| s.id == context.current)
            .or_else(|| context.screens.iter().find(|s| s.primary))
            .map(|s| s.id)
    }
    /// Apply `op`; `desktop`: whether the user's desktop can show the overlay.
    pub fn apply(&mut self, context: &Context, op: &Op, desktop: Outcome) -> Outcome {
        if matches!(op, Op::Show(false)) {
            self.clear();
            return Ok(());
        }
        if !context.allowed {
            self.clear();
            return Err(Refusal::NotAllowed);
        }
        if !valid(op) {
            return Err(Refusal::Invalid);
        }
        desktop?;
        if matches!(op, Op::Show(true)) {
            let screen = Self::screen(context, None).ok_or(Refusal::Invalid)?;
            self.enabled = true;
            self.dirty.insert(screen);
            self.changed();
            return Ok(());
        }
        if !self.enabled {
            return Err(Refusal::Off);
        }
        match op {
            Op::Draw(stroke) => {
                let screen = Self::screen(context, Some(stroke.screen)).ok_or(Refusal::Invalid)?;
                let key = (screen, stroke.id);
                let old = self.strokes.get(&key).map_or(0, |s| s.points.len());
                if old + stroke.points.len() > MAX_STROKE_POINTS
                    || self.points() + stroke.points.len() > MAX_POINTS
                    || (!self.strokes.contains_key(&key) && self.strokes.len() >= MAX_STROKES)
                {
                    return Err(Refusal::Invalid);
                }
                let entry = Arc::make_mut(self.strokes.entry(key).or_default());
                entry.id = stroke.id;
                entry.screen = screen;
                entry.width = stroke.width;
                entry.color = stroke.color;
                entry.points.extend_from_slice(&stroke.points);
                self.dirty.insert(screen);
            }
            Op::Put(stroke) => self.put(context, stroke)?,
            Op::Restore(restore) => {
                // Stroke buffers are shared; a failed restore never touches
                // the live scene.
                let mut next = self.clone();
                for stroke in &restore.strokes {
                    next.put(context, stroke)?;
                }
                *self = next;
            }
            Op::Erase(stroke) => {
                if self.strokes.remove(&(stroke.screen, stroke.id)).is_some() {
                    self.dirty.insert(stroke.screen);
                }
            }
            Op::Clear(boards) => {
                self.dirty.extend(self.strokes.keys().map(|(id, _)| *id));
                self.strokes.clear();
                if *boards {
                    self.dirty.extend(self.boards.keys().copied());
                    self.boards.clear();
                }
            }
            Op::Board(board) => {
                Self::screen(context, Some(board.screen)).ok_or(Refusal::Invalid)?;
                if let Some(color) = board.color {
                    self.boards.insert(board.screen, color);
                } else {
                    self.boards.remove(&board.screen);
                }
                self.dirty.insert(board.screen);
            }
            Op::Show(_) | Op::Reset(_) => unreachable!("handled above"),
        }
        self.changed();
        Ok(())
    }
    fn put(&mut self, context: &Context, stroke: &Stroke) -> Outcome {
        let key = (stroke.screen, stroke.id);
        Self::screen(context, Some(key.0)).ok_or(Refusal::Invalid)?;
        let old = self.strokes.get(&key).map_or(0, |s| s.points.len());
        if self.points() - old + stroke.points.len() > MAX_POINTS
            || (!self.strokes.contains_key(&key) && self.strokes.len() >= MAX_STROKES)
        {
            return Err(Refusal::Invalid);
        }
        self.strokes.insert(key, Arc::new(stroke.clone()));
        self.dirty.insert(key.0);
        Ok(())
    }
}
