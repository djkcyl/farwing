//! Stable sample buckets and a continuous, feathered waveform envelope.
use egui::{Color32, Mesh, Pos2, Vec2, pos2, vec2};

pub(super) struct Bucket {
    pub last_sequence: u64,
    pub min: [f32; 2],
    pub max: [f32; 2],
}

/// Bucket boundaries belong to the sample clock, never the scrolling viewport.
/// Only complete groups are drawn: an old peak cannot change when pixels move.
pub(super) fn buckets(
    samples: impl IntoIterator<Item = (u64, [f32; 2], [f32; 2])>,
    stride: u64,
) -> Vec<Bucket> {
    let stride = stride.max(1);
    let mut result = Vec::new();
    let mut pending: Option<(u64, Bucket)> = None;
    for (sequence, min, max) in samples {
        if sequence.is_multiple_of(stride) {
            pending = Some((
                1,
                Bucket {
                    last_sequence: sequence,
                    min,
                    max,
                },
            ));
        } else if let Some((count, bucket)) = &mut pending {
            if sequence != bucket.last_sequence + 1 {
                pending = None;
                continue;
            }
            *count += 1;
            bucket.last_sequence = sequence;
            for channel in 0..2 {
                bucket.min[channel] = bucket.min[channel].min(min[channel]);
                bucket.max[channel] = bucket.max[channel].max(max[channel]);
            }
        }
        if pending.as_ref().is_some_and(|(count, _)| *count == stride) {
            result.push(pending.take().unwrap().1);
        }
    }
    result
}

fn normal(points: &[Pos2], index: usize, above: bool) -> Vec2 {
    let before = points[index.saturating_sub(1)];
    let after = points[(index + 1).min(points.len() - 1)];
    let tangent = after - before;
    let normal = vec2(tangent.y, -tangent.x).normalized();
    if above { normal } else { -normal }
}

/// One connected strip, with a one-device-pixel transparent fringe around it.
/// Subpixel translation is retained; there are no individually rounded bars.
pub(super) fn envelope(top: &[Pos2], bottom: &[Pos2], color: Color32, feather: f32) -> Mesh {
    let mut mesh = Mesh::default();
    if top.len() < 2 || top.len() != bottom.len() {
        return mesh;
    }
    mesh.vertices.reserve(top.len() * 4 + 4);
    mesh.indices.reserve((top.len() - 1) * 18 + 12);
    for index in 0..top.len() {
        mesh.colored_vertex(
            top[index] + normal(top, index, true) * feather,
            Color32::TRANSPARENT,
        );
        mesh.colored_vertex(top[index], color);
        mesh.colored_vertex(bottom[index], color);
        mesh.colored_vertex(
            bottom[index] + normal(bottom, index, false) * feather,
            Color32::TRANSPARENT,
        );
        if index > 0 {
            let previous = (index as u32 - 1) * 4;
            let current = index as u32 * 4;
            for row in 0..3 {
                mesh.add_triangle(previous + row, current + row, current + row + 1);
                mesh.add_triangle(previous + row, current + row + 1, previous + row + 1);
            }
        }
    }
    for (index, dx) in [(0, -feather), (top.len() - 1, feather)] {
        let edge = index as u32 * 4;
        let outer = mesh.vertices.len() as u32;
        mesh.colored_vertex(pos2(top[index].x + dx, top[index].y), Color32::TRANSPARENT);
        mesh.colored_vertex(
            pos2(bottom[index].x + dx, bottom[index].y),
            Color32::TRANSPARENT,
        );
        if dx < 0.0 {
            mesh.add_triangle(outer, edge + 1, edge + 2);
            mesh.add_triangle(outer, edge + 2, outer + 1);
        } else {
            mesh.add_triangle(edge + 1, outer, outer + 1);
            mesh.add_triangle(edge + 1, outer + 1, edge + 2);
        }
    }
    mesh
}
