//! The other participants of a multi-participant session
//! (`docs/protocol/v2-design.md` §4.2–4.3). This participant's pointer is
//! reported to the host; the others' pointers, clicks and the keyboard and
//! mouse turn are shown over the picture and in the toolbar list.
use crate::features::participants::{Member, PointerView};
use crate::features::remote_input::MouseMode;
use crate::features::stream_control::StreamControlHandle;
use crate::i18n::t;
use crate::protocol::participants::{Click, button, phase};
use crate::ui::theme;
use egui::{Color32, FontId, Pos2, Rect, Stroke, Vec2, pos2, vec2};
use std::sync::Arc;

/// Arrow cursor outline in points, tip at the origin.
const ARROW: [(f32, f32); 7] = [
    (0.0, 0.0),
    (0.0, 16.5),
    (4.3, 12.4),
    (7.4, 19.0),
    (10.0, 17.8),
    (7.0, 11.4),
    (11.8, 11.4),
];
/// [`ARROW`] as triangles: one mesh keeps partial opacity free of seams.
const ARROW_TRIANGLES: [u32; 15] = [0, 1, 2, 0, 2, 5, 0, 5, 6, 2, 3, 4, 2, 4, 5];
/// Name tag offset from the pointer tip, clear of a standard arrow.
const LABEL_OFFSET: Vec2 = vec2(10.0, 18.0);
const LABEL_PADDING: Vec2 = vec2(6.0, 2.0);
const LABEL_MAX_WIDTH: f32 = 160.0;

/// This participant's pointer over one playback window, and the repaint
/// wake-up for the others' pointers.
pub(super) struct Presence {
    /// The pointer was over this window's picture when last reported.
    inside: bool,
    /// Buttons pressed over the picture whose release is still owed.
    pressed: u8,
    _repaint: Arc<dyn Fn() + Send + Sync>,
}

impl Presence {
    pub(super) fn new(control: &StreamControlHandle, context: &egui::Context) -> Self {
        let ctx = context.clone();
        Self {
            inside: false,
            pressed: 0,
            _repaint: control
                .participants()
                .subscribe(move || ctx.request_repaint()),
        }
    }

    /// Report this frame's pointer and clicks. `picture` shows remote screen
    /// `screen` in window points; `active` is false while the window is
    /// unfocused or minimized, a transition covers the picture, or a locked
    /// game cursor no longer follows the local pointer.
    pub(super) fn share(
        &mut self,
        ctx: &egui::Context,
        control: &StreamControlHandle,
        screen: i32,
        picture: Option<Rect>,
        active: bool,
    ) {
        let picture = picture.filter(|_| active);
        if picture.is_none() {
            self.pressed = 0;
        }
        let locate = |pos: Pos2| {
            // Local menus and panels over the picture hide the pointer.
            if ctx
                .layer_id_at(pos)
                .is_some_and(|layer| layer.order != egui::Order::Background)
            {
                return None;
            }
            normalized(picture?, pos)
        };
        for event in ctx.input(|i| i.events.clone()) {
            let egui::Event::PointerButton {
                pos,
                button,
                pressed,
                ..
            } = event
            else {
                continue;
            };
            let Some(button) = wire_button(button) else {
                continue;
            };
            let bit = 1 << button;
            let at = locate(pos);
            if pressed && at.is_some() {
                self.pressed |= bit;
            } else if !pressed && self.pressed & bit != 0 {
                self.pressed &= !bit;
            } else {
                continue;
            }
            self.inside |= at.is_some();
            let phase = if pressed { phase::DOWN } else { phase::UP };
            control.send_pointer(screen, at, Some(Click { button, phase }));
        }
        let position = ctx.input(|i| i.pointer.latest_pos()).and_then(locate);
        let participants = control.participants();
        if position.is_some() {
            self.inside = true;
            control.send_pointer(screen, position, None);
            if let Some(delay) = participants.pointer_retry() {
                ctx.request_repaint_after(delay);
            }
        } else if std::mem::take(&mut self.inside) && participants.pointer_screen() == Some(screen)
        {
            // Another window may already report the pointer on its own screen.
            control.send_pointer(screen, None, None);
        }
    }
}

