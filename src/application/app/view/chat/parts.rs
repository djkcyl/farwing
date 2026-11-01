//! Pieces of the chat page: painted icons, avatars and group icons, rich
//! message text (the Markdown subset with emoji images and mentions), times.
use crate::features::chat::markdown::{Block, Inline, Names};
use crate::features::chat::state::ChatState;
use crate::features::chat::{ChatService, ImageKey, emoji};
use crate::i18n::t;
use crate::ui::theme::{ACCENT, LINE, MUTED, SURFACE, TEXT};
use egui::{Color32, Pos2, Rect, RichText, Sense, Stroke, vec2};
use farwing_protocol::chat::{ChatUser, GroupState, Id, Status};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Glyph {
    Hash,
    Speaker,
    Megaphone,
    Bubble,
    Plus,
    Link,
    Compass,
    Pin,
    Bell,
    People,
    Search,
    Reply,
    Pencil,
    Trash,
    Copy,
    Clip,
    Smile,
    Cross,
    Gear,
    Chevron,
    Down,
    File,
    PersonAdd,
    Mic,
    MicOff,
    Headset,
    HeadsetOff,
    Phone,
    PhoneDown,
}

pub(super) fn paint_glyph(p: &egui::Painter, rect: Rect, glyph: Glyph, color: Color32) {
    let c = rect.center();
    let k = rect.height().min(rect.width()) / 20.0;
    let q = |x: f32, y: f32| c + vec2(x * k, y * k);
    let s = Stroke::new(1.4, color);
    match glyph {
        Glyph::Hash => {
            p.line_segment([q(-6.0, -3.0), q(7.0, -3.0)], s);
            p.line_segment([q(-7.0, 3.0), q(6.0, 3.0)], s);
            p.line_segment([q(-1.5, -8.0), q(-3.5, 8.0)], s);
            p.line_segment([q(3.5, -8.0), q(1.5, 8.0)], s);
        }
        Glyph::Speaker => {
            p.add(egui::Shape::closed_line(
                vec![q(-8.0, -3.0), q(-4.0, -3.0), q(1.0, -7.0), q(1.0, 7.0), q(-4.0, 3.0), q(-8.0, 3.0)],
                s,
            ));
            p.add(egui::Shape::line(vec![q(4.0, -3.5), q(5.5, 0.0), q(4.0, 3.5)], s));
            p.add(egui::Shape::line(vec![q(6.5, -6.5), q(9.0, 0.0), q(6.5, 6.5)], s));
        }
        Glyph::Megaphone => {
            p.add(egui::Shape::closed_line(
                vec![q(-8.0, -2.5), q(-4.0, -2.5), q(6.0, -7.5), q(6.0, 7.5), q(-4.0, 2.5), q(-8.0, 2.5)],
                s,
            ));
            p.line_segment([q(-4.0, 2.5), q(-2.5, 8.0)], s);
            p.line_segment([q(9.0, -2.0), q(9.0, 2.0)], s);
        }
        Glyph::Bubble => {
            p.add(egui::Shape::closed_line(
                vec![
                    q(-8.0, -7.0),
                    q(8.0, -7.0),
                    q(8.0, 4.0),
                    q(0.0, 4.0),
                    q(-5.0, 8.0),
                    q(-5.0, 4.0),
                    q(-8.0, 4.0),
                ],
                s,
            ));
        }
        Glyph::Plus => {
            p.line_segment([q(0.0, -7.0), q(0.0, 7.0)], s);
            p.line_segment([q(-7.0, 0.0), q(7.0, 0.0)], s);
        }
        Glyph::Link => {
            p.add(egui::Shape::line(vec![q(-1.0, -4.0), q(2.0, -7.0), q(7.0, -2.0), q(4.0, 1.0)], s));
            p.add(egui::Shape::line(vec![q(1.0, 4.0), q(-2.0, 7.0), q(-7.0, 2.0), q(-4.0, -1.0)], s));
            p.line_segment([q(-2.5, 2.5), q(2.5, -2.5)], s);
        }
        Glyph::Compass => {
            p.circle_stroke(c, 8.0 * k, s);
            p.add(egui::Shape::closed_line(vec![q(4.0, -4.0), q(1.5, 1.5), q(-4.0, 4.0), q(-1.5, -1.5)], s));
        }
        Glyph::Pin => {
            p.add(egui::Shape::closed_line(vec![q(-3.0, -8.0), q(3.0, -8.0), q(2.0, -2.0), q(5.0, 2.0), q(-5.0, 2.0), q(-2.0, -2.0)], s));
            p.line_segment([q(0.0, 2.0), q(0.0, 8.0)], s);
        }
        Glyph::Bell => {
            p.add(egui::Shape::closed_line(
                vec![q(-7.0, 5.0), q(-5.5, 2.0), q(-5.5, -3.0), q(-3.0, -7.0), q(0.0, -8.0), q(3.0, -7.0), q(5.5, -3.0), q(5.5, 2.0), q(7.0, 5.0)],
                s,
            ));
            p.line_segment([q(-2.0, 8.0), q(2.0, 8.0)], s);
        }
        Glyph::People => {
            p.circle_stroke(q(-3.0, -3.5), 3.5 * k, s);
            p.add(egui::Shape::line(vec![q(-9.0, 7.0), q(-8.0, 2.5), q(-3.0, 1.0), q(2.0, 2.5), q(3.0, 7.0)], s));
            p.circle_stroke(q(5.0, -2.0), 2.5 * k, s);
            p.add(egui::Shape::line(vec![q(5.0, 2.0), q(8.0, 3.0), q(9.0, 7.0)], s));
        }
        Glyph::Search => {
            p.circle_stroke(q(-2.0, -2.0), 6.0 * k, s);
            p.line_segment([q(2.5, 2.5), q(8.0, 8.0)], s);
        }
        Glyph::Reply => {
            p.add(egui::Shape::line(vec![q(-2.0, -7.0), q(-8.0, -1.0), q(-2.0, 5.0)], s));
            p.add(egui::Shape::line(vec![q(-8.0, -1.0), q(2.0, -1.0), q(7.0, 2.0), q(8.0, 8.0)], s));
        }
        Glyph::Pencil => {
            p.add(egui::Shape::closed_line(vec![q(-7.0, 7.0), q(-6.0, 2.0), q(4.0, -8.0), q(8.0, -4.0), q(-2.0, 6.0)], s));
        }
        Glyph::Trash => {
            p.line_segment([q(-7.0, -5.0), q(7.0, -5.0)], s);
            p.add(egui::Shape::line(vec![q(-5.0, -5.0), q(-4.0, 8.0), q(4.0, 8.0), q(5.0, -5.0)], s));
            p.add(egui::Shape::line(vec![q(-2.5, -5.0), q(-2.5, -8.0), q(2.5, -8.0), q(2.5, -5.0)], s));
        }
        Glyph::Copy => {
            p.rect_stroke(Rect::from_min_max(q(-3.0, -3.0), q(8.0, 8.0)), 1.5, s, egui::StrokeKind::Inside);
            p.add(egui::Shape::line(vec![q(-5.0, 4.0), q(-7.0, 4.0), q(-7.0, -7.0), q(4.0, -7.0), q(4.0, -5.0)], s));
        }
        Glyph::Clip => {
            p.add(egui::Shape::line(
                vec![q(6.0, -2.0), q(-1.0, 5.0), q(-4.5, 5.0), q(-6.0, 2.0), q(-6.0, 0.0), q(2.0, -8.0), q(5.0, -8.0), q(7.0, -5.0), q(-1.0, 3.0)],
                s,
            ));
        }
        Glyph::Smile => {
            p.circle_stroke(c, 8.0 * k, s);
            p.circle_filled(q(-3.0, -2.5), 1.0 * k, color);
            p.circle_filled(q(3.0, -2.5), 1.0 * k, color);
            p.add(egui::Shape::line(vec![q(-4.0, 2.0), q(-2.0, 4.0), q(2.0, 4.0), q(4.0, 2.0)], s));
        }
        Glyph::Cross => {
            p.line_segment([q(-6.0, -6.0), q(6.0, 6.0)], s);
            p.line_segment([q(6.0, -6.0), q(-6.0, 6.0)], s);
        }
        Glyph::Gear => {
            for (y, knob) in [(-6.0, -3.0), (0.0, 4.0), (6.0, -1.0)] {
                p.line_segment([q(-8.0, y), q(8.0, y)], s);
                p.circle_filled(q(knob, y), 2.2 * k, color);
            }
        }
        Glyph::Chevron => {
            p.add(egui::Shape::line(vec![q(-2.0, -5.0), q(3.0, 0.0), q(-2.0, 5.0)], s));
        }
        Glyph::Down => {
            p.add(egui::Shape::line(vec![q(-5.0, -2.0), q(0.0, 3.0), q(5.0, -2.0)], s));
        }
        Glyph::File => {
            p.add(egui::Shape::closed_line(vec![q(-6.0, -8.0), q(3.0, -8.0), q(6.0, -5.0), q(6.0, 8.0), q(-6.0, 8.0)], s));
        }
        Glyph::PersonAdd => {
            p.circle_stroke(q(-2.0, -3.0), 3.5 * k, s);
            p.add(egui::Shape::line(vec![q(-8.0, 7.0), q(-7.0, 2.5), q(-2.0, 1.0), q(3.0, 2.5), q(4.0, 7.0)], s));
            p.line_segment([q(7.0, -5.0), q(7.0, 1.0)], s);
            p.line_segment([q(4.0, -2.0), q(10.0, -2.0)], s);
        }
        Glyph::Mic | Glyph::MicOff => {
            p.rect_stroke(
                Rect::from_min_max(q(-3.0, -9.0), q(3.0, 2.0)),
                3.0 * k,
                s,
                egui::StrokeKind::Middle,
            );
            p.add(egui::Shape::line(
                vec![q(-6.5, -1.5), q(-6.0, 1.5), q(-4.0, 4.5), q(0.0, 6.0), q(4.0, 4.5), q(6.0, 1.5), q(6.5, -1.5)],
                s,
            ));
            p.line_segment([q(0.0, 6.0), q(0.0, 9.0)], s);
            if glyph == Glyph::MicOff {
                p.line_segment([q(-8.0, -8.0), q(8.0, 8.0)], Stroke::new(1.6, color));
            }
        }
        Glyph::Headset | Glyph::HeadsetOff => {
            p.add(egui::Shape::line(
                vec![q(-7.0, 3.0), q(-7.0, -1.0), q(-5.5, -5.5), q(-2.5, -7.5), q(2.5, -7.5), q(5.5, -5.5), q(7.0, -1.0), q(7.0, 3.0)],
                s,
            ));
            p.rect_stroke(Rect::from_min_max(q(-8.5, 1.0), q(-4.5, 8.0)), 1.5 * k, s, egui::StrokeKind::Middle);
            p.rect_stroke(Rect::from_min_max(q(4.5, 1.0), q(8.5, 8.0)), 1.5 * k, s, egui::StrokeKind::Middle);
            if glyph == Glyph::HeadsetOff {
                p.line_segment([q(-9.0, -8.0), q(9.0, 9.0)], Stroke::new(1.6, color));
            }
        }
        Glyph::Phone => {
            p.add(egui::Shape::closed_line(
                vec![
                    q(-7.0, -8.0),
                    q(-3.5, -8.0),
                    q(-2.0, -4.0),
                    q(-4.0, -2.0),
                    q(2.0, 4.0),
                    q(4.0, 2.0),
                    q(8.0, 3.5),
                    q(8.0, 7.0),
                    q(6.0, 9.0),
                    q(-1.0, 7.0),
                    q(-7.0, 1.0),
                    q(-9.0, -6.0),
                ],
                s,
            ));
        }
        Glyph::PhoneDown => {
            p.add(egui::Shape::closed_line(
                vec![
                    q(-10.0, 1.0),
                    q(-9.0, -3.0),
                    q(-4.0, -5.0),
                    q(4.0, -5.0),
                    q(9.0, -3.0),
                    q(10.0, 1.0),
                    q(9.0, 4.0),
                    q(5.0, 3.0),
                    q(4.0, 0.0),
                    q(-4.0, 0.0),
                    q(-5.0, 3.0),
                    q(-9.0, 4.0),
                ],
                s,
            ));
        }
    }
}

