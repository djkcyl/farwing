//! A group's settings (`docs/protocol/chat-design.md` §3, §5): overview,
//! roles, channels with overwrites, members, invitations, bans, join
//! requests and the audit log, each tab for those whose permissions allow it.
use super::parts;
use super::{ChatUi, Confirmed, Dialog};
use crate::features::chat::ChatService;
use crate::features::chat::state::{Answer, ChatState, ordered};
use crate::i18n::t;
use crate::ui::controls::{DialogIcon, dialog_frame, dialog_header};
use crate::ui::theme::{HOVER, MUTED, SELECTED, TEXT};
use egui::{RichText, Sense, vec2};
use farwing_protocol::chat::{Channel, ChannelKind, GroupState, Id, Notify, Role, perm};
use reqwest::Method;
use serde_json::json;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Tab {
    #[default]
    Overview,
    Roles,
    Channels,
    Members,
    Invites,
    Bans,
    Requests,
    Audit,
}

#[derive(Default)]
pub(super) struct SettingsUi {
    group: Option<Id>,
    tab: Tab,
    name: String,
    description: String,
    nickname: String,
    role: Option<Id>,
    role_draft: Option<Role>,
    channel: Option<Id>,
    channel_draft: Option<Channel>,
    overwrite_role: Option<Id>,
    new_channel: Option<(ChannelKind, String, Option<Id>)>,
    member_query: String,
    ban: Option<(String, String, String, u32)>,
    transfer_to: Option<String>,
    pub(super) avatar_pick: Option<std::sync::mpsc::Receiver<Vec<std::path::PathBuf>>>,
    icon_pick: Option<std::sync::mpsc::Receiver<Vec<std::path::PathBuf>>>,
}

impl SettingsUi {
    pub(super) fn open(&mut self, g: &GroupState) {
        *self = Self {
            avatar_pick: self.avatar_pick.take(),
            group: Some(g.group.id),
            name: g.group.name.clone(),
            description: g.group.description.clone(),
            nickname: g.me.nickname.clone(),
            ..Default::default()
        };
    }
}

const COLORS: [u32; 10] = [0, 0xE67E22, 0xE74C3C, 0xE91E63, 0x9B59B6, 0x3498DB, 0x1ABC9C, 0x2ECC71, 0xF1C40F, 0x95A5A6];
const ALL_PERMS: [i64; 19] = [
    perm::VIEW,
    perm::SEND,
    perm::ATTACH,
    perm::MENTION_ALL,
    perm::MANAGE_MESSAGES,
    perm::READ_HISTORY,
    perm::CONNECT,
    perm::SPEAK,
    perm::VIDEO,
    perm::VOICE_MODERATE,
    perm::PRIORITY,
    perm::INVITE,
    perm::KICK,
    perm::BAN,
    perm::MUTE,
    perm::MANAGE_CHANNELS,
    perm::MANAGE_ROLES,
    perm::MANAGE_GROUP,
    perm::ADMINISTRATOR,
];

fn perm_label(bit: i64) -> &'static str {
    let i = ALL_PERMS.iter().position(|b| *b == bit).unwrap_or(0);
    t().chat.perms[i]
}

/// What a list row starts with: a role's colour or a channel's kind.
enum Mark {
    Color(egui::Color32),
    Glyph(parts::Glyph),
    Nothing,
}

fn list_item(ui: &mut egui::Ui, selected: bool, text: &str, mark: Mark, indent: f32) -> bool {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::click());
    if selected {
        ui.painter().rect_filled(rect, 5.0, SELECTED);
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 5.0, HOVER);
    }
    let left = rect.left_center() + vec2(indent, 0.0);
    match mark {
        Mark::Color(c) => {
            ui.painter().circle_filled(left + vec2(10.0, 0.0), 5.0, c);
        }
        Mark::Glyph(g) => parts::paint_glyph(ui.painter(), egui::Rect::from_center_size(left + vec2(10.0, 0.0), vec2(16.0, 16.0)), g, MUTED),
        Mark::Nothing => {}
    }
    ui.painter().text(
        left + vec2(22.0, 0.0),
        egui::Align2::LEFT_CENTER,
        text,
        egui::FontId::proportional(13.5),
        if selected { TEXT } else { MUTED },
    );
    response.clicked()
}

