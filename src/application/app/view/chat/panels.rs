//! Beside the messages: a group's members, the pinned messages, search; the
//! card a click on a name opens; a channel's notification setting.
use super::parts::{self, Glyph};
use super::{ChatUi, Panel};
use crate::features::chat::ChatService;
use crate::features::chat::state::{Answer, ChatState};
use crate::i18n::t;
use crate::ui::theme::{HOVER, LINE, MUTED, TEXT};
use egui::{Rect, RichText, Sense, Stroke, vec2};
#[cfg(windows)]
use farwing_protocol::chat::Id;
use farwing_protocol::chat::{Channel, GroupState, Member, Message, Notify, Status};
use reqwest::Method;

pub(crate) fn bytes(n: u64) -> String {
    let n = n as f64;
    if n < 1024.0 {
        format!("{n} B")
    } else if n < 1024.0 * 1024.0 {
        format!("{:.1} KB", n / 1024.0)
    } else if n < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB", n / 1024.0 / 1024.0)
    } else {
        format!("{:.1} GB", n / 1024.0 / 1024.0 / 1024.0)
    }
}

fn panel_header(ui: &mut egui::Ui, glyph: Glyph, title: &str) -> bool {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 48.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, LINE));
    let mut close = false;
    ui.scope_builder(
        egui::UiBuilder::new().max_rect(rect.shrink2(vec2(12.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)),
        |ui| {
            let (r, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
            parts::paint_glyph(ui.painter(), r, glyph, MUTED);
            ui.add_space(6.0);
            ui.label(RichText::new(title).strong().color(TEXT));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = parts::glyph_button(ui, Glyph::Cross, t().common.close, false).clicked();
            });
        },
    );
    close
}

pub(super) fn members(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi, compact: bool) {
    if compact && panel_header(ui, Glyph::People, t().chat.members) {
        chat_ui.members = false;
        return;
    }
    let Some(g) = chat_ui.group.and_then(|g| state.group(g)) else { return };
    let Some(list) = state.members.get(&g.group.id) else {
        chat.load_members(g.group.id);
        ui.add_space(20.0);
        ui.vertical_centered(|ui| ui.add(egui::Spinner::new()));
        return;
    };
    let online = |m: &Member| matches!(state.presences.get(&m.user.user_id), Some(Status::Online | Status::Idle | Status::Dnd));
    let mut roles: Vec<_> = g.roles.iter().filter(|r| r.id != g.group.id).collect();
    roles.sort_by_key(|r| std::cmp::Reverse(r.position));
    let mut placed = std::collections::HashSet::new();
    let mut sections: Vec<(String, Vec<&Member>)> = Vec::new();
    for r in roles {
        let ms: Vec<&Member> = list.iter().filter(|m| online(m) && m.roles.contains(&r.id) && !placed.contains(&m.user.user_id)).collect();
        for m in &ms {
            placed.insert(m.user.user_id.clone());
        }
        if !ms.is_empty() {
            sections.push((r.name.clone(), ms));
        }
    }
    let rest: Vec<&Member> = list.iter().filter(|m| online(m) && !placed.contains(&m.user.user_id)).collect();
    if !rest.is_empty() {
        sections.push((t().chat.online.into(), rest));
    }
    let offline: Vec<&Member> = list.iter().filter(|m| !online(m)).collect();
    if !offline.is_empty() {
        sections.push((t().chat.status_offline.into(), offline));
    }
    egui::ScrollArea::vertical().id_salt(("chat-members", g.group.id)).show(ui, |ui| {
        egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 8)).show(ui, |ui| {
            for (title, mut ms) in sections {
                ms.sort_by_key(|m| parts::display_name(&m.user, &m.nickname).to_lowercase());
                ui.add_space(8.0);
                ui.label(parts::muted(format!("{title} — {}", ms.len())));
                ui.add_space(4.0);
                for m in ms {
                    let status = state.presences.get(&m.user.user_id).copied();
                    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
                    if response.hovered() {
                        ui.painter().rect_filled(rect, 5.0, HOVER);
                    }
                    let fade = !online(m);
                    let a = Rect::from_center_size(egui::pos2(rect.left() + 20.0, rect.center().y), vec2(30.0, 30.0));
                    parts::paint_avatar(ui, chat, a, &m.user, status);
                    let color = g
                        .roles
                        .iter()
                        .filter(|r| m.roles.contains(&r.id) && r.color != 0)
                        .max_by_key(|r| r.position)
                        .map_or(TEXT, |r| parts::rgb(r.color));
                    let color = if fade { color.gamma_multiply(0.45) } else { color };
                    let mut name = parts::display_name(&m.user, &m.nickname);
                    if g.group.owner_id == m.user.user_id {
                        name.push_str(" ♛");
                    }
                    let galley = ui.painter().layout(name, egui::FontId::proportional(14.0), color, rect.width() - 50.0);
                    ui.painter().galley(egui::pos2(rect.left() + 42.0, rect.center().y - galley.size().y / 2.0), galley, color);
                    if response.clicked() {
                        chat_ui.card = Some((m.user.user_id.clone(), rect.left_top()));
                    }
                }
            }
        });
    });
}