/// A small square button with a painted glyph.
pub(super) fn glyph_button(ui: &mut egui::Ui, glyph: Glyph, hint: &str, active: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
    let hovered = response.hovered();
    if hovered {
        ui.painter().rect_filled(rect, 5.0, crate::ui::theme::HOVER);
    }
    let color = if active { ACCENT } else if hovered { TEXT } else { MUTED };
    paint_glyph(ui.painter(), rect.shrink(5.0), glyph, color);
    response.on_hover_text(hint)
}

pub(super) fn status_color(status: Status) -> Option<Color32> {
    match status {
        Status::Online => Some(crate::ui::theme::GREEN),
        Status::Idle => Some(crate::ui::theme::AMBER),
        Status::Dnd => Some(crate::ui::theme::RED),
        Status::Invisible => Some(crate::ui::theme::DISABLED),
        Status::Offline => None,
    }
}

pub(super) fn status_text(status: Status) -> &'static str {
    let c = &t().chat;
    match status {
        Status::Online => c.status_online,
        Status::Idle => c.status_idle,
        Status::Dnd => c.status_dnd,
        Status::Invisible => c.status_invisible,
        Status::Offline => c.status_offline,
    }
}

pub(super) fn display_name(user: &ChatUser, nickname: &str) -> String {
    if user.gone && user.username.is_empty() {
        return t().chat.gone_user.to_owned();
    }
    if nickname.is_empty() { user.display_name().to_owned() } else { nickname.to_owned() }
}