pub(super) fn show(ctx: &egui::Context, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi) {
    if chat_ui.dialog != Some(Dialog::GroupSettings) {
        return;
    }
    let Some(g) = chat_ui.settings.group.and_then(|g| state.group(g)).cloned() else {
        chat_ui.dialog = None;
        return;
    };
    let p = g.group.permissions;
    let c = &t().chat;
    let mut tabs = vec![(Tab::Overview, c.tab_overview)];
    if p & perm::MANAGE_ROLES != 0 {
        tabs.push((Tab::Roles, c.tab_roles));
    }
    if p & perm::MANAGE_CHANNELS != 0 {
        tabs.push((Tab::Channels, c.tab_channels));
    }
    if p & (perm::KICK | perm::BAN | perm::MUTE | perm::MANAGE_ROLES) != 0 {
        tabs.push((Tab::Members, c.tab_members));
    }
    if p & perm::INVITE != 0 {
        tabs.push((Tab::Invites, c.tab_invites));
    }
    if p & perm::BAN != 0 {
        tabs.push((Tab::Bans, c.tab_bans));
    }
    if p & perm::MANAGE_GROUP != 0 {
        tabs.push((Tab::Requests, c.tab_requests));
        tabs.push((Tab::Audit, c.tab_audit));
    }
    let mut close = false;
    let response = egui::Modal::new(egui::Id::new("chat-group-settings")).frame(dialog_frame()).show(ctx, |ui| {
        ui.set_width(720.0);
        close = dialog_header(ui, &format!("{} · {}", c.group_settings, g.group.name), DialogIcon::Edit, true);
        ui.horizontal(|ui| {
            for (tab, label) in &tabs {
                if ui.selectable_label(chat_ui.settings.tab == *tab, *label).clicked() && chat_ui.settings.tab != *tab {
                    chat_ui.settings.tab = *tab;
                    match tab {
                        Tab::Invites => chat.ask("s-invites", Method::GET, format!("/api/v2/chat/groups/{}/invites", g.group.id), None),
                        Tab::Bans => chat.ask("s-bans", Method::GET, format!("/api/v2/chat/groups/{}/bans", g.group.id), None),
                        Tab::Requests => chat.ask("s-requests", Method::GET, format!("/api/v2/chat/groups/{}/requests", g.group.id), None),
                        Tab::Audit => chat.ask("s-audit", Method::GET, format!("/api/v2/chat/groups/{}/audit", g.group.id), None),
                        Tab::Members | Tab::Overview => chat.load_members(g.group.id),
                        _ => {}
                    }
                }
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().max_height(520.0).min_scrolled_height(520.0).auto_shrink([false, false]).id_salt("chat-settings-body").show(ui, |ui| {
            ui.set_width(ui.available_width());
            match chat_ui.settings.tab {
                Tab::Overview => overview(ui, chat, state, chat_ui, &g),
                Tab::Roles => roles(ui, chat, chat_ui, &g),
                Tab::Channels => channels(ui, chat, state, chat_ui, &g),
                Tab::Members => members(ui, chat, state, chat_ui, &g),
                Tab::Invites => invites(ui, chat, state, &g),
                Tab::Bans => bans(ui, chat, state, &g),
                Tab::Requests => requests(ui, chat, state, &g),
                Tab::Audit => audit(ui, state),
            }
        });
    });
    if close || response.should_close() {
        chat_ui.dialog = None;
    }
    if let Some(rx) = &chat_ui.settings.icon_pick
        && let Ok(paths) = rx.try_recv()
    {
        chat_ui.settings.icon_pick = None;
        if let Some(path) = paths.into_iter().next()
            && let Ok(bytes) = std::fs::read(path)
        {
            chat.upload_square(format!("/api/v2/chat/groups/{}/icon", g.group.id), bytes);
        }
    }
}

fn overview(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi, g: &GroupState) {
    let c = &t().chat;
    let id = g.group.id;
    let manage = g.group.permissions & perm::MANAGE_GROUP != 0;
    let owner = state.me().is_some_and(|m| m.user_id == g.group.owner_id);
    let s = &mut chat_ui.settings;
    ui.horizontal(|ui| {
        parts::group_icon(ui, chat, id, &g.group.name, &g.group.icon, 60.0, true);
        if manage {
            ui.vertical(|ui| {
                if ui.button(c.change_icon).clicked() && s.icon_pick.is_none() {
                    s.icon_pick = Some(crate::platform::windows::file_dialog::pick_files(c.change_icon.into()));
                }
                if !g.group.icon.is_empty() && ui.button(c.remove_icon).clicked() {
                    chat.fire(Method::DELETE, format!("/api/v2/chat/groups/{id}/icon"), None);
                }
            });
        }
    });
    ui.add_space(6.0);
    let field = |ui: &mut egui::Ui, label: &str, value: &mut String, enabled: bool| {
        ui.label(parts::muted(label));
        ui.add_enabled(enabled, crate::ui::controls::singleline(value, crate::ui::controls::HEIGHT).desired_width(f32::INFINITY));
    };
    field(ui, c.name, &mut s.name, manage);
    field(ui, c.description, &mut s.description, manage);
    field(ui, c.nickname, &mut s.nickname, true);
    // One save for the texts: the group's and my nickname in it.
    let group_changed = manage && (s.name.trim() != g.group.name || s.description.trim() != g.group.description);
    let nickname_changed = s.nickname.trim() != g.me.nickname;
    ui.add_space(4.0);
    if ui
        .add_enabled((group_changed || nickname_changed) && !s.name.trim().is_empty(), crate::ui::controls::primary(c.save))
        .clicked()
    {
        if group_changed {
            chat.fire(
                Method::PATCH,
                format!("/api/v2/chat/groups/{id}"),
                Some(json!({"name": s.name.trim(), "description": s.description.trim()})),
            );
        }
        if nickname_changed {
            chat.fire(Method::PATCH, format!("/api/v2/chat/groups/{id}/members/@me"), Some(json!({"nickname": s.nickname.trim()})));
        }
    }
    ui.add_space(10.0);
    if manage {
        let mut public = g.group.public;
        let mut approval = g.group.join_approval;
        ui.horizontal(|ui| {
            if crate::ui::controls::switch(ui, &mut public).changed() {
                chat.fire(Method::PATCH, format!("/api/v2/chat/groups/{id}"), Some(json!({"public": public})));
            }
            ui.label(c.public);
        });
        ui.horizontal(|ui| {
            if crate::ui::controls::switch(ui, &mut approval).changed() {
                chat.fire(Method::PATCH, format!("/api/v2/chat/groups/{id}"), Some(json!({"join_approval": approval})));
            }
            ui.label(c.approval);
        });
    }
    ui.horizontal(|ui| {
        ui.label(parts::muted(c.notify));
        let key = format!("g:{id}");
        let current = state.snap.as_ref().and_then(|s| s.prefs.notify.get(&key).copied()).unwrap_or(Notify::Mentions);
        for (v, label) in [(Notify::All, c.notify_all), (Notify::Mentions, c.notify_mentions), (Notify::None, c.notify_none)] {
            if ui.selectable_label(current == v, label).clicked()
                && let Some(snap) = &state.snap
            {
                let mut prefs = snap.prefs.clone();
                prefs.notify.insert(key.clone(), v);
                chat.save_prefs(prefs);
            }
        }
    });
    ui.add_space(10.0);
    ui.separator();
    if owner {
        if !state.members.contains_key(&id) {
            chat.load_members(id);
        }
        ui.horizontal(|ui| {
            let members = state.members.get(&id).cloned().unwrap_or_default();
            let chosen = s.transfer_to.clone();
            egui::ComboBox::from_id_salt("chat-transfer")
                .selected_text(
                    chosen
                        .as_ref()
                        .and_then(|u| members.iter().find(|m| &m.user.user_id == u))
                        .map_or(c.transfer_to.to_owned(), |m| parts::display_name(&m.user, &m.nickname)),
                )
                .show_ui(ui, |ui| {
                    for m in members.iter().filter(|m| m.user.user_id != g.group.owner_id) {
                        ui.selectable_value(&mut s.transfer_to, Some(m.user.user_id.clone()), parts::display_name(&m.user, &m.nickname));
                    }
                });
            if ui.add_enabled(s.transfer_to.is_some(), egui::Button::new(c.transfer)).clicked()
                && let Some(u) = s.transfer_to.take()
            {
                chat.ask("transfer", Method::POST, format!("/api/v2/chat/groups/{id}/transfer"), Some(json!({"user": u})));
            }
        });
        if ui.add(egui::Button::new(RichText::new(c.dissolve).color(crate::ui::theme::RED))).clicked() {
            chat_ui.dialog = Some(Dialog::Confirm {
                title: (c.dissolve_question)(&g.group.name),
                action: Confirmed::Dissolve(id),
            });
        }
    } else if ui.add(egui::Button::new(RichText::new(c.leave).color(crate::ui::theme::RED))).clicked() {
        chat_ui.dialog = Some(Dialog::Confirm {
            title: (c.leave_question)(&g.group.name),
            action: Confirmed::Leave(id),
        });
    }
}

fn perm_checks(ui: &mut egui::Ui, value: &mut i64, bits: &[i64]) {
    egui::Grid::new(ui.id().with("perms")).num_columns(2).spacing(vec2(24.0, 4.0)).show(ui, |ui| {
        for (i, bit) in bits.iter().enumerate() {
            let mut on = *value & bit != 0;
            if ui.checkbox(&mut on, perm_label(*bit)).changed() {
                if on {
                    *value |= bit;
                } else {
                    *value &= !bit;
                }
            }
            if i % 2 == 1 {
                ui.end_row();
            }
        }
    });
}

fn roles(ui: &mut egui::Ui, chat: &ChatService, chat_ui: &mut ChatUi, g: &GroupState) {
    let c = &t().chat;
    let id = g.group.id;
    let mut sorted: Vec<&Role> = g.roles.iter().collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.position));
    let s = &mut chat_ui.settings;
    if s.role.is_none_or(|r| !g.roles.iter().any(|x| x.id == r)) {
        s.role = sorted.first().map(|r| r.id);
        s.role_draft = None;
    }
    if s.role_draft.as_ref().map(|d| d.id) != s.role {
        s.role_draft = s.role.and_then(|r| g.roles.iter().find(|x| x.id == r)).cloned();
    }
    let mut confirm = None;
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.set_width(200.0);
            if ui.button(c.new_role).clicked() {
                chat.fire(Method::POST, format!("/api/v2/chat/groups/{id}/roles"), Some(json!({"name": c.new_role, "permissions": 0})));
            }
            let others: Vec<Id> = sorted.iter().filter(|r| r.id != id).map(|r| r.id).collect();
            for r in &sorted {
                let label = if r.id == id { c.everyone.to_owned() } else { r.name.clone() };
                let color = if r.color != 0 { parts::rgb(r.color) } else { crate::ui::theme::DISABLED };
                ui.horizontal(|ui| {
                    if list_item(ui, s.role == Some(r.id), &label, Mark::Color(color), 0.0) {
                        s.role = Some(r.id);
                    }
                });
                if r.id != id && s.role == Some(r.id) {
                    ui.horizontal(|ui| {
                        let i = others.iter().position(|x| *x == r.id).unwrap_or(0);
                        let mut order: Vec<Id> = others.iter().rev().copied().collect();
                        let j = order.len() - 1 - i;
                        if ui.small_button(c.move_up).clicked() && j + 1 < order.len() {
                            order.swap(j, j + 1);
                            chat.fire(Method::PUT, format!("/api/v2/chat/groups/{id}/roles/order"), Some(json!({"roles": order})));
                        } else if ui.small_button(c.move_down).clicked() && j > 0 {
                            order.swap(j, j - 1);
                            chat.fire(Method::PUT, format!("/api/v2/chat/groups/{id}/roles/order"), Some(json!({"roles": order})));
                        }
                    });
                }
            }
        });
        ui.separator();
        ui.vertical(|ui| {
            let Some(draft) = &mut s.role_draft else { return };
            let everyone = draft.id == id;
            if !everyone {
                ui.label(parts::muted(c.role_name));
                ui.add(crate::ui::controls::singleline(&mut draft.name, crate::ui::controls::HEIGHT).desired_width(260.0));
                ui.label(parts::muted(c.color));
                ui.horizontal(|ui| {
                    for col in COLORS {
                        let (r, response) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
                        ui.painter().circle_filled(r.center(), 10.0, if col == 0 { crate::ui::theme::SURFACE } else { parts::rgb(col) });
                        if draft.color == col {
                            ui.painter().circle_stroke(r.center(), 11.0, egui::Stroke::new(2.0, TEXT));
                        }
                        if response.clicked() {
                            draft.color = col;
                        }
                    }
                });
                ui.horizontal(|ui| {
                    crate::ui::controls::switch(ui, &mut draft.mentionable);
                    ui.label(c.mentionable);
                });
            }
            ui.label(parts::muted(c.permissions));
            perm_checks(ui, &mut draft.permissions, &ALL_PERMS);
            ui.horizontal(|ui| {
                if ui.add(crate::ui::controls::primary(c.save)).clicked() {
                    chat.fire(
                        Method::PATCH,
                        format!("/api/v2/chat/groups/{id}/roles/{}", draft.id),
                        Some(json!({
                            "name": (!everyone).then(|| draft.name.clone()),
                            "color": draft.color,
                            "mentionable": draft.mentionable,
                            "permissions": draft.permissions,
                        })),
                    );
                }
                if !everyone && ui.button(RichText::new(c.delete_role).color(crate::ui::theme::RED)).clicked() {
                    confirm = Some((draft.id, draft.name.clone()));
                }
            });
        });
    });
    if let Some((role, _)) = confirm {
        chat_ui.dialog = Some(Dialog::Confirm {
            title: c.delete_role.into(),
            action: Confirmed::DeleteRole(id, role),
        });
    }
}

