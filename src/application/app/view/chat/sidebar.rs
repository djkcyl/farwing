//! The groups rail and the column beside it: a group's channels, or the
//! direct messages and invitations; the user's bar at the bottom.
use super::parts::{self, Glyph};
use super::{ChatUi, Dialog};
use crate::features::chat::ChatService;
use crate::features::chat::state::{ChatState, ordered};
use crate::i18n::t;
use crate::ui::theme::{ACCENT, GREEN, HOVER, LINE, MUTED, SELECTED, SIDEBAR, SURFACE, TEXT};
use egui::{Align, Color32, Rect, RichText, Sense, Stroke, vec2};
use farwing_protocol::chat::{ChannelKind, Id, Notify};
use reqwest::Method;

/// One item of the rail; whether it was clicked.
fn rail_item(ui: &mut egui::Ui, active: bool, dot: bool, badge: u32, hint: &str, paint: impl FnOnce(&egui::Ui, Rect)) -> bool {
    let (rect, response) = ui.allocate_exact_size(vec2(super::RAIL, 52.0), Sense::click());
    let icon = Rect::from_center_size(rect.center(), vec2(44.0, 44.0));
    let bar_h = if active { 34.0 } else if response.hovered() { 18.0 } else if dot { 8.0 } else { 0.0 };
    if bar_h > 0.0 {
        ui.painter().rect_filled(
            Rect::from_center_size(egui::pos2(rect.left() + 2.0, rect.center().y), vec2(4.0, bar_h)),
            2.0,
            TEXT,
        );
    }
    paint(ui, icon);
    if badge > 0 {
        parts::badge(ui, icon.right_bottom() - vec2(4.0, 4.0), badge);
    }
    response.on_hover_text(hint).clicked()
}

fn glyph_icon(ui: &egui::Ui, rect: Rect, glyph: Glyph, active: bool, accent: Color32) {
    ui.painter().rect_filled(rect, if active { 14.0 } else { 22.0 }, if active { ACCENT } else { SURFACE });
    parts::paint_glyph(ui.painter(), rect.shrink(11.0), glyph, if active { Color32::WHITE } else { accent });
}

pub(super) fn rail(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi) {
    let Some(snap) = &state.snap else { return };
    egui::ScrollArea::vertical()
        .id_salt("chat-rail-scroll")
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add_space(8.0);
            let dm_badge = snap
                .dms
                .iter()
                .filter(|d| state.unread(&d.channel).0 && state.notify_level(d.channel.id, None) != Notify::None)
                .count() as u32
                + snap.group_invites.len() as u32;
            let dm_active = chat_ui.group.is_none();
            if rail_item(ui, dm_active, false, dm_badge, t().chat.dms, |ui, r| glyph_icon(ui, r, Glyph::Bubble, dm_active, TEXT)) {
                chat_ui.select_group(None, state);
            }
            let (sep, _) = ui.allocate_exact_size(vec2(super::RAIL, 10.0), Sense::hover());
            ui.painter().hline(sep.center().x - 16.0..=sep.center().x + 16.0, sep.center().y, Stroke::new(2.0, LINE));
            for g in &snap.groups {
                let mut mentions = 0;
                let mut unread = false;
                for c in &g.channels {
                    let (u, m) = state.unread(c);
                    mentions += m;
                    unread |= u && c.kind != ChannelKind::Category && state.notify_level(c.id, Some(g.group.id)) != Notify::None;
                }
                let active = chat_ui.group == Some(g.group.id);
                if rail_item(ui, active, unread, mentions, &g.group.name, |ui, r| {
                    parts::paint_group_icon(ui, chat, r, g.group.id, &g.group.name, &g.group.icon, active);
                }) {
                    chat_ui.select_group(Some(g.group.id), state);
                }
            }
            if snap.limits.create_groups
                && rail_item(ui, false, false, 0, t().chat.create_group, |ui, r| glyph_icon(ui, r, Glyph::Plus, false, GREEN))
            {
                chat_ui.dialog = Some(Dialog::Create {
                    name: String::new(),
                    description: String::new(),
                });
            }
            if rail_item(ui, false, false, 0, t().chat.join_group, |ui, r| glyph_icon(ui, r, Glyph::Link, false, GREEN)) {
                chat_ui.dialog = Some(Dialog::Join { text: String::new() });
            }
            if rail_item(ui, false, false, 0, t().chat.discover, |ui, r| glyph_icon(ui, r, Glyph::Compass, false, GREEN)) {
                chat.ask("discover", Method::GET, "/api/v2/chat/discover?q=".into(), None);
                chat_ui.dialog = Some(Dialog::Discover {
                    query: String::new(),
                    sent: String::new(),
                });
            }
            ui.add_space(8.0);
        });
}

