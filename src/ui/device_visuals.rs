use super::center_icon::{Icon, paint_icon};
use super::theme::{BG, MUTED};
use egui::{Color32, FontId, Stroke, vec2};

pub(crate) fn system_icon(p: &egui::Painter, rect: egui::Rect, platform: farwing_protocol::Platform) {
    use farwing_protocol::Platform;
    let r = rect.shrink(rect.width() * 0.12);
    match platform {
        Platform::Windows => {
            let side = (r.width() - r.width() * 0.1) / 2.0;
            for y in 0..2 {
                for x in 0..2 {
                    p.rect_filled(
                        egui::Rect::from_min_size(
                            r.min
                                + vec2(
                                    x as f32 * (side + r.width() * 0.1),
                                    y as f32 * (side + r.width() * 0.1),
                                ),
                            vec2(side, side),
                        ),
                        1.0,
                        Color32::from_rgb(84, 163, 255),
                    );
                }
            }
        }
        Platform::Macos => {
            p.rect_filled(r, 4.0, Color32::from_rgb(109, 180, 245));
            p.rect_filled(
                egui::Rect::from_min_max(r.center_top(), r.max),
                3.0,
                Color32::from_rgb(223, 237, 250),
            );
            for x in [0.28, 0.72] {
                p.line_segment(
                    [
                        r.min + vec2(r.width() * x, r.height() * 0.3),
                        r.min + vec2(r.width() * x, r.height() * 0.43),
                    ],
                    Stroke::new(1.4, BG),
                );
            }
            p.add(egui::Shape::line(
                vec![
                    r.min + vec2(r.width() * 0.25, r.height() * 0.67),
                    r.min + vec2(r.width() * 0.5, r.height() * 0.75),
                    r.min + vec2(r.width() * 0.76, r.height() * 0.66),
                ],
                Stroke::new(1.4, BG),
            ));
        }
        Platform::Android => {
            let green = Color32::from_rgb(118, 214, 160);
            let head = egui::Rect::from_min_max(
                r.min + vec2(0.0, r.height() * 0.2),
                r.max - vec2(0.0, r.height() * 0.12),
            );
            p.rect_filled(
                head,
                egui::CornerRadius {
                    nw: 12,
                    ne: 12,
                    sw: 3,
                    se: 3,
                },
                green,
            );
            for x in [0.3, 0.7] {
                p.circle_filled(
                    r.min + vec2(r.width() * x, r.height() * 0.48),
                    r.width() * 0.045,
                    BG,
                );
                p.line_segment(
                    [
                        r.min + vec2(r.width() * x, r.height() * 0.24),
                        r.min + vec2(r.width() * (if x < 0.5 { 0.15 } else { 0.85 }), 0.0),
                    ],
                    Stroke::new(1.5, green),
                );
            }
        }
        Platform::Ios => {
            p.rect_filled(r, 5.0, Color32::from_rgb(226, 230, 244));
            p.text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                "iOS",
                FontId::proportional(r.width() * 0.35),
                BG,
            );
        }
        Platform::Web => {
            // A globe: a browser login.
            let blue = Color32::from_rgb(84, 163, 255);
            let stroke = Stroke::new((r.width() * 0.08).max(1.2), blue);
            let c = r.center();
            let radius = r.width() * 0.46;
            p.circle_stroke(c, radius, stroke);
            p.line_segment([c - vec2(radius, 0.0), c + vec2(radius, 0.0)], stroke);
            let meridian = (0..=16)
                .map(|i| {
                    let a = std::f32::consts::PI * (i as f32 / 16.0 - 0.5);
                    c + vec2(a.cos() * radius * 0.45, a.sin() * radius)
                })
                .collect::<Vec<_>>();
            p.add(egui::Shape::line(meridian.clone(), stroke));
            p.add(egui::Shape::line(
                meridian.iter().map(|q| egui::pos2(2.0 * c.x - q.x, q.y)).collect(),
                stroke,
            ));
        }
        _ => paint_icon(p, r, Icon::Monitor, MUTED),
    }
}

