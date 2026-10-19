//! Smooth, cached display curves. Waypoints never participate in execution.
use egui::{Color32, Painter, Pos2, Rect, Stroke, Vec2};

#[derive(Clone, PartialEq)]
pub(super) struct Connection {
    pub id: String,
    pub from: Pos2,
    pub to: Pos2,
    pub via: Vec<[f32; 2]>,
}
struct Segment {
    curve: [Pos2; 4],
    samples: Vec<Pos2>,
}
#[derive(Clone, Copy)]
pub(super) struct Hit {
    pub wire: usize,
    pub segment: usize,
    pub point: Pos2,
}
#[derive(Default)]
pub(super) struct Cache {
    connections: Vec<Connection>,
    paths: Vec<Vec<Segment>>,
}
impl Cache {
    pub fn update(&mut self, connections: &[Connection]) {
        if self.connections == connections {
            return;
        }
        self.paths = connections
            .iter()
            .map(|wire| {
                let mut points = vec![wire.from];
                points.extend(
                    wire.via
                        .iter()
                        .take(32)
                        .filter(|p| p.iter().all(|x| x.is_finite()))
                        .map(|p| Pos2::new(p[0], p[1])),
                );
                points.push(wire.to);
                let tangents: Vec<_> = (0..points.len())
                    .map(|i| {
                        if i == 0 || i + 1 == points.len() {
                            Vec2::RIGHT
                        } else {
                            let v = (points[i] - points[i - 1]).normalized()
                                + (points[i + 1] - points[i]).normalized();
                            if v.length_sq() > 0.001 {
                                v.normalized()
                            } else {
                                Vec2::RIGHT
                            }
                        }
                    })
                    .collect();
                points
                    .windows(2)
                    .enumerate()
                    .map(|(i, pair)| {
                        let length = if points.len() == 2 {
                            ((pair[1].x - pair[0].x).abs() * 0.5).min(180.)
                        } else {
                            (pair[0].distance(pair[1]) * 0.4).min(180.)
                        };
                        let curve = [
                            pair[0],
                            pair[0] + tangents[i] * length,
                            pair[1] - tangents[i + 1] * length,
                            pair[1],
                        ];
                        let samples = (0..=40).map(|i| sample(curve, i as f32 / 40.)).collect();
                        Segment { curve, samples }
                    })
                    .collect()
            })
            .collect();
        self.connections = connections.to_vec();
    }
    pub fn hit(&self, point: Pos2, radius: f32) -> Option<Hit> {
        let mut best = radius * radius;
        let mut result = None;
        for (wire, path) in self.paths.iter().enumerate() {
            for (segment, part) in path.iter().enumerate() {
                if !Rect::from_points(&part.curve)
                    .expand(radius)
                    .contains(point)
                {
                    continue;
                }
                for pair in part.samples.windows(2) {
                    let d = pair[1] - pair[0];
                    let t = ((point - pair[0]).dot(d) / d.length_sq().max(0.0001)).clamp(0., 1.);
                    let projected = pair[0] + t * d;
                    let distance = point.distance_sq(projected);
                    if distance < best {
                        best = distance;
                        result = Some(Hit {
                            wire,
                            segment,
                            point: projected,
                        });
                    }
                }
            }
        }
        result
    }
    pub fn paint(&self, painter: &Painter, index: usize, color: Color32, highlighted: bool) {
        for part in &self.paths[index] {
            painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                part.curve,
                false,
                Color32::TRANSPARENT,
                Stroke::new(if highlighted { 3. } else { 2. }, color),
            ));
        }
    }
}
fn sample(p: [Pos2; 4], t: f32) -> Pos2 {
    let a = p[0].lerp(p[1], t);
    let b = p[1].lerp(p[2], t);
    let c = p[2].lerp(p[3], t);
    a.lerp(b, t).lerp(b.lerp(c, t), t)
}
