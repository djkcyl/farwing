//! A channel's messages (`docs/protocol/chat-design.md` §4, §12.1): only the
//! rows in view are laid out; history loads as the list scrolls; the "new
//! messages" line, replies, edits, pins, attachments, read receipts.
use super::parts::{self, Glyph, RichAction};
use super::{ChatUi, Confirmed, Dialog};
use crate::features::chat::list::Heights;
use crate::features::chat::state::{ChatState, Entry, id_after};
use crate::features::chat::{ChatService, ImageKey};
use crate::i18n::t;
use crate::ui::theme::{AMBER, HOVER, LINE, MUTED, RED, SURFACE, TEXT};
use egui::{Color32, Pos2, Rect, RichText, Sense, Stroke, vec2};
use farwing_protocol::chat::{Attachment, Channel, Dm, GroupState, Id, perm};
use std::time::Duration;

const GROUP_MS: i64 = 7 * 60 * 1000;

enum Row {
    Start,
    Day(i64),
    Unread,
    Message { index: usize, continued: bool },
}

fn key(row: &Row, list: &[Entry]) -> i64 {
    match row {
        Row::Start => -3,
        Row::Day(ms) => -(10 + parts::day_of(*ms)),
        Row::Unread => -2,
        Row::Message { index, .. } => {
            let m = &list[*index].message;
            if m.id.0 >= i64::MAX - 2_000_000 { m.id.0 - (*index as i64) } else { m.id.0 }
        }
    }
}

