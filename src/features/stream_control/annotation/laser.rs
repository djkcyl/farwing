//! A single transient single-colour stroke.
use super::*;

const MAX_SAMPLES: usize = 32;

pub(super) struct LaserTrail {
    samples: VecDeque<(Instant, Point)>,
    last_move: Instant,
    style: Style,
    options: LaserOptions,
    revision: u64,
}

impl LaserTrail {
    pub(super) fn new(point: Point, style: Style, options: LaserOptions, now: Instant) -> Self {
        Self {
            samples: VecDeque::from([(now, point)]),
            last_move: now,
            style,
            options,
            revision: 1,
        }
    }

    pub(super) fn move_to(
        &mut self,
        point: Point,
        style: Style,
        options: LaserOptions,
        now: Instant,
    ) {
        if self.style != style || self.options != options {
            self.revision = self.revision.wrapping_add(1);
        }
        self.style = style;
        self.options = options;
        if self.samples.back().is_some_and(|(_, last)| *last == point) {
            return;
        }
        self.last_move = now;
        self.revision = self.revision.wrapping_add(1);
        self.samples.push_back((now, point));
        while self.samples.len() > MAX_SAMPLES {
            self.samples.pop_front();
        }
    }
    pub(super) fn native(&self, id: u32, screen: i32) -> crate::protocol::annotation::Laser {
        let now = Instant::now();
        let life = u32::from(self.options.tail_ms) + 220;
        crate::protocol::annotation::Laser {
            id,
            screen,
            width: self.style.width,
            color: self.style.argb,
            tail_ms: u32::from(self.options.tail_ms),
            samples: self
                .samples
                .iter()
                .map(|(at, p)| crate::protocol::annotation::LaserSample {
                    point: Some(PbDrawPoint { x: p.x, y: p.y }),
                    age_ms: now
                        .saturating_duration_since(*at)
                        .as_millis()
                        .min(u128::from(life)) as u32,
                })
                .collect(),
        }
    }
}

pub(super) fn update(a: &mut Annotation, now: Instant, native: bool) {
    let Some(shape) = &mut a.live_shape else {
        return;
    };
    if shape.finish.is_some() {
        return;
    }
    let Some(laser) = &mut shape.laser else {
        return;
    };
    let age = now.saturating_duration_since(laser.last_move);
    let tail = Duration::from_millis(u64::from(laser.options.tail_ms));
    let lifetime = tail + LASER_FADE;
    if age >= lifetime {
        shape.finish = Some(false);
        return;
    }
    while laser.samples.len() > 1 && now.saturating_duration_since(laser.samples[0].0) > tail {
        laser.samples.pop_front();
    }
    if native {
        shape.revision = laser.revision;
        return;
    }
    let points = laser.samples.iter().map(|(_, p)| *p).collect::<Vec<_>>();
    let fade = ((1. - age.as_secs_f32() / lifetime.as_secs_f32()).clamp(0., 1.) * 16.).ceil() / 16.;
    let alpha = (((laser.style.argb >> 24) as f32 * fade).round() as u32).max(1);
    let style = Style {
        argb: (laser.style.argb & 0xffffff) | (alpha << 24),
        ..laser.style
    };
    if shape.stroke.points != points || shape.stroke.style != style {
        shape.stroke.points = points;
        shape.stroke.style = style;
        shape.revision = shape.revision.wrapping_add(1);
    }
}