const SLOW: [u32; 10] = [0, 5, 10, 30, 60, 300, 600, 1800, 3600, 21600];

fn channels(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi, g: &GroupState) {
    let c = &t().chat;
    let id = g.group.id;
    let list = ordered(&g.channels);
    let categories: Vec<&Channel> = g.channels.iter().filter(|ch| ch.kind == ChannelKind::Category).collect();
    let s = &mut chat_ui.settings;
    if s.channel.is_none_or(|c| !g.channels.iter().any(|x| x.id == c)) {
        s.channel = list.iter().find(|c| c.kind != ChannelKind::Category).map(|c| c.id);
    }
    if s.channel_draft.as_ref().map(|d| d.id) != s.channel {
        s.channel_draft = s.channel.and_then(|ch| g.channels.iter().find(|x| x.id == ch)).cloned();
    }
    let mut confirm = None;
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.set_width(220.0);
            if ui.button(c.new_channel).clicked() {
                s.new_channel = Some((ChannelKind::Text, String::new(), categories.first().map(|ch| ch.id)));
            }
            let mut done = false;
            if let Some((kind, name, parent)) = &mut s.new_channel {
                parts::card_frame().show(ui, |ui| {
                    egui::ComboBox::from_id_salt("chat-new-kind")
                        .selected_text(kind_label(*kind))
                        .show_ui(ui, |ui| {
                            for k in [ChannelKind::Text, ChannelKind::Voice, ChannelKind::Announcement, ChannelKind::Category] {
                                ui.selectable_value(kind, k, kind_label(k));
                            }
                        });
                    ui.add(crate::ui::controls::singleline(name, crate::ui::controls::HEIGHT).hint_text(c.name));
                    if *kind != ChannelKind::Category {
                        category_combo(ui, "chat-new-parent", parent, &categories);
                    }
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!name.trim().is_empty(), crate::ui::controls::primary(c.create)).clicked() {
                            chat.fire(
                                Method::POST,
                                "/api/v2/chat/channels".into(),
                                Some(json!({
                                    "group_id": id,
                                    "kind": kind.as_str(),
                                    "name": name.trim(),
                                    "parent_id": if *kind == ChannelKind::Category { None } else { *parent },
                                })),
                            );
                            done = true;
                        } else if ui.button(t().common.cancel).clicked() {
                            done = true;
                        }
                    });
                });
            }
            if done {
                s.new_channel = None;
            }
            for ch in &list {
                let mark = match ch.kind {
                    ChannelKind::Category => Mark::Nothing,
                    ChannelKind::Voice => Mark::Glyph(parts::Glyph::Speaker),
                    ChannelKind::Announcement => Mark::Glyph(parts::Glyph::Megaphone),
                    _ => Mark::Glyph(parts::Glyph::Hash),
                };
                let indent = if ch.parent_id.is_some() { 12.0 } else { 0.0 };
                if list_item(ui, s.channel == Some(ch.id), &ch.name, mark, indent) {
                    s.channel = Some(ch.id);
                }
            }
        });
        ui.separator();
        ui.vertical(|ui| {
            let Some(draft) = &mut s.channel_draft else { return };
            ui.label(parts::muted(c.name));
            ui.add(crate::ui::controls::singleline(&mut draft.name, crate::ui::controls::HEIGHT).desired_width(280.0));
            if draft.kind != ChannelKind::Category {
                ui.label(parts::muted(c.topic));
                ui.add(crate::ui::controls::singleline(&mut draft.topic, crate::ui::controls::HEIGHT).desired_width(280.0));
                category_combo(ui, "chat-ch-parent", &mut draft.parent_id, &categories);
                ui.horizontal(|ui| {
                    ui.label(parts::muted(c.slow_mode_label));
                    egui::ComboBox::from_id_salt("chat-slow")
                        .selected_text(if draft.slow_mode_s == 0 { c.off.to_owned() } else { (c.seconds)(draft.slow_mode_s) })
                        .show_ui(ui, |ui| {
                            for v in SLOW {
                                ui.selectable_value(&mut draft.slow_mode_s, v, if v == 0 { c.off.to_owned() } else { (c.seconds)(v) });
                            }
                        });
                });
            }
            if draft.kind == ChannelKind::Voice {
                ui.horizontal(|ui| {
                    ui.label(parts::muted(c.bitrate));
                    for b in [32, 64, 96, 128] {
                        ui.selectable_value(&mut draft.voice_bitrate, b, format!("{b} kbps"));
                    }
                });
                let v = &t().voice;
                ui.horizontal(|ui| {
                    ui.label(parts::muted(v.voice_limit));
                    let mut limited = draft.voice_limit > 0;
                    if ui.checkbox(&mut limited, "").changed() {
                        draft.voice_limit = if limited { 10 } else { 0 };
                    }
                    if limited {
                        ui.add(egui::DragValue::new(&mut draft.voice_limit).range(1..=farwing_protocol::chat::limits::MAX_VOICE_LIMIT));
                    } else {
                        ui.label(parts::muted(v.no_limit));
                    }
                });
                let owner = state.me().is_some_and(|u| u.user_id == g.group.owner_id);
                if owner && state.snap.as_ref().is_some_and(|s| s.limits.voice_compat) {
                    ui.horizontal(|ui| {
                        crate::ui::controls::switch(ui, &mut draft.voice_compat);
                        ui.label(v.compat);
                    });
                }
            }
            ui.horizontal(|ui| {
                if ui.add(crate::ui::controls::primary(c.save)).clicked() {
                    let category = draft.kind == ChannelKind::Category;
                    chat.fire(
                        Method::PATCH,
                        format!("/api/v2/chat/channels/{}", draft.id),
                        Some(json!({
                            "name": draft.name.trim(),
                            "topic": (!category).then(|| draft.topic.clone()),
                            "parent_id": if category { serde_json::Value::Null } else { json!(draft.parent_id) },
                            "slow_mode_s": (!category).then_some(draft.slow_mode_s),
                            "voice_bitrate": (draft.kind == ChannelKind::Voice).then_some(draft.voice_bitrate),
                            "voice_limit": (draft.kind == ChannelKind::Voice).then_some(draft.voice_limit),
                            "voice_compat": (draft.kind == ChannelKind::Voice
                                && state.snap.as_ref().is_some_and(|s| s.limits.voice_compat)
                                && state.me().is_some_and(|u| u.user_id == g.group.owner_id))
                            .then_some(draft.voice_compat),
                        })),
                    );
                }
                if ui.button(RichText::new(c.delete_channel).color(crate::ui::theme::RED)).clicked() {
                    confirm = Some((draft.id, draft.name.clone()));
                }
            });
            ui.add_space(8.0);
            ui.label(RichText::new(c.overwrites).strong());
            let mut roles: Vec<&Role> = g.roles.iter().collect();
            roles.sort_by_key(|r| std::cmp::Reverse(r.position));
            let role = s.overwrite_role.filter(|r| roles.iter().any(|x| x.id == *r)).unwrap_or(id);
            egui::ComboBox::from_id_salt("chat-ow-role")
                .selected_text(roles.iter().find(|r| r.id == role).map_or(String::new(), |r| if r.id == id { c.everyone.to_owned() } else { r.name.clone() }))
                .show_ui(ui, |ui| {
                    for r in &roles {
                        ui.selectable_value(&mut s.overwrite_role, Some(r.id), if r.id == id { c.everyone.to_owned() } else { r.name.clone() });
                    }
                });
            let current = g
                .channels
                .iter()
                .find(|x| x.id == draft.id)
                .and_then(|x| x.overwrites.iter().find(|o| o.role_id == role))
                .copied()
                .unwrap_or(farwing_protocol::chat::Overwrite { role_id: role, allow: 0, deny: 0 });
            let channel_bits = [
                perm::VIEW,
                perm::SEND,
                perm::ATTACH,
                perm::MENTION_ALL,
                perm::MANAGE_MESSAGES,
                perm::READ_HISTORY,
                perm::CONNECT,
                perm::SPEAK,
                perm::VIDEO,
                perm::VOICE_MODERATE,
                perm::PRIORITY,
                perm::MANAGE_CHANNELS,
            ];
            egui::Grid::new("chat-ow-grid").num_columns(2).spacing(vec2(16.0, 4.0)).show(ui, |ui| {
                for bit in channel_bits {
                    ui.label(perm_label(bit));
                    let state = if current.allow & bit != 0 { 2 } else if current.deny & bit != 0 { 0 } else { 1 };
                    ui.horizontal(|ui| {
                        for (v, label) in [(0, c.deny), (1, c.inherit), (2, c.allow)] {
                            if ui.selectable_label(state == v, label).clicked() && state != v {
                                let allow = if v == 2 { current.allow | bit } else { current.allow & !bit };
                                let deny = if v == 0 { current.deny | bit } else { current.deny & !bit };
                                chat.fire(
                                    Method::PUT,
                                    format!("/api/v2/chat/channels/{}/overwrites/{role}", draft.id),
                                    Some(json!({"allow": allow, "deny": deny})),
                                );
                            }
                        }
                    });
                    ui.end_row();
                }
            });
        });
    });
    if let Some((ch, name)) = confirm {
        chat_ui.dialog = Some(Dialog::Confirm {
            title: (c.delete_channel_question)(&name),
            action: Confirmed::DeleteChannel(ch),
        });
    }
}

