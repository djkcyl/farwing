//! Icons used in the device center.
use egui::{Color32, Stroke, vec2};

#[derive(Clone, Copy)]
pub(crate) enum Icon {
    Monitor,
    Settings,
    Refresh,
    Info,
    Assist,
    Edit,
    Logout,
    Logs,
    #[cfg(windows)]
    Plugins,
    Keyboard,
    Diagnostics,
    Friends,
    /// This device and who may connect to it.
    Host,
    /// Records: a clock.
    Records,
    /// Notifications: a bell.
    Notices,
    /// Chat: a speech bubble.
    Chat,
}

pub(crate) fn paint_icon(p: &egui::Painter, rect: egui::Rect, icon: Icon, color: Color32) {
    let c = rect.center();
    let q = |x, y| c + vec2(x, y);
    let s = Stroke::new(1.5, color);
    match icon {
        Icon::Diagnostics => {
            p.add(egui::Shape::line(
                vec![
                    q(-10.0, 0.0),
                    q(-5.0, 0.0),
                    q(-2.0, -8.0),
                    q(2.0, 8.0),
                    q(5.0, 0.0),
                    q(10.0, 0.0),
                ],
                s,
            ));
        }
        Icon::Keyboard => crate::ui::controls::paint_keyboard(p, rect, color),
        Icon::Notices => {
            p.add(egui::Shape::closed_line(
                vec![
                    q(-7.0, 5.0),
                    q(-5.5, 2.0),
                    q(-5.5, -3.0),
                    q(-3.0, -7.0),
                    q(0.0, -8.0),
                    q(3.0, -7.0),
                    q(5.5, -3.0),
                    q(5.5, 2.0),
                    q(7.0, 5.0),
                ],
                s,
            ));
            p.line_segment([q(-2.0, 8.0), q(2.0, 8.0)], s);
        }
        Icon::Chat => {
            p.add(egui::Shape::closed_line(
                vec![
                    q(-9.0, -7.0),
                    q(9.0, -7.0),
                    q(9.0, 4.0),
                    q(-1.0, 4.0),
                    q(-6.0, 8.5),
                    q(-6.0, 4.0),
                    q(-9.0, 4.0),
                ],
                s,
            ));
        }
        Icon::Records => {
            p.circle_stroke(c, 8.5, s);
            p.add(egui::Shape::line(vec![q(0.0, -5.0), q(0.0, 0.0), q(4.0, 3.0)], s));
        }
        Icon::Friends => {
            p.circle_stroke(q(-4.0, -4.0), 3.5, s);
            p.circle_stroke(q(5.0, -2.0), 3.0, s);
            p.add(egui::Shape::line(
                vec![q(-11.0, 8.0), q(-9.0, 2.0), q(-4.0, 1.0), q(1.0, 2.0), q(3.0, 8.0)],
                s,
            ));
            p.add(egui::Shape::line(vec![q(4.0, 3.0), q(8.0, 3.5), q(10.0, 8.0)], s));
        }
        #[cfg(windows)]
        Icon::Plugins => {
            crate::plugins::paint_plugin_icon(p, rect, color);
        }
        Icon::Logs => {
            p.rect_stroke(
                egui::Rect::from_center_size(c, vec2(15.0, 19.0)),
                2.0,
                s,
                egui::StrokeKind::Inside,
            );
            for y in [-5.0, 0.0, 5.0] {
                p.line_segment([q(-4.0, y), q(4.0, y)], s);
            }
        }
        Icon::Host => {
            // A shield with a check: who may connect here.
            p.add(egui::Shape::closed_line(
                vec![
                    q(0.0, -10.0),
                    q(8.0, -7.0),
                    q(8.0, 0.0),
                    q(5.0, 6.0),
                    q(0.0, 10.0),
                    q(-5.0, 6.0),
                    q(-8.0, 0.0),
                    q(-8.0, -7.0),
                ],
                s,
            ));
            p.add(egui::Shape::line(vec![q(-4.0, 0.0), q(-1.0, 3.5), q(4.5, -3.0)], s));
        }
        Icon::Assist => {
            p.circle_stroke(q(-4.0, -5.0), 3.0, s);
            p.circle_stroke(q(6.0, -3.0), 2.5, s);
            p.add(egui::Shape::line(
                vec![
                    q(-10.0, 7.0),
                    q(-8.0, 1.0),
                    q(-3.0, 0.0),
                    q(2.0, 3.0),
                    q(3.0, 7.0),
                ],
                s,
            ));
            p.add(egui::Shape::line(
                vec![q(5.0, 2.0), q(9.0, 3.0), q(11.0, 7.0)],
                s,
            ));
        }
        Icon::Edit => {
            p.add(egui::Shape::closed_line(
                vec![
                    q(-8.0, 8.0),
                    q(-6.0, 2.0),
                    q(5.0, -9.0),
                    q(9.0, -5.0),
                    q(-2.0, 6.0),
                ],
                s,
            ));
        }
        Icon::Monitor => {
            p.rect_stroke(
                egui::Rect::from_center_size(q(0.0, -2.0), vec2(20.0, 14.0)),
                2.0,
                s,
                egui::StrokeKind::Inside,
            );
            p.line_segment([q(0.0, 5.0), q(0.0, 9.0)], s);
            p.line_segment([q(-5.0, 9.0), q(5.0, 9.0)], s);
        }
        Icon::Settings => {
            for (y, knob) in [(-6.0, -3.0), (0.0, 4.0), (6.0, -1.0)] {
                p.line_segment([q(-9.0, y), q(9.0, y)], s);
                p.circle_filled(q(knob, y), 2.5, color);
            }
        }
        Icon::Refresh => {
            let points = (0..=32)
                .map(|i| {
                    let angle = std::f32::consts::FRAC_PI_4
                        + i as f32 / 32.0 * (std::f32::consts::TAU - std::f32::consts::FRAC_PI_4);
                    q(-2.0, 0.0) + vec2(angle.cos(), angle.sin()) * 8.0
                })
                .collect();
            p.add(egui::Shape::line(points, s));
            p.add(egui::Shape::line(
                vec![q(2.0, -4.0), q(6.0, 0.0), q(10.0, -4.0)],
                s,
            ));
        }
        Icon::Info => {
            p.circle_stroke(c, 8.0, s);
            p.circle_filled(q(0.0, -3.5), 1.0, color);
            p.line_segment([q(0.0, 0.0), q(0.0, 4.0)], s);
        }
        Icon::Logout => {
            p.add(egui::Shape::line(
                vec![q(-1.0, -8.0), q(-8.0, -8.0), q(-8.0, 8.0), q(-1.0, 8.0)],
                s,
            ));
            p.line_segment([q(-2.0, 0.0), q(9.0, 0.0)], s);
            p.add(egui::Shape::line(
                vec![q(5.0, -4.0), q(9.0, 0.0), q(5.0, 4.0)],
                s,
            ));
        }
    }
}