/// Paint the other participants' pointers and click rings on `screen`,
/// shown at `picture`. Paint only: no area, response or hit region.
pub(super) fn paint_pointers(
    ctx: &egui::Context,
    control: &StreamControlHandle,
    screen: i32,
    picture: Rect,
) {
    let participants = control.participants();
    if let Some(delay) = participants.next_frame() {
        ctx.request_repaint_after(delay);
    }
    if picture.width() <= 0.0 || picture.height() <= 0.0 {
        return;
    }
    // Background paint stays above the video but below every menu.
    let painter = ctx
        .layer_painter(egui::LayerId::background())
        .with_clip_rect(picture);
    for click in participants.clicks() {
        if click.screen_id == screen {
            paint_click(
                &painter,
                on_picture(picture, [click.x, click.y]),
                click.button,
                click.progress,
            );
        }
    }
    for pointer in participants.pointers() {
        if pointer.screen_id == screen {
            paint_pointer(&painter, picture, &pointer);
        }
    }
}

/// While another participant holds the control token, tell this one that its
/// own input is not applied (only briefly, until the token moves here or
/// control turns off). Paint only, under the toolbar.
pub(super) fn paint_turn_hint(ctx: &egui::Context, control: &StreamControlHandle, picture: Rect) {
    if control.mouse().mode() == MouseMode::View {
        return;
    }
    let Some(holder) = control.participants().controlled_by_other() else {
        return;
    };
    let painter = ctx
        .layer_painter(egui::LayerId::background())
        .with_clip_rect(picture);
    let galley = painter.layout(
        (t().viewer.participant_in_control)(display_name(&holder.nickname)),
        FontId::proportional(theme::SMALL),
        theme::TEXT,
        (picture.width() - 64.0).max(1.0),
    );
    let size = galley.size() + vec2(38.0, 12.0);
    let rect = Rect::from_min_size(
        pos2(picture.center().x - size.x / 2.0, picture.top() + 12.0),
        size,
    );
    painter.rect_filled(rect, theme::PANEL_RADIUS, theme::VIEWER_SCRIM);
    painter.circle_filled(
        pos2(rect.left() + 15.0, rect.center().y),
        4.0,
        participant_color(holder.color),
    );
    painter.galley(rect.min + vec2(26.0, 6.0), galley, theme::TEXT);
}

/// Toolbar hint for the participants button.
pub(super) fn button_hint(members: &[Member]) -> String {
    if members.len() > 1 {
        (t().viewer.participants_count)(members.len())
    } else {
        t().viewer.participants.to_owned()
    }
}

