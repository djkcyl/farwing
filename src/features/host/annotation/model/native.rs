use super::*;
use crate::protocol::annotation::{Command, Operation};

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
                    Some(Operation::Replace(_)) => valid_native(c),
                    _ => false,
                })
        }
        Some(Operation::Board(board)) => board.color.is_none_or(|c| c >> 24 == 255),
        Some(Operation::Clear(_)) => true,
        None => false,
    }
}
impl Model {
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
                self.strokes.clear();
                if *all {
                    self.dirty.extend(self.boards.keys().copied());
                    self.boards.clear();
                }
            }
            Operation::Draw(_) => unreachable!(),
        }
        self.changed();
        0
    }
}