fn kind_label(kind: ChannelKind) -> &'static str {
    let c = &t().chat;
    match kind {
        ChannelKind::Voice => c.kind_voice,
        ChannelKind::Category => c.kind_category,
        ChannelKind::Announcement => c.kind_announcement,
        _ => c.kind_text,
    }
}

fn category_combo(ui: &mut egui::Ui, salt: &str, parent: &mut Option<Id>, categories: &[&Channel]) {
    let c = &t().chat;
    ui.horizontal(|ui| {
        ui.label(parts::muted(c.category));
        egui::ComboBox::from_id_salt(salt)
            .selected_text(parent.and_then(|p| categories.iter().find(|x| x.id == p)).map_or(c.none.to_owned(), |x| x.name.clone()))
            .show_ui(ui, |ui| {
                ui.selectable_value(parent, None, c.none);
                for cat in categories {
                    ui.selectable_value(parent, Some(cat.id), &cat.name);
                }
            });
    });
}

const MUTES: [i64; 4] = [600, 3600, 86400, 7 * 86400];
const BAN_HOURS: [u32; 4] = [0, 1, 24, 168];

fn members(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi, g: &GroupState) {
    let c = &t().chat;
    let id = g.group.id;
    let p = g.group.permissions;
    let Some(list) = state.members.get(&id) else {
        chat.load_members(id);
        ui.add(egui::Spinner::new());
        return;
    };
    let s = &mut chat_ui.settings;
    ui.add(crate::ui::controls::singleline(&mut s.member_query, crate::ui::controls::HEIGHT).desired_width(260.0).hint_text(t().chat.search));
    let me = state.me().map(|m| m.user_id.clone()).unwrap_or_default();
    let q = s.member_query.to_lowercase();
    let mut kick = None;
    for m in list.iter().filter(|m| format!("{} {} {}", m.nickname, m.user.username, m.user.nickname).to_lowercase().contains(&q)) {
        let uid = m.user.user_id.clone();
        parts::card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                parts::avatar(ui, chat, &m.user, 30.0, None);
                ui.vertical(|ui| {
                    let mut name = parts::display_name(&m.user, &m.nickname);
                    if g.group.owner_id == uid {
                        name = format!("{name} · {}", c.owner);
                    }
                    ui.label(RichText::new(name).color(TEXT));
                    let roles: Vec<String> = g.roles.iter().filter(|r| m.roles.contains(&r.id)).map(|r| r.name.clone()).collect();
                    if !roles.is_empty() {
                        ui.label(parts::muted(roles.join(" · ")));
                    }
                });
                if uid == me || g.group.owner_id == uid {
                    return;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if p & perm::BAN != 0 && ui.button(c.ban).clicked() {
                        s.ban = Some((uid.clone(), parts::display_name(&m.user, &m.nickname), String::new(), 0));
                    }
                    if p & perm::KICK != 0 && ui.button(c.kick).clicked() {
                        kick = Some((uid.clone(), parts::display_name(&m.user, &m.nickname)));
                    }
                    if p & perm::MUTE != 0 {
                        let muted = m.muted_until.is_some_and(|u| u > chrono::Utc::now().timestamp());
                        if muted {
                            if ui.button(c.unmute).clicked() {
                                chat.fire(Method::PATCH, format!("/api/v2/chat/groups/{id}/members/{uid}"), Some(json!({"muted_until": null})));
                            }
                        } else {
                            egui::ComboBox::from_id_salt(("chat-mute", &uid)).selected_text(c.mute).show_ui(ui, |ui| {
                                for (i, secs) in MUTES.iter().enumerate() {
                                    if ui.selectable_label(false, c.mute_durations[i]).clicked() {
                                        let until = chrono::Utc::now().timestamp() + secs;
                                        chat.fire(Method::PATCH, format!("/api/v2/chat/groups/{id}/members/{uid}"), Some(json!({"muted_until": until})));
                                    }
                                }
                            });
                        }
                    }
                    if p & perm::MANAGE_ROLES != 0 {
                        egui::ComboBox::from_id_salt(("chat-roles", &uid)).selected_text(c.roles).show_ui(ui, |ui| {
                            for r in g.roles.iter().filter(|r| r.id != id) {
                                let mut on = m.roles.contains(&r.id);
                                if ui.checkbox(&mut on, &r.name).changed() {
                                    let mut roles = m.roles.clone();
                                    if on {
                                        roles.push(r.id);
                                    } else {
                                        roles.retain(|x| *x != r.id);
                                    }
                                    chat.fire(Method::PATCH, format!("/api/v2/chat/groups/{id}/members/{uid}"), Some(json!({"roles": roles})));
                                }
                            }
                        });
                    }
                });
            });
        });
        ui.add_space(4.0);
    }
    if let Some((uid, name)) = kick {
        chat_ui.dialog = Some(Dialog::Confirm {
            title: (c.kick_question)(&name),
            action: Confirmed::Kick(id, uid),
        });
    }
    let s = &mut chat_ui.settings;
    if let Some((uid, name, reason, hours)) = &mut s.ban {
        let mut done = false;
        egui::Window::new(format!("{} · {name}", c.ban))
            .collapsible(false)
            .resizable(false)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                ui.label(parts::muted(c.reason));
                ui.add(crate::ui::controls::singleline(reason, crate::ui::controls::HEIGHT).desired_width(260.0));
                ui.label(parts::muted(c.delete_recent));
                ui.horizontal(|ui| {
                    for (i, h) in BAN_HOURS.iter().enumerate() {
                        ui.selectable_value(hours, *h, c.delete_hours[i]);
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button(RichText::new(c.ban).color(crate::ui::theme::RED)).clicked() {
                        chat.fire(Method::PUT, format!("/api/v2/chat/groups/{id}/bans/{uid}"), Some(json!({"reason": reason, "delete_hours": hours})));
                        done = true;
                    }
                    if ui.button(t().common.cancel).clicked() {
                        done = true;
                    }
                });
            });
        if done {
            s.ban = None;
        }
    }
}