/// The participants list opened from the toolbar button.
/// `forwarded`: the picture comes through the server's forwarder, end-to-end
/// encrypted or not.
pub(super) fn menu(response: &egui::Response, members: &[Member], forwarded: Option<bool>) {
    egui::Popup::menu(response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .gap(6.0)
        .width(theme::PARTICIPANTS_MENU_WIDTH)
        .show(|ui| {
            super::stream_menu::menu_style(ui);
            ui.set_width(theme::PARTICIPANTS_MENU_WIDTH);
            ui.spacing_mut().item_spacing.y = 2.0;
            let (header, _) = ui.allocate_exact_size(
                vec2(ui.available_width(), theme::MENU_HEIGHT),
                egui::Sense::hover(),
            );
            ui.painter().text(
                header.left_center() + vec2(10.0, 0.0),
                egui::Align2::LEFT_CENTER,
                (t().viewer.participants_header)(members.len()),
                FontId::proportional(theme::SMALL),
                theme::MUTED,
            );
            if let Some(e2ee) = forwarded {
                let (line, _) = ui.allocate_exact_size(
                    vec2(ui.available_width(), theme::MENU_HEIGHT),
                    egui::Sense::hover(),
                );
                let viewer = &t().viewer;
                ui.painter().text(
                    line.left_center() + vec2(10.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    if e2ee {
                        viewer.participants_forwarded_e2ee
                    } else {
                        viewer.participants_forwarded_compatible
                    },
                    FontId::proportional(theme::SMALL),
                    theme::MUTED,
                );
            }
            for member in members {
                ui.push_id(member.id, |ui| participant_row(ui, member));
            }
        });
}

fn participant_row(ui: &mut egui::Ui, member: &Member) {
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::PARTICIPANTS_ROW_HEIGHT),
        egui::Sense::hover(),
    );
    let painter = ui.painter();
    if member.controlling {
        painter.rect_filled(rect, theme::CONTROL_RADIUS, theme::SURFACE);
    } else if response.hovered() {
        painter.rect_filled(rect, theme::CONTROL_RADIUS, theme::HOVER);
    }
    let y = rect.center().y;
    painter.circle_filled(
        pos2(rect.left() + 15.0, y),
        5.0,
        participant_color(member.color),
    );

    let (status, status_color) = if member.controlling {
        (t().viewer.status_in_control, theme::GREEN)
    } else if member.control {
        (t().viewer.status_can_control, theme::MUTED)
    } else {
        (t().viewer.status_view_only, theme::MUTED)
    };
    let status = painter.layout_no_wrap(
        status.into(),
        FontId::proportional(theme::TINY),
        status_color,
    );
    let mut right = rect.right() - 10.0 - status.size().x;
    painter.galley(pos2(right, y - status.size().y / 2.0), status, status_color);
    if member.controlling {
        right -= 20.0;
        paint_keyboard(painter, pos2(right + 7.0, y), theme::GREEN);
    }

    let badge = relation_label(&member.relation).map(|label| {
        painter.layout_no_wrap(
            label.into(),
            FontId::proportional(theme::TINY),
            theme::MUTED,
        )
    });
    let badge_width = badge.as_ref().map_or(0.0, |g| g.size().x + 10.0 + 6.0);
    let name_left = rect.left() + 30.0;
    let mut job = egui::text::LayoutJob::default();
    job.append(
        display_name(&member.nickname),
        0.0,
        egui::TextFormat::simple(FontId::proportional(theme::COMPACT_TEXT), theme::TEXT),
    );
    if member.you {
        job.append(
            t().viewer.participant_you,
            4.0,
            egui::TextFormat::simple(FontId::proportional(theme::COMPACT_TEXT), theme::MUTED),
        );
    }
    job.wrap.max_width = (right - 10.0 - badge_width - name_left).max(0.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let name = painter.layout_job(job);
    let name_right = name_left + name.size().x;
    painter.galley(pos2(name_left, y - name.size().y / 2.0), name, theme::TEXT);
    if let Some(badge) = badge {
        let pill = Rect::from_min_size(
            pos2(name_right + 6.0, y - badge.size().y / 2.0 - 1.0),
            badge.size() + vec2(10.0, 2.0),
        );
        painter.rect(
            pill,
            theme::CONTROL_RADIUS,
            Color32::TRANSPARENT,
            Stroke::new(1.0, theme::LINE),
            egui::StrokeKind::Inside,
        );
        painter.galley(pill.min + vec2(5.0, 1.0), badge, theme::MUTED);
    }
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Label,
            true,
            display_name(&member.nickname),
        )
    });
    if !member.device_name.is_empty() {
        response.on_hover_text((t().viewer.participant_device)(&member.device_name));
    }
}

