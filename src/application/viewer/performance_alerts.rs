//! Paint-only status icons: no egui area, response, tooltip or mouse hit region.
use crate::diagnostics::performance::health::{Assessment, Metric};
use egui::{Color32, Painter, Pos2, Rect, Stroke, pos2, vec2};
use std::time::{Duration, Instant};

const SIZE: f32 = 32.0;
const GAP: f32 = 4.0;
const MARGIN: f32 = 18.0;
const ORDER: [Metric; 6] = [
    Metric::Rtt,
    Metric::Loss,
    Metric::Jitter,
    Metric::FrameDelay,
    Metric::LocalDelay,
    Metric::PresentationStall,
];

pub(super) fn show(ctx: &egui::Context, health: &Assessment, bounds: Rect) {
    let now = Instant::now();
    if health.alerts.is_empty()
        || bounds.width() < SIZE + MARGIN * 2.0
        || bounds.height() < SIZE + MARGIN * 2.0
    {
        return;
    }
    let columns =
        (((bounds.width() - MARGIN * 2.0 + GAP) / (SIZE + GAP)) as usize).clamp(1, ORDER.len());
    let count = health.alerts.iter().filter(|a| a.expires_at > now).count();
    if count == 0 {
        return;
    }
    // Background paint remains below menus and never participates in input routing.
    let painter = ctx
        .layer_painter(egui::LayerId::background())
        .with_clip_rect(bounds);
    let mut index = 0;
    for metric in ORDER {
        let Some(alert) = health
            .alerts
            .iter()
            .find(|a| a.metric == metric && a.expires_at > now)
        else {
            continue;
        };
        let row = index / columns;
        let col = index % columns;
        let row_count = (count - row * columns).min(columns);
        let x = bounds.right() - MARGIN - row_count as f32 * (SIZE + GAP)
            + GAP
            + col as f32 * (SIZE + GAP);
        let y = bounds.top() + MARGIN + row as f32 * (SIZE + GAP);
        let remaining = alert.expires_at.saturating_duration_since(now);
        let opacity = if alert.historical {
            (remaining.as_secs_f32() / 3.0).clamp(0.0, 1.0) * 0.5
        } else {
            1.0
        };
        let color = if alert.historical {
            crate::ui::theme::MUTED
        } else {
            super::hud::health_color(Some(alert.level))
        };
        let origin = pos2(x, y);
        paint_icon(
            &painter,
            origin + vec2(0.0, 1.0),
            metric,
            Color32::from_black_alpha((150.0 * opacity) as u8),
            2.8,
        );
        paint_icon(&painter, origin, metric, color.gamma_multiply(opacity), 1.8);
        if alert.historical {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
        ctx.request_repaint_after(remaining);
        index += 1;
    }
}

fn paint_icon(p: &Painter, origin: Pos2, metric: Metric, color: Color32, width: f32) {
    let scale = SIZE / 24.0;
    let stroke = Stroke::new(width * scale, color);
    let point = |x, y| origin + vec2(x, y) * scale;
    let line = |points: &[(f32, f32)]| {
        p.line(points.iter().map(|&(x, y)| point(x, y)).collect(), stroke);
    };
    match metric {
        Metric::Rtt => {
            p.circle_stroke(point(12.0, 14.0), 7.5 * scale, stroke);
            line(&[(9.0, 2.0), (15.0, 2.0)]);
            line(&[(12.0, 2.0), (12.0, 5.0)]);
            line(&[(18.0, 7.0), (20.0, 5.0)]);
            line(&[(12.0, 10.0), (12.0, 14.0)]);
        }
        Metric::Loss => {
            line(&[
                (3.0, 21.0),
                (6.0, 18.0),
                (4.0, 16.0),
                (9.0, 11.0),
                (13.0, 15.0),
                (8.0, 20.0),
                (6.0, 18.0),
            ]);
            line(&[
                (21.0, 3.0),
                (18.0, 6.0),
                (20.0, 8.0),
                (15.0, 13.0),
                (11.0, 9.0),
                (16.0, 4.0),
                (18.0, 6.0),
            ]);
            line(&[(3.0, 3.0), (6.0, 6.0)]);
            line(&[(18.0, 18.0), (21.0, 21.0)]);
        }
        Metric::Jitter => line(&[
            (2.0, 12.0),
            (6.0, 12.0),
            (9.0, 3.0),
            (14.0, 21.0),
            (17.0, 12.0),
            (22.0, 12.0),
        ]),
        Metric::FrameDelay => {
            let points: Vec<_> = (0..=24)
                .map(|i| {
                    let a = std::f32::consts::PI + i as f32 / 24.0 * std::f32::consts::PI;
                    point(12.0 + a.cos() * 9.0, 15.0 + a.sin() * 9.0)
                })
                .collect();
            p.line(points, stroke);
            line(&[(3.0, 15.0), (3.0, 19.0), (21.0, 19.0), (21.0, 15.0)]);
            line(&[(12.0, 15.0), (17.0, 9.0)]);
            p.circle_filled(point(12.0, 15.0), 1.5 * scale, color);
        }
        Metric::LocalDelay => {
            line(&[
                (2.0, 8.0),
                (12.0, 3.0),
                (22.0, 8.0),
                (12.0, 13.0),
                (2.0, 8.0),
            ]);
            line(&[(2.0, 13.0), (12.0, 18.0), (22.0, 13.0)]);
            line(&[(2.0, 17.0), (12.0, 22.0), (22.0, 17.0)]);
        }
        Metric::PresentationStall => {
            p.rect_stroke(
                Rect::from_min_max(point(2.0, 3.0), point(22.0, 17.0)),
                2.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            line(&[(12.0, 17.0), (12.0, 21.0)]);
            line(&[(8.0, 21.0), (16.0, 21.0)]);
            line(&[(9.0, 7.0), (9.0, 13.0)]);
            line(&[(15.0, 7.0), (15.0, 13.0)]);
        }
    }
}