// Shared image framing for every device; no device-name or brand detection.
fn wallpaper_uv(
    source: egui::Vec2,
    viewport: egui::Vec2,
    zoom: f32,
    anchor: egui::Vec2,
) -> egui::Rect {
    let scale = (viewport.x / source.x).max(viewport.y / source.y) * zoom;
    let visible = viewport / (source * scale);
    let offset = (egui::Vec2::splat(1.0) - visible) * anchor;
    egui::Rect::from_min_size(egui::pos2(offset.x, offset.y), visible)
}

pub(crate) fn wallpaper(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    texture: Option<&egui::TextureHandle>,
    platform: farwing_protocol::Platform,
) {
    use farwing_protocol::Platform;
    if let Some(texture) = texture {
        let source = texture.size_vec2();
        let uv = wallpaper_uv(source, rect.size(), 1.1, vec2(0.5, 0.5));
        egui::Image::new((texture.id(), source))
            .uv(uv)
            .corner_radius(8.0)
            .paint_at(ui, rect);
    } else {
        let p = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
        let colors = match platform {
            Platform::Android => [Color32::from_rgb(22, 61, 58), Color32::from_rgb(35, 82, 75)],
            Platform::Ios | Platform::Macos => [
                Color32::from_rgb(48, 42, 76),
                Color32::from_rgb(81, 72, 115),
            ],
            _ => [
                Color32::from_rgb(25, 45, 74),
                Color32::from_rgb(41, 80, 113),
            ],
        };
        p.rect_filled(rect, 8.0, colors[0]);
        for i in 0..4 {
            p.circle_stroke(
                rect.right_center() - vec2(rect.width() * 0.14, rect.height() * 0.2),
                rect.height() * (0.45 + i as f32 * 0.27),
                Stroke::new(rect.height() * 0.12, colors[1].gamma_multiply(0.5)),
            );
        }
    }
}

/// Detail backdrop: the same cover/offset rule for every image.
/// Fade before the information rows; the content layout never follows image height.
pub(crate) fn detail_wallpaper(
    bounds: egui::Rect,
    texture: Option<&egui::TextureHandle>,
    fade_start: f32,
    fade_end: f32,
) -> egui::Shape {
    let Some(texture) = texture else {
        return egui::Shape::Noop;
    };
    let source = texture.size_vec2();
    // A stable framing area keeps composition independent of window height
    // and of whether this device has power controls. Preserve image aspect.
    let frame_size = vec2(bounds.width(), 380.0);
    let uv = wallpaper_uv(source, frame_size, 1.05, vec2(0.5, 0.46));
    let image_height = frame_size.y / uv.height();
    let height = (1.0 - uv.top()) * image_height;
    let end = fade_end.min(height);
    let start = fade_start.min(end * 0.5);
    let mut mesh = egui::Mesh::with_texture(texture.id());
    let mut rows = vec![(0.0, 1.0), (start, 1.0)];
    for i in 1..=24 {
        let t = i as f32 / 24.0;
        let alpha = 1.0 - t * t * (3.0 - 2.0 * t);
        rows.push((start + (end - start) * t, alpha));
    }
    rows.push((height, 0.0));
    for (i, (y, alpha)) in rows.into_iter().enumerate() {
        let color = Color32::from_white_alpha((alpha * 255.0).round() as u8);
        for (x, u) in [(bounds.left(), uv.left()), (bounds.right(), uv.right())] {
            mesh.vertices.push(egui::epaint::Vertex {
                pos: egui::pos2(x, bounds.top() + y),
                uv: egui::pos2(u, (uv.top() + y / image_height).min(1.0)),
                color,
            });
        }
        if i > 0 {
            let v = i as u32 * 2;
            mesh.indices
                .extend_from_slice(&[v - 2, v - 1, v, v, v - 1, v + 1]);
        }
    }
    egui::Shape::mesh(mesh)
}