fn paint_pointer(painter: &egui::Painter, picture: Rect, pointer: &PointerView) {
    let opacity = pointer.opacity.clamp(0.0, 1.0);
    let tip = on_picture(picture, [pointer.x, pointer.y]);
    let color = participant_color(pointer.color).gamma_multiply(opacity);
    // The turn holder's pointer is the host's own cursor, already in the
    // video: only its name tag is added.
    if !pointer.controlling {
        let points: Vec<Pos2> = ARROW.iter().map(|&(x, y)| tip + vec2(x, y)).collect();
        painter.add(egui::Shape::closed_line(
            points.iter().map(|p| *p + vec2(0.0, 1.0)).collect(),
            Stroke::new(2.5, Color32::from_black_alpha((70.0 * opacity) as u8)),
        ));
        let mut mesh = egui::Mesh::default();
        for point in &points {
            mesh.colored_vertex(*point, color);
        }
        for triangle in ARROW_TRIANGLES.chunks_exact(3) {
            mesh.add_triangle(triangle[0], triangle[1], triangle[2]);
        }
        painter.add(egui::Shape::mesh(mesh));
        painter.add(egui::Shape::closed_line(
            points,
            Stroke::new(1.2, Color32::WHITE.gamma_multiply(opacity)),
        ));
    }
    let mut job = egui::text::LayoutJob::simple_singleline(
        display_name(&pointer.nickname).to_owned(),
        FontId::proportional(theme::TINY),
        Color32::WHITE.gamma_multiply(opacity),
    );
    job.wrap.max_width = LABEL_MAX_WIDTH;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let name = painter.layout_job(job);
    let tag = label_rect(picture, tip, name.size() + LABEL_PADDING * 2.0);
    painter.rect_filled(tag, tag.height() / 2.0, color);
    painter.galley(tag.min + LABEL_PADDING, name, Color32::WHITE);
}

fn paint_click(painter: &egui::Painter, center: Pos2, button: i32, progress: f32) {
    let progress = progress.clamp(0.0, 1.0);
    let color = match button {
        button::LEFT => theme::POINTER_LEFT_CLICK,
        button::RIGHT => theme::POINTER_RIGHT_CLICK,
        _ => theme::POINTER_MIDDLE_CLICK,
    };
    // Ease out: the ring opens quickly, then settles while it fades.
    let radius = 6.0 + 16.0 * (1.0 - (1.0 - progress).powi(2));
    painter.circle_stroke(
        center,
        radius,
        Stroke::new(2.5, color.gamma_multiply(1.0 - progress)),
    );
}

fn paint_keyboard(painter: &egui::Painter, center: Pos2, color: Color32) {
    let stroke = Stroke::new(1.1, color);
    painter.rect_stroke(
        Rect::from_center_size(center, vec2(14.0, 9.0)),
        1.5,
        stroke,
        egui::StrokeKind::Inside,
    );
    for x in [-3.5, 0.0, 3.5] {
        painter.circle_filled(center + vec2(x, -1.5), 0.7, color);
    }
    painter.line_segment([center + vec2(-3.0, 1.8), center + vec2(3.0, 1.8)], stroke);
}

/// `pos` as the normalized point of the remote screen shown in `picture`,
/// or `None` outside the picture. Right and bottom edges are outside, as
/// for remote input.
fn normalized(picture: Rect, pos: Pos2) -> Option<[f32; 2]> {
    let x = (pos.x - picture.left()) / picture.width();
    let y = (pos.y - picture.top()) / picture.height();
    ((0.0..1.0).contains(&x) && (0.0..1.0).contains(&y)).then_some([x, y])
}

/// A normalized remote screen point within `picture`.
fn on_picture(picture: Rect, [x, y]: [f32; 2]) -> Pos2 {
    picture.min + vec2(x * picture.width(), y * picture.height())
}