fn initial(name: &str) -> String {
    name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_else(|| "?".into())
}

/// A round avatar (the picture, or the name's first letter) with status.
pub(super) fn avatar(ui: &mut egui::Ui, chat: &ChatService, user: &ChatUser, size: f32, status: Option<Status>) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    paint_avatar(ui, chat, rect, user, status);
    response
}

pub(super) fn paint_avatar(ui: &egui::Ui, chat: &ChatService, rect: Rect, user: &ChatUser, status: Option<Status>) {
    let p = ui.painter();
    let texture = (!user.avatar.is_empty())
        .then(|| chat.image(&ImageKey::Avatar(user.user_id.clone(), user.avatar.clone())))
        .flatten();
    match texture {
        Some(t) => {
            egui::Image::new(&t)
                .corner_radius(rect.width() / 2.0)
                .paint_at(ui, rect);
        }
        None => {
            p.circle_filled(rect.center(), rect.width() / 2.0, crate::ui::theme::ACTIVE);
            let name = if user.gone && user.username.is_empty() { "?".to_owned() } else { initial(user.display_name()) };
            p.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                name,
                egui::FontId::proportional(rect.width() * 0.42),
                TEXT,
            );
        }
    }
    if let Some(color) = status.and_then(status_color) {
        let r = (rect.width() * 0.16).max(4.5);
        let at = rect.right_bottom() - vec2(r * 0.8, r * 0.8);
        p.circle_filled(at, r + 2.0, crate::ui::theme::SIDEBAR);
        p.circle_filled(at, r, color);
    }
}

