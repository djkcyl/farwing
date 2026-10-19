//! Bounded normalized strokes, independent of HWNDs and connection transport.
use super::Context;
use crate::features::stream_control::annotation::wire::*;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

mod native;
pub(super) use native::valid_native;

pub(crate) const MAX_POINTS: usize = 1_048_576;
pub(crate) const MAX_STROKES: usize = 1024;
#[derive(Clone, Default)]
pub(crate) struct Model {
    pub enabled: bool,
    pub strokes: BTreeMap<(i32, u32), Arc<PbDrawStroke>>,
    pub revision: u64,
    pub dirty: BTreeSet<i32>,
    pub boards: BTreeMap<i32, u32>,
    transient: BTreeSet<(i32, u32)>,
    animations: BTreeMap<(i32, u32), native::Animation>,
}
pub(super) fn valid(command: &PbDrawRequestKind) -> bool {
    match command {
        PbDrawRequestKind::Stroke(s) => {
            s.stroke_id != 0
                && !s.points.is_empty()
                && s.points.len() <= 4096
                && s.line_width
                    .is_none_or(|w| w.is_finite() && w > 0.0 && w <= 64.0)
                && s.points.iter().all(|p| {
                    p.x.is_finite()
                        && p.y.is_finite()
                        && (0.0..=1.0).contains(&p.x)
                        && (0.0..=1.0).contains(&p.y)
                })
        }
        PbDrawRequestKind::Clear(c) => c.clear_type == 1 || (c.clear_type == 2 && c.stroke_id != 0),
        PbDrawRequestKind::Toggle(_) => true,
    }
}
impl Model {
    pub fn clear(&mut self) {
        self.dirty.extend(self.strokes.keys().map(|(id, _)| *id));
        self.enabled = false;
        self.strokes.clear();
        self.dirty.extend(self.boards.keys().copied());
        self.boards.clear();
        self.transient.clear();
        self.animations.clear();
        self.changed();
    }
    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
    pub fn animated(&self) -> bool {
        !self.animations.is_empty()
    }
    pub fn reconcile(&mut self, old: &Context, new: &Context) {
        if old.screens == new.screens {
            return;
        }
        let before = self.strokes.len();
        self.strokes.retain(|(id, _), _| {
            let a = old.screens.iter().find(|s| s.id == *id);
            let b = new.screens.iter().find(|s| s.id == *id);
            matches!((a,b), (Some(a),Some(b)) if a.device_name==b.device_name && a.identity==b.identity && a.adapter==b.adapter)
        });
        self.boards.retain(|id,_| {
            matches!((old.screens.iter().find(|s|s.id==*id),new.screens.iter().find(|s|s.id==*id)),(Some(a),Some(b)) if a.device_name==b.device_name && a.identity==b.identity && a.adapter==b.adapter)
        });
        self.transient.retain(|key| self.strokes.contains_key(key));
        self.animations
            .retain(|key, _| self.transient.contains(key));
        if before != self.strokes.len() || old.screens != new.screens {
            self.dirty
                .extend(old.screens.iter().chain(&new.screens).map(|s| s.id));
            self.changed();
        }
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
    pub fn apply(&mut self, context: &Context, command: &PbDrawRequestKind, desktop: i32) -> i32 {
        if matches!(
            command,
            PbDrawRequestKind::Toggle(PbDrawToggle { enable: false })
        ) {
            self.clear();
            return 0;
        }
        if !context.allowed {
            self.clear();
            return 4;
        }
        if !valid(command) {
            return 4;
        }
        if desktop != 0 {
            return desktop;
        }
        match command {
            PbDrawRequestKind::Toggle(_) => {
                if Self::screen(context, None).is_none() {
                    return 4;
                }
                self.enabled = true;
                self.dirty.extend(Self::screen(context, None));
            }
            PbDrawRequestKind::Stroke(stroke) => {
                let Some(screen) = Self::screen(context, stroke.screen_id) else {
                    return 4;
                };
                if !self.enabled {
                    return 4;
                }
                let key = (screen, stroke.stroke_id);
                let old = self.strokes.get(&key).map_or(0, |s| s.points.len());
                let total: usize = self.strokes.values().map(|s| s.points.len()).sum();
                if old + stroke.points.len() > 32768
                    || total + stroke.points.len() > MAX_POINTS
                    || (!self.strokes.contains_key(&key) && self.strokes.len() >= MAX_STROKES)
                {
                    return 4;
                }
                let entry = Arc::make_mut(self.strokes.entry(key).or_default());
                entry.stroke_id = stroke.stroke_id;
                entry.screen_id = Some(screen);
                entry.line_width = Some(stroke.line_width.unwrap_or(3.0));
                entry.color = Some(stroke.color.unwrap_or(0xffff4444));
                entry.points.extend_from_slice(&stroke.points);
                self.dirty.insert(screen);
            }
            PbDrawRequestKind::Clear(clear) => match clear.clear_type {
                1 => {
                    self.dirty.extend(self.strokes.keys().map(|(id, _)| *id));
                    self.strokes.clear();
                    self.dirty.extend(self.boards.keys().copied());
                    self.boards.clear();
                    self.transient.clear();
                    self.animations.clear();
                } // ALL is session-wide, even with an optional screen.
                2 if clear.stroke_id != 0 => {
                    self.dirty.extend(
                        self.strokes
                            .keys()
                            .filter(|(screen, id)| {
                                *id == clear.stroke_id
                                    && clear.screen_id.is_none_or(|s| s == *screen)
                            })
                            .map(|(screen, _)| *screen),
                    );
                    self.strokes.retain(|(screen, id), _| {
                        *id != clear.stroke_id || clear.screen_id.is_some_and(|s| s != *screen)
                    });
                    self.transient.retain(|key| self.strokes.contains_key(key));
                    self.animations
                        .retain(|key, _| self.transient.contains(key));
                }
                _ => return 4,
            },
        }
        self.changed();
        0
    }
}