fn answer_list<'a>(state: &'a ChatState, key: &str, field: &str) -> Option<Vec<&'a serde_json::Value>> {
    match state.answers.get(key)? {
        Answer::Done(v) => Some(v[field].as_array().map(|a| a.iter().collect()).unwrap_or_default()),
        _ => None,
    }
}

fn user_name(v: &serde_json::Value) -> String {
    v["nickname"].as_str().filter(|n| !n.is_empty()).or(v["username"].as_str()).unwrap_or_default().to_owned()
}

fn date(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y/%m/%d %H:%M").to_string())
        .unwrap_or_default()
}

fn invites(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, g: &GroupState) {
    let c = &t().chat;
    let base = chat.server_base();
    let Some(list) = answer_list(state, "s-invites", "invites") else {
        ui.add(egui::Spinner::new());
        return;
    };
    for i in list {
        let code = i["code"].as_str().unwrap_or_default();
        let revoked = i["revoked"].as_bool().unwrap_or(false);
        parts::card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(format!("{base}/chat/invite/{code}")).monospace().color(if revoked { MUTED } else { TEXT }));
                    let uses = i["uses"].as_u64().unwrap_or(0);
                    let max = i["max_uses"].as_u64().map_or(c.unlimited.to_owned(), |m| m.to_string());
                    let expires = i["expires_at"].as_i64().map_or(c.never.to_owned(), date);
                    ui.label(parts::muted(format!("{} · {uses}/{max} · {}", user_name(&i["by"]), if revoked { c.revoked.to_owned() } else { expires })));
                });
                if !revoked {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(c.revoke).clicked() {
                            chat.fire(Method::DELETE, format!("/api/v2/chat/invites/{code}"), None);
                            chat.ask("s-invites", Method::GET, format!("/api/v2/chat/groups/{}/invites", g.group.id), None);
                        }
                        if ui.button(t().chat.copy_link).clicked() {
                            ui.ctx().copy_text(format!("{base}/chat/invite/{code}"));
                        }
                    });
                }
            });
        });
        ui.add_space(4.0);
    }
}

