//! Compact colour editor for annotation; no generic numeric-mode or eyedropper UI.
use crate::ui::theme;
use egui::ecolor::HsvaGamma;
use egui::{Color32, Pos2, Rect, Sense, Stroke, Ui, pos2, vec2};

#[derive(Clone)]
struct Editor {
    hsv: HsvaGamma,
    rgb: [u8; 3],
    hex: String,
    invalid_hex: bool,
}
impl Editor {
    fn new(rgb: [u8; 3]) -> Self {
        Self {
            hsv: Color32::from_rgb(rgb[0], rgb[1], rgb[2]).into(),
            rgb,
            hex: hex(rgb),
            invalid_hex: false,
        }
    }
    fn set_rgb(&mut self, rgb: [u8; 3]) {
        *self = Self::new(rgb);
    }
    fn apply_hsv(&mut self) {
        let c = Color32::from(self.hsv);
        self.rgb = [c.r(), c.g(), c.b()];
        self.hex = hex(self.rgb);
        self.invalid_hex = false;
    }
}
fn hex(rgb: [u8; 3]) -> String {
    format!("{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2])
}
fn colour(h: f32, s: f32, v: f32) -> Color32 {
    Color32::from(HsvaGamma { h, s, v, a: 1. })
}
fn mesh_quad(mesh: &mut egui::Mesh, r: Rect, colours: [Color32; 4]) {
    let first = mesh.vertices.len() as u32;
    for (pos, color) in [
        r.left_top(),
        r.right_top(),
        r.left_bottom(),
        r.right_bottom(),
    ]
    .into_iter()
    .zip(colours)
    {
        mesh.vertices.push(egui::epaint::Vertex {
            pos,
            uv: egui::epaint::WHITE_UV,
            color,
        });
    }
    mesh.indices
        .extend([first, first + 1, first + 2, first + 2, first + 1, first + 3]);
}
fn drag_position(response: &egui::Response) -> Option<Pos2> {
    (response.clicked() || response.dragged() || response.drag_stopped())
        .then(|| response.interact_pointer_pos())
        .flatten()
}
fn marker(ui: &Ui, p: Pos2, r: f32) {
    ui.painter()
        .circle_stroke(p, r + 1., Stroke::new(1., Color32::BLACK));
    ui.painter()
        .circle_stroke(p, r, Stroke::new(2., Color32::WHITE));
}
pub(super) fn show(ui: &mut Ui, rgb: &mut [u8; 3], opacity: &mut u8) {
    let id = ui.id().with("annotation-colour-editor");
    let mut editor = ui
        .data_mut(|d| d.get_temp::<Editor>(id))
        .unwrap_or_else(|| Editor::new(*rgb));
    if editor.rgb != *rgb {
        editor.set_rgb(*rgb);
    }
    let width = theme::ANNOTATION_PICKER_WIDTH;
    ui.set_width(width);
    ui.spacing_mut().item_spacing = vec2(6., 8.);
    ui.spacing_mut().interact_size.y = theme::COMPACT_HEIGHT;
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("画笔颜色")
                .size(theme::COMPACT_TEXT)
                .strong(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let (r, _) = ui.allocate_exact_size(vec2(32., 22.), Sense::hover());
            egui::color_picker::show_color_at(
                ui.painter(),
                Color32::from_rgba_unmultiplied(
                    rgb[0],
                    rgb[1],
                    rgb[2],
                    (u32::from(*opacity) * 255 / 100) as u8,
                ),
                r,
            );
        });
    });
    let (r, response) = ui.allocate_exact_size(
        vec2(width, theme::ANNOTATION_COLOR_PLANE_HEIGHT),
        Sense::click_and_drag(),
    );
    if let Some(p) = drag_position(&response) {
        editor.hsv.s = ((p.x - r.left()) / r.width()).clamp(0., 1.);
        editor.hsv.v = (1. - (p.y - r.top()) / r.height()).clamp(0., 1.);
        editor.apply_hsv();
    }
    let mut mesh = egui::Mesh::default();
    for y in 0..16 {
        for x in 0..16 {
            let s0 = x as f32 / 16.;
            let s1 = (x + 1) as f32 / 16.;
            let v0 = 1. - y as f32 / 16.;
            let v1 = 1. - (y + 1) as f32 / 16.;
            let cell = Rect::from_min_max(
                pos2(r.left() + s0 * r.width(), r.top() + (1. - v0) * r.height()),
                pos2(r.left() + s1 * r.width(), r.top() + (1. - v1) * r.height()),
            );
            mesh_quad(
                &mut mesh,
                cell,
                [
                    colour(editor.hsv.h, s0, v0),
                    colour(editor.hsv.h, s1, v0),
                    colour(editor.hsv.h, s0, v1),
                    colour(editor.hsv.h, s1, v1),
                ],
            );
        }
    }
    ui.painter().add(egui::Shape::mesh(mesh));
    marker(
        ui,
        pos2(
            r.left() + editor.hsv.s * r.width(),
            r.top() + (1. - editor.hsv.v) * r.height(),
        ),
        4.,
    );
    response.on_hover_text("选择饱和度与明度");

    let (r, response) = ui.allocate_exact_size(
        vec2(width, theme::ANNOTATION_COLOR_BAR_HEIGHT),
        Sense::click_and_drag(),
    );
    if let Some(p) = drag_position(&response) {
        editor.hsv.h = ((p.x - r.left()) / r.width()).clamp(0., 1.);
        editor.apply_hsv();
    }
    let mut mesh = egui::Mesh::default();
    for x in 0..48 {
        let h0 = x as f32 / 48.;
        let h1 = (x + 1) as f32 / 48.;
        let cell = Rect::from_min_max(
            pos2(r.left() + h0 * r.width(), r.top()),
            pos2(r.left() + h1 * r.width(), r.bottom()),
        );
        mesh_quad(
            &mut mesh,
            cell,
            [
                colour(h0, 1., 1.),
                colour(h1, 1., 1.),
                colour(h0, 1., 1.),
                colour(h1, 1., 1.),
            ],
        );
    }
    ui.painter().add(egui::Shape::mesh(mesh));
    marker(
        ui,
        pos2(r.left() + editor.hsv.h * r.width(), r.center().y),
        4.,
    );
    response.on_hover_text("色相");

    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("不透明度")
                .size(theme::SMALL)
                .color(theme::MUTED),
        );
        let (r, response) = ui.allocate_exact_size(
            vec2(width - 120., theme::ANNOTATION_COLOR_BAR_HEIGHT),
            Sense::click_and_drag(),
        );
        if let Some(p) = drag_position(&response) {
            *opacity = (((p.x - r.left()) / r.width()).clamp(0., 1.) * 100.)
                .round()
                .clamp(1., 100.) as u8;
        }
        for y in 0..2 {
            for x in 0..((r.width() / 7.).ceil() as usize) {
                let cell =
                    Rect::from_min_size(r.min + vec2(x as f32 * 7., y as f32 * 7.), vec2(7., 7.))
                        .intersect(r);
                ui.painter().rect_filled(
                    cell,
                    0,
                    if (x + y) % 2 == 0 {
                        theme::HOVER
                    } else {
                        theme::SURFACE
                    },
                );
            }
        }
        let c = Color32::from_rgb(editor.rgb[0], editor.rgb[1], editor.rgb[2]);
        let mut mesh = egui::Mesh::default();
        mesh_quad(
            &mut mesh,
            r,
            [Color32::TRANSPARENT, c, Color32::TRANSPARENT, c],
        );
        ui.painter().add(egui::Shape::mesh(mesh));
        marker(
            ui,
            pos2(r.left() + *opacity as f32 / 100. * r.width(), r.center().y),
            4.,
        );
        super::number_input(
            ui,
            vec2(52., theme::COMPACT_HEIGHT),
            egui::DragValue::new(opacity)
                .range(1..=100)
                .speed(1.)
                .suffix("%"),
        );
    });
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("HEX")
                .size(theme::SMALL)
                .color(theme::MUTED),
        );
        let response = ui.add(
            super::singleline(&mut editor.hex, theme::COMPACT_HEIGHT)
                .desired_width(width - 40.)
                .char_limit(7)
                .font(egui::FontId::monospace(theme::COMPACT_TEXT)),
        );
        if response.lost_focus()
            || (response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
        {
            let text = editor.hex.trim().trim_start_matches('#');
            if let Ok(value) = u32::from_str_radix(text, 16)
                && text.len() == 6
            {
                editor.set_rgb([(value >> 16) as u8, (value >> 8) as u8, value as u8]);
            } else {
                editor.invalid_hex = true;
            }
        }
    });
    crate::ui::controls::observe_notice(
        ui.ctx(),
        "annotation-color-error",
        "颜色格式无效",
        crate::ui::controls::DialogIcon::Error,
        editor.invalid_hex.then_some("请输入六位十六进制颜色"),
    );
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.;
        for preset in theme::ANNOTATION_COLORS {
            let (r, response) = ui.allocate_exact_size(vec2(32., 26.), Sense::click());
            ui.painter().rect_filled(
                r.shrink2(vec2(2., 3.)),
                theme::CONTROL_RADIUS,
                Color32::from_rgb(preset[0], preset[1], preset[2]),
            );
            if editor.rgb == preset {
                ui.painter().rect_stroke(
                    r,
                    theme::CONTROL_RADIUS,
                    Stroke::new(1., theme::TEXT),
                    egui::StrokeKind::Inside,
                );
            }
            if response.clicked() {
                editor.set_rgb(preset);
            }
        }
    });
    *rgb = editor.rgb;
    ui.data_mut(|d| d.insert_temp(id, editor));
}