fn result_card(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi, m: &Message, group: Option<&GroupState>) {
    parts::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            parts::avatar(ui, chat, &m.author, 22.0, None);
            ui.label(RichText::new(parts::display_name(&m.author, "")).strong().color(TEXT));
            ui.label(parts::muted(parts::time_text(m.created_at)));
            if let Some((c, _, d)) = state.channel(m.channel_id) {
                let where_ = match d {
                    Some(d) => parts::display_name(&d.user, ""),
                    None => format!("#{}", c.name),
                };
                ui.label(parts::muted(where_));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.link(t().chat.jump).clicked() {
                    if let Some((_, g, _)) = state.channel(m.channel_id) {
                        chat_ui.group = g.map(|g| g.group.id);
                        chat_ui.select_channel(m.channel_id);
                    }
                    chat.jump(m.channel_id, m.id);
                }
            });
        });
        if !m.content.is_empty() {
            parts::rich(ui, chat, state, group, &m.content, m.id, &mut chat_ui.spoilers);
        }
        if !m.attachments.is_empty() {
            ui.label(parts::muted((t().chat.attachment_count)(m.attachments.len())));
        }
    });
    ui.add_space(6.0);
}

fn messages_of(state: &ChatState, key: &str) -> Option<Result<(Vec<Message>, bool), String>> {
    match state.answers.get(key)? {
        Answer::Waiting => None,
        Answer::Failed(e) => Some(Err(e.clone())),
        Answer::Done(v) => Some(Ok((
            serde_json::from_value(v["messages"].clone()).unwrap_or_default(),
            v["more"].as_bool().unwrap_or(false),
        ))),
    }
}

pub(super) fn pins(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi) {
    let Some(channel) = chat_ui.channel else { return };
    let key = format!("pins:{channel}");
    if panel_header(ui, Glyph::Pin, t().chat.pins) {
        chat_ui.panel = None;
        return;
    }
    if !state.answers.contains_key(&key) {
        chat.ask(&key, Method::GET, format!("/api/v2/chat/channels/{channel}/pins"), None);
    }
    let group = state.channel(channel).and_then(|(_, g, _)| g.cloned());
    egui::ScrollArea::vertical().id_salt(("chat-pins", channel)).show(ui, |ui| {
        egui::Frame::new().inner_margin(egui::Margin::same(10)).show(ui, |ui| match messages_of(state, &key) {
            None => {
                ui.add(egui::Spinner::new());
            }
            Some(Err(e)) => {
                ui.label(parts::muted(e));
            }
            Some(Ok((list, _))) if list.is_empty() => {
                ui.label(parts::muted(t().chat.no_pins));
            }
            Some(Ok((list, _))) => {
                for m in &list {
                    result_card(ui, chat, state, chat_ui, m, group.as_ref());
                }
            }
        });
    });
    // Pins change by pushes of messages: read again now and then.
    if ui.ctx().input(|i| i.time) as u64 % 15 == 0 {
        chat.forget(&key);
    }
}