pub(super) fn group_icon(ui: &mut egui::Ui, chat: &ChatService, id: Id, name: &str, icon: &str, size: f32, square: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    paint_group_icon(ui, chat, rect, id, name, icon, square);
    response
}

pub(super) fn paint_group_icon(ui: &egui::Ui, chat: &ChatService, rect: Rect, id: Id, name: &str, icon: &str, square: bool) {
    let radius = if square { 12.0 } else { rect.width() / 2.0 };
    let texture = (!icon.is_empty()).then(|| chat.image(&ImageKey::Icon(id, icon.to_owned()))).flatten();
    match texture {
        Some(t) => {
            egui::Image::new(&t).corner_radius(radius).paint_at(ui, rect);
        }
        None => {
            ui.painter().rect_filled(rect, radius, crate::ui::theme::SURFACE);
            let short: String = name.chars().take(2).collect();
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                short,
                egui::FontId::proportional(rect.width() * 0.3),
                TEXT,
            );
        }
    }
}

/// An emoji image of height `size` (the emoji itself while it loads).
fn emoji_image(ui: &mut egui::Ui, chat: &ChatService, key: &str, text: &str, size: f32) {
    match chat.image(&ImageKey::Emoji(key.to_owned())) {
        Some(t) => {
            ui.add(egui::Image::new(&t).fit_to_exact_size(vec2(size, size)))
                .on_hover_text(text);
        }
        None => {
            ui.label(RichText::new(text).size(size * 0.8));
        }
    }
}