fn header(ui: &mut egui::Ui, title: &str, buttons: impl FnOnce(&mut egui::Ui)) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 48.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, LINE));
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect.shrink2(vec2(12.0, 0.0)))
            .layout(egui::Layout::left_to_right(Align::Center)),
        |ui| {
            ui.add(egui::Label::new(RichText::new(title).strong().color(TEXT)).truncate());
            ui.with_layout(egui::Layout::right_to_left(Align::Center), buttons);
        },
    );
}

/// A row of the channel or direct-message list.
fn row(ui: &mut egui::Ui, active: bool, unread: bool, faded: bool, paint: impl FnOnce(&egui::Ui, Rect, Color32)) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::click());
    if active {
        ui.painter().rect_filled(rect, 5.0, SELECTED);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 5.0, HOVER);
    }
    let mut color = if active || unread { TEXT } else { Color32::from_rgb(120, 131, 148) };
    if faded {
        color = color.gamma_multiply(0.6);
    }
    paint(ui, rect, color);
    response
}

pub(super) fn side(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi) {
    let full = ui.max_rect();
    let bar = Rect::from_min_max(egui::pos2(full.left(), full.bottom() - 52.0), full.max);
    let list = Rect::from_min_max(full.min, egui::pos2(full.right(), bar.top()));
    ui.scope_builder(egui::UiBuilder::new().max_rect(list), |ui| match chat_ui.group {
        Some(g) => group_side(ui, chat, state, chat_ui, g),
        None => dm_side(ui, chat, state, chat_ui),
    });
    ui.painter().rect_filled(bar, 0.0, SIDEBAR);
    ui.painter().hline(bar.x_range(), bar.top(), Stroke::new(1.0, LINE));
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(bar.shrink2(vec2(10.0, 8.0)))
            .layout(egui::Layout::left_to_right(Align::Center)),
        |ui| user_bar(ui, chat, state, chat_ui),
    );
}

fn group_side(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi, group: Id) {
    use farwing_protocol::chat::perm;
    let Some(g) = state.group(group) else { return };
    let p = g.group.permissions;
    header(ui, &g.group.name, |ui| {
        let manage = p & (perm::MANAGE_GROUP | perm::MANAGE_CHANNELS | perm::MANAGE_ROLES | perm::KICK | perm::BAN | perm::MUTE) != 0;
        if parts::glyph_button(ui, if manage { Glyph::Gear } else { Glyph::Down }, t().chat.group_settings, false).clicked() {
            chat_ui.settings.open(g);
            chat_ui.dialog = Some(Dialog::GroupSettings);
        }
        if p & perm::INVITE != 0 && parts::glyph_button(ui, Glyph::PersonAdd, t().chat.invite, false).clicked() {
            chat.ask("friends", Method::GET, "/api/v2/friends".into(), None);
            chat_ui.dialog = Some(Dialog::Invite {
                group,
                uses: 0,
                expires: 4,
            });
        }
    });
    egui::ScrollArea::vertical().id_salt(("chat-channels", group)).show(ui, |ui| {
        ui.add_space(6.0);
        egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 0)).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            for c in ordered(&g.channels) {
                if c.kind == ChannelKind::Category {
                    let folded = chat_ui.folded.contains(&c.id);
                    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::click());
                    let color = if response.hovered() { MUTED } else { Color32::from_rgb(108, 120, 139) };
                    let chevron = Rect::from_center_size(egui::pos2(rect.left() + 8.0, rect.bottom() - 9.0), vec2(10.0, 10.0));
                    parts::paint_glyph(ui.painter(), chevron, if folded { Glyph::Chevron } else { Glyph::Down }, color);
                    ui.painter().text(
                        egui::pos2(rect.left() + 16.0, rect.bottom() - 9.0),
                        egui::Align2::LEFT_CENTER,
                        &c.name,
                        egui::FontId::proportional(12.0),
                        color,
                    );
                    if response.clicked() {
                        if folded {
                            chat_ui.folded.remove(&c.id);
                        } else {
                            chat_ui.folded.insert(c.id);
                        }
                    }
                    continue;
                }
                let (unread, mentions) = state.unread(c);
                let muted = state.notify_level(c.id, Some(group)) == Notify::None;
                if c.parent_id.is_some_and(|p| chat_ui.folded.contains(&p)) && chat_ui.channel != Some(c.id) && mentions == 0 {
                    continue;
                }
                let active = chat_ui.channel == Some(c.id);
                let glyph = match c.kind {
                    ChannelKind::Voice => Glyph::Speaker,
                    ChannelKind::Announcement => Glyph::Megaphone,
                    _ => Glyph::Hash,
                };
                let response = row(ui, active, unread && !muted, muted, |ui, rect, color| {
                    parts::paint_glyph(ui.painter(), Rect::from_center_size(egui::pos2(rect.left() + 14.0, rect.center().y), vec2(16.0, 16.0)), glyph, color);
                    let font = if unread && !muted { egui::FontId::proportional(14.0) } else { egui::FontId::proportional(14.0) };
                    let galley = ui.painter().layout(c.name.clone(), font, color, rect.width() - 64.0);
                    ui.painter().galley(egui::pos2(rect.left() + 28.0, rect.center().y - galley.size().y / 2.0), galley, color);
                    if mentions > 0 {
                        parts::badge(ui, egui::pos2(rect.right() - 16.0, rect.center().y), mentions);
                    }
                });
                if response.clicked() && c.kind != ChannelKind::Voice {
                    chat_ui.select_channel(c.id);
                }
            }
        });
    });
}