pub(super) fn list(
    ui: &mut egui::Ui,
    chat: &ChatService,
    state: &ChatState,
    chat_ui: &mut ChatUi,
    channel: &Channel,
    group: Option<&GroupState>,
    dm: Option<&Dm>,
) {
    let box_ = state.messages.get(&channel.id);
    if box_.is_none() {
        chat.load_latest(channel.id);
    }
    let empty = crate::features::chat::state::ChannelMessages::default();
    let box_ = box_.unwrap_or(&empty);
    let me = state.me().map(|m| m.user_id.clone()).unwrap_or_default();

    // A newly opened channel starts at the bottom; its "new" line is where
    // reading stopped.
    if chat_ui.scroll.channel != Some(channel.id) {
        chat_ui.scroll = super::Scroll {
            channel: Some(channel.id),
            offset: Some(f32::MAX),
            at_bottom: true,
            read_at_open: state.reads.get(&channel.id).map(|r| r.last_read_id).filter(|r| id_after(channel.last_message_id, Some(*r))),
            ..Default::default()
        };
        if box_.list.is_empty() || box_.newer {
            chat.load_latest(channel.id);
        }
    }

    let mut rows = Vec::new();
    if !box_.older && !box_.loading {
        rows.push(Row::Start);
    }
    let mut unread_shown = false;
    for (i, e) in box_.list.iter().enumerate() {
        let prev = i.checked_sub(1).map(|j| &box_.list[j].message);
        let m = &e.message;
        let new_day = prev.is_none_or(|p| parts::day_of(p.created_at) != parts::day_of(m.created_at));
        if new_day {
            rows.push(Row::Day(m.created_at));
        }
        let unread = !unread_shown
            && !e.sending
            && m.author.user_id != me
            && chat_ui.scroll.read_at_open.is_some_and(|r| m.id > r);
        if unread {
            unread_shown = true;
            rows.push(Row::Unread);
        }
        let continued = !new_day
            && !unread
            && m.reply.is_none()
            && prev.is_some_and(|p| !p.deleted && p.author.user_id == m.author.user_id && m.created_at - p.created_at < GROUP_MS);
        rows.push(Row::Message { index: i, continued });
    }
    let keys: Vec<i64> = rows.iter().map(|r| key(r, &box_.list)).collect();

    // The other side's read position (direct messages): under my last
    // message it covers.
    let read_mark = dm.and_then(|d| d.peer_read_id).and_then(|peer| {
        box_.list
            .iter()
            .rev()
            .find(|e| e.message.author.user_id == me && !e.sending && e.message.id <= peer)
            .map(|e| e.message.id)
    });

    // Out of the map while drawing; put back below.
    let mut heights = chat_ui.heights.remove(&channel.id).unwrap_or_else(|| Heights::new(44.0));
    // Older messages added above keep the view where it was.
    let first = keys.first().copied();
    let mut area = egui::ScrollArea::vertical()
        .id_salt(("chat-messages", channel.id))
        .auto_shrink([false, false]);
    if let (Some(prev), Some(_)) = (chat_ui.scroll.first, first)
        && first != Some(prev)
        && !chat_ui.scroll.at_bottom
        && chat_ui.scroll.offset.is_none()
        && let Some(at) = keys.iter().position(|k| *k == prev)
    {
        chat_ui.scroll.offset = Some(chat_ui.scroll.last + heights.offset_of(&keys, at));
    }
    chat_ui.scroll.first = first;
    // A jump to a message.
    let mut highlight = None;
    if let Some((ch, target, at)) = state.jump
        && ch == channel.id
        && at.elapsed() < Duration::from_secs(3)
    {
        highlight = Some(target);
        if let Some(index) = rows.iter().position(|r| matches!(r, Row::Message { index, .. } if box_.list[*index].message.id == target))
            && at.elapsed() < Duration::from_millis(400)
        {
            chat_ui.scroll.offset = Some((heights.offset_of(&keys, index) - 80.0).max(0.0));
            chat_ui.scroll.at_bottom = false;
        }
    }
    // At the bottom it stays there while rows arrive and get measured. The
    // offset is worked out here: an unclamped one draws a blank frame.
    let view_h = ui.available_height();
    let bottom = (heights.total(&keys) - view_h).max(0.0);
    let offset = match chat_ui.scroll.offset.take() {
        Some(o) if o >= f32::MAX => Some(bottom),
        None if chat_ui.scroll.at_bottom => Some(bottom),
        other => other,
    };
    if let Some(offset) = offset {
        area = area.vertical_scroll_offset(offset);
    }

    let mut actions = Vec::new();
    let output = area.show_viewport(ui, |ui, viewport| {
        let width = ui.available_width();
        if (width - chat_ui.scroll.width).abs() > 1.0 {
            heights.clear();
            chat_ui.scroll.width = width;
        }
        let window = heights.window(&keys, viewport.min.y, viewport.max.y, 600.0);
        let origin = ui.max_rect().min;
        ui.set_min_height(window.total.max(viewport.height()));
        // Short lists sit at the bottom, as chats do.
        let lift = (viewport.height() - window.total).max(0.0);
        let mut y = window.top + lift;
        let pointer = ui.ctx().pointer_hover_pos();
        for i in window.first..window.end {
            let row_key = keys[i];
            let known = heights.height(row_key);
            let rect = Rect::from_min_size(origin + vec2(0.0, y), vec2(width, known));
            let hovered = pointer.is_some_and(|p| rect.contains(p)) && ui.rect_contains_pointer(ui.clip_rect());
            let used = ui
                .scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(Rect::from_min_size(rect.min, vec2(width, 100_000.0)))
                        .id_salt(("chat-row", row_key)),
                    |ui| match &rows[i] {
                        Row::Start => start(ui, channel, dm),
                        Row::Day(ms) => divider(ui, &parts::day_text(*ms), MUTED, LINE),
                        Row::Unread => divider(ui, t().chat.new_messages, RED, RED.gamma_multiply(0.6)),
                        Row::Message { index, continued } => {
                            let e = &box_.list[*index];
                            let lit = highlight == Some(e.message.id);
                            message(ui, chat, state, chat_ui, e, *continued, hovered, lit, channel, group, &mut actions);
                            if read_mark == Some(e.message.id) {
                                ui.horizontal(|ui| {
                                    ui.add_space(56.0);
                                    ui.label(parts::muted(t().chat.read));
                                });
                            }
                        }
                    },
                )
                .response
                .rect
                .height();
            if heights.set(row_key, used) {
                ui.ctx().request_repaint();
            }
            y += used;
        }
        if viewport.min.y < 400.0 {
            chat.load_older(channel.id);
        }
        if box_.newer && viewport.max.y > window.total - 400.0 {
            chat.load_newer(channel.id);
        }
    });
    chat_ui.heights.insert(channel.id, heights);
    chat_ui.scroll.last = output.state.offset.y;
    // Against this frame's model: rows drawn past an estimate inflate egui's
    // content size.
    let at_bottom = output.state.offset.y >= bottom - 30.0;
    chat_ui.scroll.at_bottom = at_bottom && !box_.newer;
    let focused = ui.ctx().input(|i| i.viewport().focused.unwrap_or(true));
    if chat_ui.scroll.at_bottom && focused && !box_.cached {
        chat.mark_read(channel.id);
    }
    if !at_bottom || box_.newer {
        let rect = ui.max_rect();
        let button = Rect::from_min_size(rect.right_bottom() - vec2(150.0, 44.0), vec2(130.0, 30.0));
        let response = ui.interact(button, ui.id().with("chat-latest"), Sense::click());
        ui.painter().rect(button, 15.0, SURFACE, Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
        ui.painter().text(button.center(), egui::Align2::CENTER_CENTER, t().chat.jump_latest, egui::FontId::proportional(13.0), TEXT);
        if response.clicked() {
            if box_.newer {
                chat.load_latest(channel.id);
            }
            chat_ui.scroll.offset = Some(f32::MAX);
        }
    }
    for a in actions {
        act(ui.ctx(), chat, chat_ui, a);
    }
}