/// One line of plain text with emoji pictures (a reply's preview), cut at
/// the width.
pub(super) fn emoji_line(ui: &mut egui::Ui, chat: &ChatService, text: &str, size: f32, color: Color32) -> egui::Response {
    let clip = ui.available_rect_before_wrap();
    let short: String = text.chars().take(160).collect();
    let inner = ui.scope(|ui| {
        ui.set_clip_rect(clip.intersect(ui.clip_rect()));
        ui.spacing_mut().item_spacing.x = 0.0;
        for piece in emoji::pieces(&short) {
            match piece {
                emoji::Piece::Text(s) => {
                    ui.add(egui::Label::new(RichText::new(s).size(size).color(color)).extend().selectable(false));
                }
                emoji::Piece::Emoji { text, key } => emoji_image(ui, chat, &key, text, size * 1.3),
            }
        }
    });
    ui.interact(inner.response.rect.intersect(clip), ui.id().with("emoji-line"), Sense::click())
}

/// What a click in rich text asks for.
pub(super) enum RichAction {
    User(String, Pos2),
    Link(String),
}

#[derive(Clone, Copy, Default)]
struct Style {
    bold: bool,
    italic: bool,
    strike: bool,
    spoiler_hidden: bool,
}

struct Ctx<'a> {
    chat: &'a ChatService,
    state: &'a ChatState,
    group: Option<&'a GroupState>,
    size: f32,
    spoilers: &'a mut std::collections::HashSet<(Id, usize)>,
    message: Id,
    spoiler_index: usize,
    action: Option<RichAction>,
}

fn styled(text: &str, style: Style, size: f32) -> RichText {
    let mut r = RichText::new(text).size(size).color(TEXT);
    if style.bold {
        r = r.strong();
    }
    if style.italic {
        r = r.italics();
    }
    if style.strike {
        r = r.strikethrough();
    }
    if style.spoiler_hidden {
        r = r.color(Color32::from_rgb(58, 66, 82)).background_color(Color32::from_rgb(58, 66, 82));
    }
    r
}

fn text_run(ui: &mut egui::Ui, cx: &mut Ctx<'_>, text: &str, style: Style) {
    for piece in emoji::pieces(text) {
        match piece {
            emoji::Piece::Text(s) => {
                ui.add(egui::Label::new(styled(s, style, cx.size)).wrap());
            }
            emoji::Piece::Emoji { text, key } => {
                if style.spoiler_hidden {
                    ui.label(styled("▒▒", style, cx.size));
                } else {
                    emoji_image(ui, cx.chat, &key, text, cx.size * 1.35);
                }
            }
        }
    }
}