pub(super) fn search(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi) {
    let Some(channel) = chat_ui.channel else { return };
    if panel_header(ui, Glyph::Search, t().chat.search) {
        chat_ui.panel = None;
        return;
    }
    let group = state.channel(channel).and_then(|(_, g, _)| g.cloned());
    let c = &t().chat;
    let mut run = false;
    egui::Frame::new().inner_margin(egui::Margin::same(10)).show(ui, |ui| {
        let response = ui.add(
            crate::ui::controls::singleline(&mut chat_ui.search_query, 30.0)
                .desired_width(ui.available_width())
                .hint_text(c.search_placeholder),
        );
        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            run = true;
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let scopes: Vec<(u8, &str)> = if group.is_some() {
                vec![(0, c.scope_channel), (1, c.scope_group), (2, c.scope_all)]
            } else {
                vec![(0, c.scope_channel), (2, c.scope_all)]
            };
            for (v, label) in scopes {
                if ui.selectable_label(chat_ui.search_scope == v, label).clicked() {
                    chat_ui.search_scope = v;
                }
            }
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.checkbox(&mut chat_ui.search_files, c.with_files);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(t().chat.search).clicked() {
                    run = true;
                }
            });
        });
    });
    ui.painter().hline(ui.max_rect().x_range(), ui.cursor().min.y, Stroke::new(1.0, LINE));
    if run && (!chat_ui.search_query.trim().is_empty() || chat_ui.search_files) {
        let mut q = format!("q={}", crate::features::chat::service::urlencode(chat_ui.search_query.trim()));
        if chat_ui.search_files {
            q.push_str("&has=attachment");
        }
        match chat_ui.search_scope {
            0 => q.push_str(&format!("&channel={channel}")),
            1 => {
                if let Some(g) = &group {
                    q.push_str(&format!("&group={}", g.group.id));
                }
            }
            _ => {}
        }
        chat.ask("search", Method::GET, format!("/api/v2/chat/search?{q}"), None);
        chat.forget("search_more");
        chat_ui.search_more.clear();
        chat_ui.search_more_left = false;
        chat_ui.search_sent = q;
    }
    // A further page joins the list.
    if let Some(Ok((list, next))) = messages_of(state, "search_more") {
        chat_ui.search_more.extend(list);
        chat_ui.search_more_left = next;
        chat.forget("search_more");
    }
    let more = if chat_ui.search_more.is_empty() {
        messages_of(state, "search").and_then(Result::ok).is_some_and(|(_, more)| more)
    } else {
        chat_ui.search_more_left
    };
    egui::ScrollArea::vertical().id_salt("chat-search-results").show(ui, |ui| {
        egui::Frame::new().inner_margin(egui::Margin::same(10)).show(ui, |ui| match messages_of(state, "search") {
            None if state.answers.contains_key("search") => {
                ui.add(egui::Spinner::new());
            }
            None => {}
            Some(Err(e)) => {
                ui.label(parts::muted(e));
            }
            Some(Ok((list, _))) if list.is_empty() => {
                ui.label(parts::muted(c.no_results));
            }
            Some(Ok((list, _))) => {
                let extra = chat_ui.search_more.clone();
                for m in list.iter().chain(&extra) {
                    let g = state.channel(m.channel_id).and_then(|(_, g, _)| g.cloned());
                    result_card(ui, chat, state, chat_ui, m, g.as_ref());
                }
                if more
                    && !matches!(state.answers.get("search_more"), Some(Answer::Waiting))
                    && let Some(last) = list.iter().chain(&extra).last()
                    && ui.button(c.more_results).clicked()
                {
                    let q = format!("{}&before={}", chat_ui.search_sent, last.id);
                    chat.ask("search_more", Method::GET, format!("/api/v2/chat/search?{q}"), None);
                }
            }
        });
    });
    let _ = Panel::Search;
}