fn start(ui: &mut egui::Ui, channel: &Channel, dm: Option<&Dm>) {
    ui.add_space(20.0);
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.vertical(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(56.0, 56.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 28.0, SURFACE);
            parts::paint_glyph(ui.painter(), rect.shrink(16.0), if dm.is_some() { Glyph::Bubble } else { Glyph::Hash }, TEXT);
            let name = match dm {
                Some(d) => parts::display_name(&d.user, ""),
                None => format!("#{}", channel.name),
            };
            ui.label(RichText::new((t().chat.start_of)(&name)).size(19.0).strong().color(TEXT));
            if !channel.topic.is_empty() {
                ui.label(parts::muted(&channel.topic));
            }
        });
    });
    ui.add_space(8.0);
}

fn divider(ui: &mut egui::Ui, text: &str, color: Color32, line: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
    let galley = ui.painter().layout_no_wrap(text.to_owned(), egui::FontId::proportional(12.0), color);
    let w = galley.size().x;
    let mid = rect.center();
    ui.painter().hline(rect.left() + 16.0..=mid.x - w / 2.0 - 8.0, mid.y, Stroke::new(1.0, line));
    ui.painter().hline(mid.x + w / 2.0 + 8.0..=rect.right() - 16.0, mid.y, Stroke::new(1.0, line));
    ui.painter().galley(mid - galley.size() / 2.0, galley, color);
}

/// What a click in the list asks for.
enum Action {
    Reply(farwing_protocol::chat::Message),
    Edit(farwing_protocol::chat::Message),
    Pin(Id, bool),
    Copy(String),
    Delete(Id),
    User(String, Pos2),
    Link(String),
    Image(Attachment),
    Download(Attachment),
    Jump(Id, Id),
    Resend(Id, Entry),
    Discard(Id, Entry),
}