fn chip(ui: &mut egui::Ui, text: &str, color: Color32, size: f32) -> egui::Response {
    ui.add(
        egui::Label::new(
            RichText::new(text)
                .size(size)
                .color(color)
                .background_color(Color32::from_rgba_unmultiplied(75, 136, 235, 46)),
        )
        .sense(Sense::click()),
    )
}

fn inline(ui: &mut egui::Ui, cx: &mut Ctx<'_>, nodes: &[Inline], style: Style) {
    for n in nodes {
        match n {
            Inline::Text(s) => text_run(ui, cx, s, style),
            Inline::Break => {
                ui.end_row();
            }
            Inline::Code(s) => {
                ui.label(RichText::new(s).code().size(cx.size * 0.92));
            }
            Inline::Bold(c) => inline(ui, cx, c, Style { bold: true, ..style }),
            Inline::Italic(c) => inline(ui, cx, c, Style { italic: true, ..style }),
            Inline::Strike(c) => inline(ui, cx, c, Style { strike: true, ..style }),
            Inline::Spoiler(c) => {
                let key = (cx.message, cx.spoiler_index);
                cx.spoiler_index += 1;
                let hidden = !cx.spoilers.contains(&key);
                let before = ui.min_rect();
                let start = ui.cursor().min;
                inline(ui, cx, c, Style { spoiler_hidden: hidden, ..style });
                if hidden {
                    let area = Rect::from_min_max(start, ui.min_rect().max.max(before.max));
                    let response = ui.interact(area, ui.id().with(key), Sense::click());
                    if response.on_hover_text(t().chat.spoiler).clicked() {
                        cx.spoilers.insert(key);
                    }
                }
            }
            Inline::Link { href, children } => {
                let label: String = crate::features::chat::markdown::plain(children, &cx.state.names(cx.group));
                let response = ui
                    .add(egui::Label::new(RichText::new(label).size(cx.size).color(ACCENT).underline()).sense(Sense::click()))
                    .on_hover_text(href.as_str());
                if response.clicked() {
                    cx.action = Some(RichAction::Link(href.clone()));
                }
            }
            Inline::User(id) => {
                let name = cx.state.names(cx.group).user(id);
                let response = chip(ui, &format!("@{name}"), Color32::from_rgb(156, 192, 255), cx.size);
                if response.clicked() {
                    cx.action = Some(RichAction::User(id.clone(), response.rect.left_bottom()));
                }
            }
            Inline::Role(id) => {
                let role = cx.group.and_then(|g| g.roles.iter().find(|r| r.id.to_string() == *id));
                let color = role
                    .filter(|r| r.color != 0)
                    .map_or(Color32::from_rgb(156, 192, 255), |r| rgb(r.color));
                chip(ui, &format!("@{}", cx.state.names(cx.group).role(id)), color, cx.size);
            }
            Inline::Channel(id) => {
                chip(ui, &format!("#{}", cx.state.names(cx.group).channel(id)), Color32::from_rgb(156, 192, 255), cx.size);
            }
            Inline::Everyone => {
                chip(ui, &format!("@{}", t().chat.mention_all), Color32::from_rgb(156, 192, 255), cx.size);
            }
            Inline::Here => {
                chip(ui, &format!("@{}", t().chat.mention_here), Color32::from_rgb(156, 192, 255), cx.size);
            }
        }
    }
}

pub(super) fn rgb(color: u32) -> Color32 {
    Color32::from_rgb((color >> 16) as u8, (color >> 8) as u8, color as u8)
}