pub(super) fn user_card(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi) {
    let Some((user, at)) = chat_ui.card.clone() else { return };
    let group = chat_ui.group.and_then(|g| state.group(g));
    let member = group.and_then(|g| state.members.get(&g.group.id)).and_then(|l| l.iter().find(|m| m.user.user_id == user)).cloned();
    let snap = state.snap.as_ref();
    let known = member
        .as_ref()
        .map(|m| m.user.clone())
        .or_else(|| snap.and_then(|s| s.dms.iter().find(|d| d.user.user_id == user).map(|d| d.user.clone())))
        .or_else(|| snap.filter(|s| s.me.user_id == user).map(|s| s.me.clone()));
    let status = state.presences.get(&user).copied().unwrap_or(Status::Offline);
    let screen = ui.ctx().content_rect();
    let pos = egui::pos2(at.x.min(screen.right() - 300.0), at.y.min(screen.bottom() - 220.0));
    let area = egui::Area::new(ui.id().with("chat-card"))
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .show(ui.ctx(), |ui| {
            crate::ui::controls::dialog_frame().inner_margin(egui::Margin::same(14)).show(ui, |ui| {
                ui.set_width(260.0);
                ui.horizontal(|ui| {
                    if let Some(u) = &known {
                        parts::avatar(ui, chat, u, 54.0, Some(status));
                    }
                    ui.vertical(|ui| {
                        let name = known.as_ref().map_or_else(|| t().chat.unknown_user.to_owned(), |u| parts::display_name(u, member.as_ref().map_or("", |m| m.nickname.as_str())));
                        ui.label(RichText::new(name).size(15.0).strong().color(TEXT));
                        if let Some(u) = &known {
                            ui.label(parts::muted(&u.username));
                        }
                        ui.label(parts::muted(parts::status_text(status)));
                    });
                });
                if let (Some(m), Some(g)) = (&member, group) {
                    ui.horizontal_wrapped(|ui| {
                        let mut roles: Vec<_> = g.roles.iter().filter(|r| m.roles.contains(&r.id)).collect();
                        roles.sort_by_key(|r| std::cmp::Reverse(r.position));
                        for r in roles {
                            let color = if r.color != 0 { parts::rgb(r.color) } else { MUTED };
                            ui.label(RichText::new(format!("● {}", r.name)).size(12.0).color(color));
                        }
                    });
                }
                let me = snap.is_some_and(|s| s.me.user_id == user);
                if !me && known.as_ref().is_some_and(|u| !u.gone) {
                    ui.add_space(6.0);
                    if ui.button(t().chat.send_dm).clicked() {
                        chat.ask("open_dm", Method::POST, "/api/v2/chat/dm".into(), Some(serde_json::json!({"user": user})));
                    }
                }
            });
        });
    if let Some(Answer::Done(v)) = state.answers.get("open_dm")
        && let Some(id) = v["channel"]["id"].as_str().and_then(|s| s.parse().ok())
    {
        chat_ui.group = None;
        chat_ui.select_channel(farwing_protocol::chat::Id(id));
        chat_ui.card = None;
        chat.forget("open_dm");
        return;
    }
    if ui.input(|i| i.pointer.any_pressed())
        && let Some(p) = ui.input(|i| i.pointer.interact_pos())
        && !area.response.rect.contains(p)
    {
        chat_ui.card = None;
    }
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        chat_ui.card = None;
    }
}