#[allow(clippy::too_many_arguments)]
fn message(
    ui: &mut egui::Ui,
    chat: &ChatService,
    state: &ChatState,
    chat_ui: &mut ChatUi,
    e: &Entry,
    continued: bool,
    hovered: bool,
    lit: bool,
    channel: &Channel,
    group: Option<&GroupState>,
    actions: &mut Vec<Action>,
) {
    let m = &e.message;
    let top = ui.cursor().min.y;
    let full = ui.available_width();
    if !continued {
        ui.add_space(8.0);
    }
    let member = group.and_then(|g| state.members.get(&g.group.id)).and_then(|l| l.iter().find(|x| x.user.user_id == m.author.user_id));
    let color = group.zip(member).and_then(|(g, mm)| {
        g.roles
            .iter()
            .filter(|r| mm.roles.contains(&r.id) && r.color != 0)
            .max_by_key(|r| r.position)
            .map(|r| parts::rgb(r.color))
    });
    if let Some(reply) = &m.reply {
        ui.horizontal(|ui| {
            ui.add_space(56.0);
            let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
            parts::paint_glyph(ui.painter(), r, Glyph::Reply, MUTED);
            if reply.deleted {
                ui.label(parts::muted(t().chat.reply_deleted));
            } else {
                let names = state.names(group);
                let text = crate::features::chat::markdown::plain_message(&reply.content, &names);
                let preview = if text.is_empty() { (t().chat.attachment_count)(reply.attachments as usize) } else { text };
                let line = format!("@{}  {preview}", parts::display_name(&reply.author, ""));
                let response = parts::emoji_line(ui, chat, &line, 12.0, MUTED);
                if response.clicked() {
                    actions.push(Action::Jump(m.channel_id, reply.id));
                }
            }
        });
    }
    ui.horizontal_top(|ui| {
        ui.add_space(9.0);
        ui.allocate_ui_with_layout(vec2(38.0, 20.0), egui::Layout::top_down(egui::Align::Center), |ui| {
            // The column keeps its width when empty (continued rows).
            ui.set_min_width(38.0);
            if continued {
                if hovered {
                    ui.label(RichText::new(parts::time_text(m.created_at).rsplit(' ').next().unwrap_or("")).size(10.5).color(MUTED));
                }
            } else {
                let response = parts::avatar(ui, chat, &m.author, 38.0, None);
                if response.clicked() {
                    actions.push(Action::User(m.author.user_id.clone(), response.rect.right_top()));
                }
            }
        });
        ui.add_space(9.0);
        ui.vertical(|ui| {
            ui.set_width(full - 56.0 - 16.0);
            ui.spacing_mut().item_spacing.y = 2.0;
            if !continued {
                ui.horizontal(|ui| {
                    let name = parts::display_name(&m.author, member.map_or("", |x| x.nickname.as_str()));
                    let response = ui.add(egui::Label::new(RichText::new(name).strong().color(color.unwrap_or(TEXT))).sense(Sense::click()));
                    if response.clicked() {
                        actions.push(Action::User(m.author.user_id.clone(), response.rect.right_top()));
                    }
                    ui.label(RichText::new(parts::time_text(m.created_at)).size(11.5).color(MUTED));
                    if m.pinned {
                        let (r, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
                        parts::paint_glyph(ui.painter(), r, Glyph::Pin, AMBER);
                    }
                });
            }
            if m.deleted {
                ui.label(RichText::new(t().chat.deleted).italics().size(13.0).color(MUTED));
                return;
            }
            if let Some(call) = m.call {
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                    let color = if call.missed { RED } else { crate::ui::theme::GREEN };
                    parts::paint_glyph(ui.painter(), r, Glyph::Phone, color);
                    let v = &t().voice;
                    let text = if call.missed { v.call_missed.to_owned() } else { (v.call_record)(call.duration_s.div_ceil(60).max(1)) };
                    ui.label(RichText::new(text).color(MUTED));
                });
                return;
            }
            let faded = e.sending || e.failed.is_some();
            ui.scope(|ui| {
                if faded {
                    ui.set_opacity(0.6);
                }
                if !m.content.is_empty()
                    && let Some(a) = parts::rich(ui, chat, state, group, &m.content, m.id, &mut chat_ui.spoilers)
                {
                    actions.push(match a {
                        RichAction::User(u, p) => Action::User(u, p),
                        RichAction::Link(h) => Action::Link(h),
                    });
                }
            });
            if m.edited_at.is_some() {
                ui.label(parts::muted(format!("({})", t().chat.edited)));
            }
            for a in &m.attachments {
                attachment(ui, chat, a, actions);
            }
            if let Some(why) = &e.failed {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(if why.is_empty() { t().chat.failed } else { why.as_str() }).size(12.0).color(RED));
                    if ui.link(t().chat.resend).clicked() {
                        actions.push(Action::Resend(m.channel_id, e.clone()));
                    }
                    if ui.link(t().chat.discard).clicked() {
                        actions.push(Action::Discard(m.channel_id, e.clone()));
                    }
                });
            }
        });
    });
    if continued {
        ui.add_space(1.0);
    }
    let row = Rect::from_min_max(egui::pos2(ui.max_rect().left(), top), egui::pos2(ui.max_rect().left() + full, ui.cursor().min.y));
    if lit {
        ui.painter().rect_filled(row, 0.0, Color32::from_rgba_unmultiplied(75, 136, 235, 30));
    } else if hovered {
        ui.painter().rect_filled(row, 0.0, Color32::from_rgba_unmultiplied(255, 255, 255, 5));
    }
    if hovered && !m.deleted && !e.sending && e.failed.is_none() {
        toolbar(ui, row, m, channel, state, actions);
    }
}