/// A message's text, laid out in `ui`; what a click asked for.
#[allow(clippy::too_many_arguments)]
pub(super) fn rich(
    ui: &mut egui::Ui,
    chat: &ChatService,
    state: &ChatState,
    group: Option<&GroupState>,
    content: &str,
    message: Id,
    spoilers: &mut std::collections::HashSet<(Id, usize)>,
) -> Option<RichAction> {
    let mut cx = Ctx {
        chat,
        state,
        group,
        size: crate::ui::theme::BODY,
        spoilers,
        message,
        spoiler_index: 0,
        action: None,
    };
    if emoji::only_emoji(content) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for p in emoji::pieces(content) {
                if let emoji::Piece::Emoji { text, key } = p {
                    emoji_image(ui, chat, &key, text, 40.0);
                }
            }
        });
        return None;
    }
    for block in crate::features::chat::markdown::parse(content) {
        match block {
            Block::Paragraph(nodes) => {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(0.0, 2.0);
                    inline(ui, &mut cx, &nodes, Style::default());
                });
            }
            Block::Quote(nodes) => {
                egui::Frame::new()
                    .inner_margin(egui::Margin { left: 10, right: 0, top: 1, bottom: 1 })
                    .show(ui, |ui| {
                        let start = ui.cursor().min;
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = vec2(0.0, 2.0);
                            inline(ui, &mut cx, &nodes, Style::default());
                        });
                        let end = ui.min_rect().bottom();
                        ui.painter().line_segment(
                            [Pos2::new(start.x - 8.0, start.y), Pos2::new(start.x - 8.0, end)],
                            Stroke::new(3.0, LINE),
                        );
                    });
            }
            Block::Code { text, .. } => {
                egui::Frame::new()
                    .fill(Color32::from_rgb(18, 22, 28))
                    .stroke(Stroke::new(1.0, LINE))
                    .corner_radius(6.0)
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(egui::Label::new(RichText::new(text).monospace().size(13.0).color(TEXT)).wrap());
                    });
            }
        }
    }
    cx.action
}

/// A message's time: "14:05" today, else with the date.
pub(super) fn time_text(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    let Some(at) = Local.timestamp_millis_opt(ms).single() else { return String::new() };
    let now = Local::now();
    let hm = at.format("%H:%M").to_string();
    if at.date_naive() == now.date_naive() {
        hm
    } else if Some(at.date_naive()) == now.date_naive().pred_opt() {
        format!("{} {hm}", t().chat.yesterday)
    } else {
        format!("{} {hm}", at.format("%Y/%m/%d"))
    }
}

pub(super) fn day_text(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    let Some(at) = Local.timestamp_millis_opt(ms).single() else { return String::new() };
    let now = Local::now();
    if at.date_naive() == now.date_naive() {
        t().chat.today.to_owned()
    } else if Some(at.date_naive()) == now.date_naive().pred_opt() {
        t().chat.yesterday.to_owned()
    } else {
        at.format("%Y/%m/%d").to_string()
    }
}

pub(super) fn day_of(ms: i64) -> i64 {
    use chrono::{Datelike, Local, TimeZone};
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map_or(0, |d| i64::from(d.year()) * 400 + i64::from(d.ordinal()))
}

/// A labelled frame for dialogs and panels.
pub(super) fn card_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(10, 8))
}

pub(super) fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).size(crate::ui::theme::SMALL).color(MUTED)
}

pub(super) fn badge(ui: &egui::Ui, at: Pos2, count: u32) {
    let text = if count > 99 { "99+".to_owned() } else { count.to_string() };
    let galley = ui.painter().layout_no_wrap(text, egui::FontId::proportional(10.5), Color32::WHITE);
    let w = (galley.size().x + 10.0).max(18.0);
    let rect = Rect::from_center_size(at, vec2(w, 17.0));
    ui.painter().rect_filled(rect.expand(2.0), 10.0, crate::ui::theme::SIDEBAR);
    ui.painter().rect_filled(rect, 9.0, crate::ui::theme::DANGER_FILL);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
}