/// Where a pointer's name tag of `size` goes: below-right of the arrow,
/// flipped left or up near the picture's right and bottom edges.
fn label_rect(picture: Rect, tip: Pos2, size: Vec2) -> Rect {
    let mut min = tip + LABEL_OFFSET;
    if min.x + size.x > picture.right() {
        min.x = tip.x - size.x - 2.0;
    }
    if min.y + size.y > picture.bottom() {
        min.y = tip.y - size.y - 4.0;
    }
    min.x = min.x.clamp(
        picture.left(),
        (picture.right() - size.x).max(picture.left()),
    );
    min.y = min.y.clamp(
        picture.top(),
        (picture.bottom() - size.y).max(picture.top()),
    );
    Rect::from_min_size(min, size)
}

fn wire_button(button: egui::PointerButton) -> Option<i32> {
    match button {
        egui::PointerButton::Primary => Some(button::LEFT),
        egui::PointerButton::Secondary => Some(button::RIGHT),
        egui::PointerButton::Middle => Some(button::MIDDLE),
        _ => None,
    }
}

fn relation_label(relation: &str) -> Option<&'static str> {
    let t = &t().viewer;
    match relation {
        "own" => Some(t.relation_own),
        "friend" => Some(t.relation_friend),
        "assist" => Some(t.relation_guest),
        "trusted" => Some(t.relation_trusted),
        _ => None,
    }
}

fn display_name(nickname: &str) -> &str {
    if nickname.trim().is_empty() {
        t().viewer.participant
    } else {
        nickname
    }
}

fn participant_color(rgb: u32) -> Color32 {
    let [_, r, g, b] = rgb.to_be_bytes();
    Color32::from_rgb(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picture_points_round_trip_and_reject_outside() {
        let picture = Rect::from_min_size(pos2(10.0, 40.0), vec2(200.0, 100.0));
        assert_eq!(normalized(picture, pos2(10.0, 40.0)), Some([0.0, 0.0]));
        assert_eq!(normalized(picture, pos2(110.0, 65.0)), Some([0.5, 0.25]));
        let [x, y] = normalized(picture, pos2(57.0, 123.0)).unwrap();
        assert!(on_picture(picture, [x, y]).distance(pos2(57.0, 123.0)) < 1e-3);
        for outside in [
            pos2(9.9, 50.0),
            pos2(210.0, 50.0),
            pos2(50.0, 39.0),
            pos2(50.0, 140.0),
        ] {
            assert_eq!(normalized(picture, outside), None);
        }
        let empty = Rect::from_min_size(pos2(10.0, 40.0), Vec2::ZERO);
        assert_eq!(normalized(empty, pos2(10.0, 40.0)), None);
    }

    #[test]
    fn name_tags_stay_inside_the_picture() {
        let picture = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0));
        let size = vec2(60.0, 16.0);
        let tag = label_rect(picture, pos2(100.0, 100.0), size);
        assert_eq!(tag.min, pos2(100.0, 100.0) + LABEL_OFFSET);
        let corner = label_rect(picture, pos2(395.0, 295.0), size);
        assert!(picture.contains_rect(corner));
        assert!(corner.right() <= 395.0 && corner.bottom() <= 295.0);
        let tiny = Rect::from_min_size(Pos2::ZERO, vec2(30.0, 10.0));
        assert_eq!(label_rect(tiny, pos2(20.0, 5.0), size).min, Pos2::ZERO);
    }

    #[test]
    fn buttons_and_relations_map_to_the_protocol() {
        assert_eq!(
            wire_button(egui::PointerButton::Primary),
            Some(button::LEFT)
        );
        assert_eq!(
            wire_button(egui::PointerButton::Secondary),
            Some(button::RIGHT)
        );
        assert_eq!(
            wire_button(egui::PointerButton::Middle),
            Some(button::MIDDLE)
        );
        assert_eq!(wire_button(egui::PointerButton::Extra1), None);
        assert!(relation_label("assist").is_some());
        assert_eq!(relation_label(""), None);
        assert_eq!(display_name("Ann"), "Ann");
        assert!(!display_name(" ").trim().is_empty());
        assert_eq!(
            participant_color(0x3B82F6),
            Color32::from_rgb(0x3B, 0x82, 0xF6)
        );
    }
}