#[cfg(windows)]
/// A voice participant's card: their volume here, and a moderator's mute
/// and disconnect.
pub(super) fn voice_card(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi) {
    let Some((channel, participant, at)) = chat_ui.voice_card else { return };
    let Some(p) = state
        .snap
        .as_ref()
        .and_then(|s| s.voice.iter().find(|v| v.channel_id == channel))
        .and_then(|v| v.participants.iter().find(|p| p.participant == participant))
        .cloned()
    else {
        chat_ui.voice_card = None;
        return;
    };
    let v = &t().voice;
    let me = state.me().is_some_and(|u| u.user_id == p.user_id);
    let moderate = state
        .channel(channel)
        .is_some_and(|(c, _, _)| c.permissions & farwing_protocol::chat::perm::VOICE_MODERATE != 0);
    let area = egui::Area::new(ui.id().with("chat-voice-card"))
        .order(egui::Order::Foreground)
        .fixed_pos(at + vec2(8.0, 0.0))
        .show(ui.ctx(), |ui| {
            parts::card_frame().show(ui, |ui| {
                ui.set_width(220.0);
                let name = state
                    .members
                    .values()
                    .flat_map(|m| m.iter())
                    .find(|m| m.user.user_id == p.user_id)
                    .map(|m| parts::display_name(&m.user, &m.nickname))
                    .or_else(|| state.me().filter(|u| u.user_id == p.user_id).map(|u| u.display_name().to_owned()))
                    .unwrap_or_default();
                ui.label(RichText::new(name).strong().color(TEXT));
                if !me {
                    let mut settings = chat.voice_settings();
                    let mut volume = u32::from(*settings.volumes.get(&p.user_id).unwrap_or(&100));
                    ui.label(parts::muted(v.user_volume));
                    if ui.add(egui::Slider::new(&mut volume, 0..=200).suffix("%")).changed() {
                        settings.volumes.insert(p.user_id.clone(), volume as u8);
                        chat.set_voice_settings(settings.clone());
                    }
                    let muted = settings.muted.contains(&p.user_id);
                    if ui.button(if muted { v.unmute_user } else { v.mute_user }).clicked() {
                        if muted {
                            settings.muted.remove(&p.user_id);
                        } else {
                            settings.muted.insert(p.user_id.clone());
                        }
                        chat.set_voice_settings(settings);
                    }
                }
                if moderate {
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        let label = if p.server_muted { v.server_unmute } else { v.server_mute };
                        if ui.button(label).clicked() {
                            chat.moderate_voice(channel, participant, serde_json::json!({"server_muted": !p.server_muted}));
                        }
                        let label = if p.server_deafened { v.server_undeafen } else { v.server_deafen };
                        if ui.button(label).clicked() {
                            chat.moderate_voice(channel, participant, serde_json::json!({"server_deafened": !p.server_deafened}));
                        }
                        if !me && ui.button(RichText::new(v.kick).color(crate::ui::theme::RED)).clicked() {
                            chat.disconnect_voice(channel, participant);
                            chat_ui.voice_card = None;
                        }
                    });
                    // Another voice channel of the group.
                    let others: Vec<(Id, String)> = state
                        .channel(channel)
                        .and_then(|(_, g, _)| g)
                        .map(|g| {
                            g.channels
                                .iter()
                                .filter(|c| c.kind == farwing_protocol::chat::ChannelKind::Voice && c.id != channel)
                                .map(|c| (c.id, c.name.clone()))
                                .collect()
                        })
                        .unwrap_or_default();
                    if !others.is_empty() {
                        ui.menu_button(v.move_to, |ui| {
                            for (id, name) in others {
                                if ui.button(name).clicked() {
                                    chat.moderate_voice(channel, participant, serde_json::json!({"move_to": id}));
                                    chat_ui.voice_card = None;
                                    ui.close();
                                }
                            }
                        });
                    }
                }
            });
        });
    if ui.input(|i| i.pointer.any_pressed())
        && let Some(pos) = ui.input(|i| i.pointer.interact_pos())
        && !area.response.rect.contains(pos)
        && pos.distance(at) > 4.0
    {
        chat_ui.voice_card = None;
    }
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        chat_ui.voice_card = None;
    }
}

pub(super) fn notify_menu(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, channel: &Channel, group: Option<&GroupState>) {
    let Some(prefs) = state.snap.as_ref().map(|s| s.prefs.clone()) else { return };
    let key = format!("c:{}", channel.id);
    let current = prefs.notify.get(&key).copied();
    let c = &t().chat;
    let mut options: Vec<(Option<Notify>, &str)> = Vec::new();
    if group.is_some() {
        options.push((None, c.notify_inherit));
        options.push((Some(Notify::All), c.notify_all));
        options.push((Some(Notify::Mentions), c.notify_mentions));
    } else {
        options.push((None, c.notify_all));
    }
    options.push((Some(Notify::None), c.notify_none));
    ui.set_min_width(160.0);
    for (value, label) in options {
        if ui.selectable_label(current == value, label).clicked() {
            let mut next = prefs.clone();
            match value {
                Some(v) => {
                    next.notify.insert(key.clone(), v);
                }
                None => {
                    next.notify.remove(&key);
                }
            }
            chat.save_prefs(next);
            ui.close();
        }
    }
}
