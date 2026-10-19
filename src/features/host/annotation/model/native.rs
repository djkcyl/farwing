use super::*;
use crate::protocol::annotation::{Click, Command, Laser, Operation};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(super) enum Animation {
    Laser { value: Laser, received: Instant },
    Click { value: Click, started: Instant },
}
fn point(p: &PbDrawPoint) -> bool {
    p.x.is_finite() && p.y.is_finite() && (0.0..=1.0).contains(&p.x) && (0.0..=1.0).contains(&p.y)
}
fn width(w: f32) -> bool {
    w.is_finite() && w > 0.0 && w <= 64.0
}
pub(in crate::features::host::annotation) fn valid_native(command: &Command) -> bool {
    match &command.operation {
        Some(Operation::Draw(draw)) => draw.payload.as_ref().is_some_and(valid),
        Some(Operation::Replace(object)) => object.stroke.as_ref().is_some_and(|s| {
            s.screen_id.is_some()
                && s.stroke_id != 0
                && !s.points.is_empty()
                && s.points.len() <= 32768
                && s.line_width.is_none_or(width)
                && s.points.iter().all(point)
        }),
        Some(Operation::Batch(batch)) => {
            !batch.commands.is_empty()
                && batch.commands.len() <= 512
                && batch.commands.iter().all(|c| match &c.operation {
                    Some(Operation::Draw(draw))
                        if !matches!(draw.payload, Some(PbDrawRequestKind::Toggle(_))) =>
                    {
                        valid_native(c)
                    }
                    Some(Operation::Replace(object)) if !object.transient => valid_native(c),
                    _ => false,
                })
        }
        Some(Operation::Board(board)) => board.color.is_none_or(|c| c >> 24 == 255),
        Some(Operation::Clear(_)) => true,
        Some(Operation::Laser(laser)) => {
            laser.id != 0
                && width(laser.width)
                && (40..=300).contains(&laser.tail_ms)
                && !laser.samples.is_empty()
                && laser.samples.len() <= 32
                && laser
                    .samples
                    .iter()
                    .all(|s| s.point.as_ref().is_some_and(point) && s.age_ms <= laser.tail_ms + 220)
                && laser.samples.windows(2).all(|w| w[0].age_ms >= w[1].age_ms)
        }
        Some(Operation::Click(click)) => {
            click.id != 0
                && width(click.width)
                && click.center.as_ref().is_some_and(point)
                && click
                    .radii
                    .as_ref()
                    .is_some_and(|p| point(p) && p.x > 0.0 && p.y > 0.0)
        }
        None => false,
    }
}
impl Model {
    pub fn suspend(&mut self) {
        if self.transient.is_empty() {
            return;
        }
        for key in &self.transient {
            self.strokes.remove(key);
            self.dirty.insert(key.0);
        }
        self.transient.clear();
        self.animations.clear();
        self.changed();
    }
    pub fn apply_action(
        &mut self,
        context: &Context,
        action: &super::super::Action,
        desktop: i32,
    ) -> i32 {
        match action {
            super::super::Action::Native(c) => self.apply_native(context, c, desktop),
        }
    }
    fn apply_native(&mut self, context: &Context, command: &Command, desktop: i32) -> i32 {
        if !valid_native(command) {
            return 4;
        }
        if let Some(Operation::Draw(draw)) = &command.operation {
            return self.apply(context, draw.payload.as_ref().unwrap(), desktop);
        }
        if !context.allowed {
            self.clear();
            return 4;
        }
        if desktop != 0 {
            return desktop;
        }
        if !self.enabled {
            return 4;
        }
        match command.operation.as_ref().unwrap() {
            Operation::Batch(batch) => {
                // Snapshot metadata and share immutable stroke buffers. Appending uses
                // copy-on-write; failed transactions never mutate the live scene.
                let mut next = self.clone();
                for c in &batch.commands {
                    let code = next.apply_native(context, c, desktop);
                    if code != 0 {
                        return code;
                    }
                }
                *self = next;
            }
            Operation::Replace(object) => {
                let stroke = object.stroke.as_ref().unwrap();
                let key = (stroke.screen_id.unwrap(), stroke.stroke_id);
                if Self::screen(context, Some(key.0)).is_none() {
                    return 4;
                }
                let old = self.strokes.get(&key).map_or(0, |s| s.points.len());
                let total: usize = self.strokes.values().map(|s| s.points.len()).sum();
                if total - old + stroke.points.len() > MAX_POINTS
                    || (!self.strokes.contains_key(&key) && self.strokes.len() >= MAX_STROKES)
                {
                    return 4;
                }
                self.strokes.insert(key, Arc::new(stroke.clone()));
                self.animations.remove(&key);
                if object.transient {
                    self.transient.insert(key);
                } else {
                    self.transient.remove(&key);
                }
                self.dirty.insert(key.0);
            }
            Operation::Board(board) => {
                if Self::screen(context, Some(board.screen)).is_none() {
                    return 4;
                }
                if let Some(color) = board.color {
                    self.boards.insert(board.screen, color);
                } else {
                    self.boards.remove(&board.screen);
                }
                self.dirty.insert(board.screen);
            }
            Operation::Clear(all) => {
                self.dirty.extend(self.strokes.keys().map(|(id, _)| *id));
                if *all {
                    self.dirty.extend(self.boards.keys().copied());
                    self.strokes.clear();
                    self.boards.clear();
                    self.transient.clear();
                    self.animations.clear();
                } else {
                    self.strokes.retain(|key, _| self.transient.contains(key));
                }
            }
            Operation::Laser(value) => {
                let key = (value.screen, value.id);
                if !self.can_animate(context, key) {
                    return 4;
                }
                self.transient.insert(key);
                self.animations.insert(
                    key,
                    Animation::Laser {
                        value: value.clone(),
                        received: Instant::now(),
                    },
                );
                self.tick(Instant::now());
            }
            Operation::Click(value) => {
                let key = (value.screen, value.id);
                if !self.can_animate(context, key) {
                    return 4;
                }
                self.transient.insert(key);
                self.animations.insert(
                    key,
                    Animation::Click {
                        value: value.clone(),
                        started: Instant::now(),
                    },
                );
                self.tick(Instant::now());
            }
            Operation::Draw(_) => unreachable!(),
        }
        self.changed();
        0
    }
    fn can_animate(&self, context: &Context, key: (i32, u32)) -> bool {
        Self::screen(context, Some(key.0)).is_some()
            && (self.animations.contains_key(&key) || self.animations.len() < 8)
            && (!self.strokes.contains_key(&key) || self.transient.contains(&key))
            && (self.strokes.contains_key(&key) || self.strokes.len() < MAX_STROKES)
            && self.strokes.values().map(|s| s.points.len()).sum::<usize>() + 64 <= MAX_POINTS
    }
    pub fn tick(&mut self, now: Instant) {
        let mut expired = Vec::new();
        for (&key, animation) in &self.animations {
            let (points, color, line) = match animation {
                Animation::Laser { value, received } => {
                    let elapsed = now
                        .saturating_duration_since(*received)
                        .as_millis()
                        .min(u32::MAX as u128) as u32;
                    let age = elapsed.saturating_add(value.samples.last().unwrap().age_ms);
                    let life = value.tail_ms + 220;
                    if age >= life {
                        expired.push(key);
                        continue;
                    }
                    let mut points: Vec<_> = value
                        .samples
                        .iter()
                        .filter(|s| s.age_ms.saturating_add(elapsed) <= value.tail_ms)
                        .map(|s| s.point.clone().unwrap())
                        .collect();
                    if points.is_empty() {
                        points.push(value.samples.last().unwrap().point.clone().unwrap());
                    }
                    (
                        points,
                        fade(value.color, 1.0 - age as f32 / life as f32),
                        value.width,
                    )
                }
                Animation::Click { value, started } => {
                    let elapsed = now.saturating_duration_since(*started);
                    if elapsed >= Duration::from_millis(450) {
                        expired.push(key);
                        continue;
                    }
                    let t = elapsed.as_secs_f32() / 0.45;
                    let radius = 0.35 + 0.65 * t;
                    let center = value.center.as_ref().unwrap();
                    let radii = value.radii.as_ref().unwrap();
                    let points = (0..=40)
                        .map(|i| {
                            let a = i as f32 / 40.0 * std::f32::consts::TAU;
                            PbDrawPoint {
                                x: (center.x + a.cos() * radii.x * radius).clamp(0.0, 1.0),
                                y: (center.y + a.sin() * radii.y * radius).clamp(0.0, 1.0),
                            }
                        })
                        .collect();
                    (points, fade(value.color, 1.0 - t), value.width)
                }
            };
            let stroke = PbDrawStroke {
                stroke_id: key.1,
                screen_id: Some(key.0),
                points,
                line_width: Some(line),
                color: Some(color),
            };
            if self.strokes.get(&key).map(AsRef::as_ref) != Some(&stroke) {
                self.strokes.insert(key, Arc::new(stroke));
                self.dirty.insert(key.0);
            }
        }
        for key in expired {
            self.animations.remove(&key);
            self.transient.remove(&key);
            self.strokes.remove(&key);
            self.dirty.insert(key.0);
        }
        if !self.dirty.is_empty() {
            self.changed();
        }
    }
}
fn fade(color: u32, amount: f32) -> u32 {
    (color & 0xffffff) | ((((color >> 24) as f32 * amount.clamp(0.0, 1.0)).round() as u32) << 24)
}