fn dm_side(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi) {
    let Some(snap) = &state.snap else { return };
    header(ui, t().chat.dms, |ui| {
        if parts::glyph_button(ui, Glyph::Plus, t().chat.start_dm, false).clicked() {
            chat.ask("friends", Method::GET, "/api/v2/friends".into(), None);
            chat_ui.dialog = Some(Dialog::StartDm);
        }
    });
    egui::ScrollArea::vertical().id_salt("chat-dms").show(ui, |ui| {
        ui.add_space(6.0);
        egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 0)).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for inv in &snap.group_invites {
                parts::card_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(parts::muted((t().chat.invited_by)(&inv.by.display_name())));
                    ui.horizontal(|ui| {
                        parts::group_icon(ui, chat, inv.group.id, &inv.group.name, &inv.group.icon, 28.0, false);
                        ui.label(RichText::new(&inv.group.name).strong());
                    });
                    ui.horizontal(|ui| {
                        if ui.add(crate::ui::controls::primary(t().chat.accept)).clicked() {
                            chat.ask("join", Method::POST, format!("/api/v2/chat/group-invites/{}/accept", inv.id), None);
                        }
                        if ui.button(t().chat.decline).clicked() {
                            chat.fire(Method::POST, format!("/api/v2/chat/group-invites/{}/reject", inv.id), None);
                        }
                    });
                });
                ui.add_space(4.0);
            }
            let mut dms: Vec<_> = snap.dms.iter().collect();
            dms.sort_by_key(|d| std::cmp::Reverse(d.channel.last_message_id.unwrap_or(d.channel.id)));
            for d in dms {
                let (unread, mentions) = state.unread(&d.channel);
                let active = chat_ui.channel == Some(d.channel.id);
                let status = state.presences.get(&d.user.user_id).copied();
                let response = {
                    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
                    if active {
                        ui.painter().rect_filled(rect, 5.0, SELECTED);
                    } else if response.hovered() {
                        ui.painter().rect_filled(rect, 5.0, HOVER);
                    }
                    let avatar = Rect::from_center_size(egui::pos2(rect.left() + 20.0, rect.center().y), vec2(30.0, 30.0));
                    parts::paint_avatar(ui, chat, avatar, &d.user, status);
                    let color = if active || unread { TEXT } else { Color32::from_rgb(140, 150, 166) };
                    let galley = ui.painter().layout(parts::display_name(&d.user, ""), egui::FontId::proportional(14.0), color, rect.width() - 80.0);
                    ui.painter().galley(egui::pos2(rect.left() + 42.0, rect.center().y - galley.size().y / 2.0), galley, color);
                    if unread {
                        parts::badge(ui, egui::pos2(rect.right() - 16.0, rect.center().y), mentions.max(1));
                    }
                    response
                };
                if response.clicked() {
                    chat_ui.select_channel(d.channel.id);
                }
            }
        });
    });
    if let Some(crate::features::chat::state::Answer::Done(v)) = state.answers.get("join")
        && let Some(id) = v["joined"]["group"]["id"].as_str().and_then(|s| s.parse().ok())
    {
        chat_ui.select_group(Some(Id(id)), state);
        chat.forget("join");
    }
}

fn user_bar(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi) {
    let Some(snap) = &state.snap else { return };
    let status = snap.status;
    parts::avatar(ui, chat, &snap.me, 32.0, Some(if status == farwing_protocol::chat::Status::Invisible { farwing_protocol::chat::Status::Offline } else { status }));
    ui.add_space(6.0);
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.add(egui::Label::new(RichText::new(snap.me.display_name()).color(TEXT)).truncate());
        ui.label(parts::muted(parts::status_text(status)));
    });
    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
        if parts::glyph_button(ui, Glyph::Gear, t().chat.settings, false).clicked() {
            chat_ui.dialog = Some(Dialog::Me);
        }
    });
}