fn bans(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, g: &GroupState) {
    let c = &t().chat;
    let Some(list) = answer_list(state, "s-bans", "bans") else {
        ui.add(egui::Spinner::new());
        return;
    };
    if list.is_empty() {
        ui.label(parts::muted(c.no_bans));
    }
    for b in list {
        let uid = b["user"]["user_id"].as_str().unwrap_or_default().to_owned();
        ui.horizontal(|ui| {
            ui.label(RichText::new(user_name(&b["user"])).color(TEXT));
            let reason = b["reason"].as_str().unwrap_or_default();
            ui.label(parts::muted(format!("{} · {}{}", user_name(&b["by"]), date(b["at"].as_i64().unwrap_or(0)), if reason.is_empty() { String::new() } else { format!(" · {reason}") })));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(c.unban).clicked() {
                    chat.fire(Method::DELETE, format!("/api/v2/chat/groups/{}/bans/{uid}", g.group.id), None);
                    chat.ask("s-bans", Method::GET, format!("/api/v2/chat/groups/{}/bans", g.group.id), None);
                }
            });
        });
    }
}

fn requests(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, g: &GroupState) {
    let c = &t().chat;
    let Some(list) = answer_list(state, "s-requests", "requests") else {
        ui.add(egui::Spinner::new());
        return;
    };
    if list.is_empty() {
        ui.label(parts::muted(c.no_requests));
    }
    for r in list {
        let uid = r["user"]["user_id"].as_str().unwrap_or_default().to_owned();
        ui.horizontal(|ui| {
            ui.label(RichText::new(user_name(&r["user"])).color(TEXT));
            ui.label(parts::muted(date(r["at"].as_i64().unwrap_or(0))));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                for (answer, label) in [("reject", c.reject), ("approve", c.approve)] {
                    if ui.button(label).clicked() {
                        chat.fire(Method::POST, format!("/api/v2/chat/groups/{}/requests/{uid}/{answer}", g.group.id), None);
                        chat.ask("s-requests", Method::GET, format!("/api/v2/chat/groups/{}/requests", g.group.id), None);
                    }
                }
            });
        });
    }
}

fn audit(ui: &mut egui::Ui, state: &ChatState) {
    let Some(list) = answer_list(state, "s-audit", "entries") else {
        ui.add(egui::Spinner::new());
        return;
    };
    for e in list {
        let action = e["action"].as_str().unwrap_or_default();
        let text = (t().chat.audit_action)(action);
        ui.horizontal(|ui| {
            ui.label(parts::muted(date(e["at"].as_i64().unwrap_or(0))));
            ui.label(RichText::new(user_name(&e["actor"])).strong().color(TEXT));
            ui.label(if text.is_empty() { action } else { text });
            if let Some(name) = e["details"]["name"].as_str() {
                ui.label(parts::muted(name));
            }
        });
    }
}