fn toolbar(ui: &mut egui::Ui, row: Rect, m: &farwing_protocol::chat::Message, channel: &Channel, state: &ChatState, actions: &mut Vec<Action>) {
    let mine = state.me().is_some_and(|me| me.user_id == m.author.user_id);
    let can_send = channel.permissions & perm::SEND != 0;
    let can_manage = channel.permissions & perm::MANAGE_MESSAGES != 0;
    let can_pin = can_manage || channel.kind == farwing_protocol::chat::ChannelKind::Dm;
    let mut items: Vec<(Glyph, &str, Action)> = Vec::new();
    if can_send {
        items.push((Glyph::Reply, t().chat.reply, Action::Reply(m.clone())));
    }
    if mine && can_send {
        items.push((Glyph::Pencil, t().chat.edit, Action::Edit(m.clone())));
    }
    if can_pin {
        items.push((Glyph::Pin, if m.pinned { t().chat.unpin } else { t().chat.pin }, Action::Pin(m.id, !m.pinned)));
    }
    if !m.content.is_empty() {
        items.push((Glyph::Copy, t().chat.copy, Action::Copy(m.content.clone())));
    }
    if mine || can_manage {
        items.push((Glyph::Trash, t().chat.delete, Action::Delete(m.id)));
    }
    if items.is_empty() {
        return;
    }
    let w = items.len() as f32 * 28.0 + 6.0;
    let bar = Rect::from_min_size(egui::pos2(row.right() - w - 16.0, row.top() - 12.0), vec2(w, 32.0));
    let painter = ui.painter().clone().with_layer_id(egui::LayerId::new(egui::Order::Foreground, ui.id().with("chat-toolbar")));
    painter.rect(bar, 6.0, SURFACE, Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
    for (i, (glyph, hint, action)) in items.into_iter().enumerate() {
        let r = Rect::from_min_size(bar.min + vec2(3.0 + i as f32 * 28.0, 2.0), vec2(28.0, 28.0));
        let response = ui.interact(r, ui.id().with(("chat-tool", m.id, i)), Sense::click()).on_hover_text(hint);
        if response.hovered() {
            painter.rect_filled(r, 5.0, HOVER);
        }
        parts::paint_glyph(&painter, r.shrink(6.0), glyph, if response.hovered() { TEXT } else { MUTED });
        if response.clicked() {
            actions.push(action);
        }
    }
}

fn attachment(ui: &mut egui::Ui, chat: &ChatService, a: &Attachment, actions: &mut Vec<Action>) {
    if a.thumb && a.width > 0 {
        let scale = (400.0 / a.width as f32).min(300.0 / a.height as f32).min(1.0);
        let size = vec2(a.width as f32 * scale, a.height as f32 * scale).min(vec2(ui.available_width(), 300.0));
        let (rect, response) = ui.allocate_exact_size(size, Sense::click());
        match chat.image(&ImageKey::Thumb(a.id)) {
            Some(t) => {
                egui::Image::new(&t).corner_radius(8.0).paint_at(ui, rect);
            }
            None => {
                ui.painter().rect_filled(rect, 8.0, SURFACE);
            }
        }
        if response.on_hover_text(&a.name).clicked() {
            actions.push(Action::Image(a.clone()));
        }
        return;
    }
    parts::card_frame().show(ui, |ui| {
        ui.set_width(340.0f32.min(ui.available_width()));
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
            parts::paint_glyph(ui.painter(), r, Glyph::File, MUTED);
            ui.vertical(|ui| {
                ui.add(egui::Label::new(RichText::new(&a.name).color(TEXT)).truncate());
                ui.label(parts::muted(super::panels::bytes(a.size)));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if parts::glyph_button(ui, Glyph::Down, t().chat.download, false).clicked() {
                    actions.push(Action::Download(a.clone()));
                }
            });
        });
    });
}

fn act(ctx: &egui::Context, chat: &ChatService, chat_ui: &mut ChatUi, a: Action) {
    match a {
        Action::Reply(m) => {
            chat_ui.editing = None;
            chat_ui.reply = Some(m);
        }
        Action::Edit(m) => {
            chat_ui.reply = None;
            chat_ui.drafts.insert(m.channel_id, m.content.clone());
            chat_ui.editing = Some(m);
        }
        Action::Pin(id, on) => chat.pin(id, on),
        Action::Copy(text) => {
            ctx.copy_text(text);
            chat_ui.toast = Some((t().chat.copied.into(), std::time::Instant::now()));
        }
        Action::Delete(id) => {
            chat_ui.dialog = Some(Dialog::Confirm {
                title: t().chat.delete_question.into(),
                action: Confirmed::Delete(id),
            })
        }
        Action::User(u, p) => chat_ui.card = Some((u, p)),
        Action::Link(href) => {
            let host = url::Url::parse(&href).ok().and_then(|u| u.host_str().map(str::to_owned)).unwrap_or_default();
            if chat_ui.trusted.contains(&host) {
                ctx.open_url(egui::OpenUrl::new_tab(href));
            } else {
                chat_ui.dialog = Some(Dialog::Link { href, trust: false });
            }
        }
        Action::Image(a) => {
            chat_ui.zoom = 1.0;
            chat_ui.image = Some(a);
        }
        Action::Download(a) => chat.download(&a),
        Action::Jump(channel, id) => chat.jump(channel, id),
        Action::Resend(channel, e) => chat.resend(channel, &e),
        Action::Discard(channel, e) => chat.discard(channel, &e),
    }
}
