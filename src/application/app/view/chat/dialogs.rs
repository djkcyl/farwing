//! Chat dialogs: creating, joining and discovering groups, inviting,
//! starting a direct message, personal chat settings, opening a link,
//! confirmations, and the image viewer.
use super::parts;
use super::{ChatUi, Confirmed, Dialog};
use crate::features::chat::state::{Answer, ChatState};
use crate::features::chat::{ChatService, ImageKey};
use crate::i18n::t;
use crate::ui::controls::{DialogAction, DialogIcon, dialog_actions, dialog_frame, dialog_header};
use crate::ui::theme::TEXT;
use egui::{RichText, vec2};
use farwing_protocol::chat::{GroupPreview, Id, Prefs, Status};
use reqwest::Method;
use serde_json::json;

const EXPIRES: [Option<u64>; 7] = [Some(1800), Some(3600), Some(6 * 3600), Some(86400), Some(7 * 86400), Some(30 * 86400), None];
const USES: [u32; 7] = [0, 1, 5, 10, 25, 50, 100];

/// Keeps a dialog with a changing list drawn while it is open.
fn ui_repaint_soon(ctx: &egui::Context, on: bool) {
    if on {
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
    }
}

fn answer<'a>(state: &'a ChatState, key: &str) -> Option<&'a Answer> {
    state.answers.get(key)
}

/// Invite code from a link (`…/chat/invite/<code>`) or the code itself.
fn invite_code(text: &str) -> String {
    let v = text.trim();
    match v.find("/chat/invite/") {
        Some(i) => v[i + "/chat/invite/".len()..].chars().take_while(char::is_ascii_alphanumeric).collect(),
        None => v.to_owned(),
    }
}

fn preview_card(ui: &mut egui::Ui, chat: &ChatService, g: &GroupPreview, note: Option<&str>) -> bool {
    let mut clicked = false;
    parts::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            parts::group_icon(ui, chat, g.id, &g.name, &g.icon, 44.0, false);
            ui.vertical(|ui| {
                ui.label(RichText::new(&g.name).strong().color(TEXT));
                let mut line = (t().chat.member_count)(g.member_count);
                if g.join_approval {
                    line = format!("{line} · {}", t().chat.needs_approval);
                }
                ui.label(parts::muted(line));
                if !g.description.is_empty() {
                    ui.add(egui::Label::new(parts::muted(&g.description)).truncate());
                }
                if let Some(n) = note {
                    ui.label(RichText::new(n).size(12.0).color(crate::ui::theme::AMBER));
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                clicked = note.is_none() && ui.add(crate::ui::controls::primary(t().chat.join)).clicked();
            });
        });
    });
    clicked
}

pub(super) fn show(ctx: &egui::Context, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi) {
    dialog(ctx, chat, state, chat_ui);
    settle(chat, state, chat_ui);
}

fn dialog(ctx: &egui::Context, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi) {
    let Some(mut dialog) = chat_ui.dialog.take() else { return };
    let mut keep = true;
    let mut copied = false;
    let c = &t().chat;
    match &mut dialog {
        Dialog::Create { name, description } => {
            let response = egui::Modal::new(egui::Id::new("chat-create")).frame(dialog_frame()).show(ctx, |ui| {
                ui.set_width(380.0);
                if dialog_header(ui, c.create_group, DialogIcon::Edit, true) {
                    keep = false;
                }
                ui.label(parts::muted(c.group_name));
                ui.add(crate::ui::controls::singleline(name, crate::ui::controls::HEIGHT).desired_width(f32::INFINITY));
                ui.label(parts::muted(c.description));
                ui.add(crate::ui::controls::singleline(description, crate::ui::controls::HEIGHT).desired_width(f32::INFINITY));
                let (ok, cancel) = dialog_actions(ui, Some(DialogAction::new(c.create).enabled(!name.trim().is_empty())), Some(t().common.cancel));
                if ok {
                    chat.ask(
                        "create",
                        Method::POST,
                        "/api/v2/chat/groups".into(),
                        Some(json!({
                            "name": name.trim(),
                            "description": description.trim(),
                            "admin_role": c.starter_admin,
                            "category": c.starter_category,
                            "text_channel": c.starter_text,
                            "voice_channel": c.starter_voice,
                        })),
                    );
                    keep = false;
                }
                keep &= !cancel;
            });
            keep &= !response.should_close();
        }
        Dialog::Join { text } => {
            let code = invite_code(text);
            let key = format!("invite:{code}");
            let valid = code.len() == 10 && code.chars().all(|ch| ch.is_ascii_alphanumeric());
            if valid && answer(state, &key).is_none() {
                chat.ask(&key, Method::GET, format!("/api/v2/chat/invites/{code}"), None);
            }
            let response = egui::Modal::new(egui::Id::new("chat-join")).frame(dialog_frame()).show(ctx, |ui| {
                ui.set_width(420.0);
                if dialog_header(ui, c.join_group, DialogIcon::Info, true) {
                    keep = false;
                }
                ui.label(parts::muted(c.invite_code));
                ui.add(crate::ui::controls::singleline(text, crate::ui::controls::HEIGHT).desired_width(f32::INFINITY));
                if valid && let Some(Answer::Done(v)) = answer(state, &key) {
                    let g: GroupPreview = serde_json::from_value(v["group"].clone()).unwrap_or_default();
                    let note = if v["expired"].as_bool() == Some(true) {
                        Some(c.invite_expired)
                    } else if v["member"].as_bool() == Some(true) {
                        Some(c.already_member)
                    } else {
                        None
                    };
                    ui.add_space(6.0);
                    if preview_card(ui, chat, &g, note) {
                        chat.ask("join", Method::POST, format!("/api/v2/chat/invites/{code}/join"), None);
                        keep = false;
                    }
                }
                let (_, cancel) = dialog_actions(ui, None, Some(t().common.cancel));
                keep &= !cancel;
            });
            keep &= !response.should_close();
        }
        Dialog::Discover { query, sent } => {
            if *query != *sent {
                *sent = query.clone();
                chat.ask("discover", Method::GET, format!("/api/v2/chat/discover?q={}", crate::features::chat::service::urlencode(query.trim())), None);
            }
            let response = egui::Modal::new(egui::Id::new("chat-discover")).frame(dialog_frame()).show(ctx, |ui| {
                ui.set_width(520.0);
                if dialog_header(ui, c.discover, DialogIcon::Info, true) {
                    keep = false;
                }
                ui.add(crate::ui::controls::singleline(query, crate::ui::controls::HEIGHT).desired_width(f32::INFINITY).hint_text(c.discover_search));
                ui.add_space(6.0);
                egui::ScrollArea::vertical().max_height(380.0).min_scrolled_height(380.0).show(ui, |ui| match answer(state, "discover") {
                    Some(Answer::Done(v)) => {
                        let list: Vec<GroupPreview> = serde_json::from_value(v["groups"].clone()).unwrap_or_default();
                        if list.is_empty() {
                            ui.label(parts::muted(c.no_public));
                        }
                        let mine: Vec<Id> = state.snap.as_ref().map(|s| s.groups.iter().map(|g| g.group.id).collect()).unwrap_or_default();
                        for g in &list {
                            let note = mine.contains(&g.id).then_some(c.already_member);
                            if preview_card(ui, chat, g, note) {
                                chat.ask("join", Method::POST, format!("/api/v2/chat/groups/{}/join", g.id), None);
                                keep = false;
                            }
                            ui.add_space(4.0);
                        }
                    }
                    Some(Answer::Failed(e)) => {
                        ui.label(parts::muted(e));
                    }
                    _ => {
                        ui.add(egui::Spinner::new());
                    }
                });
            });
            keep &= !response.should_close();
        }
        Dialog::Invite { group, uses, expires } => {
            let group = *group;
            let Some(g) = state.group(group) else {
                return;
            };
            let response = egui::Modal::new(egui::Id::new("chat-invite")).frame(dialog_frame()).show(ctx, |ui| {
                ui.set_width(440.0);
                if dialog_header(ui, &format!("{} · {}", c.invite, g.group.name), DialogIcon::Info, true) {
                    keep = false;
                }
                ui.horizontal(|ui| {
                    ui.label(parts::muted(c.max_uses));
                    egui::ComboBox::from_id_salt("chat-invite-uses")
                        .selected_text(if *uses == 0 { c.unlimited.to_owned() } else { uses.to_string() })
                        .show_ui(ui, |ui| {
                            for n in USES {
                                ui.selectable_value(uses, n, if n == 0 { c.unlimited.to_owned() } else { n.to_string() });
                            }
                        });
                    ui.label(parts::muted(c.expires));
                    egui::ComboBox::from_id_salt("chat-invite-expires")
                        .selected_text(if *expires >= 6 { c.never } else { c.durations[*expires] })
                        .show_ui(ui, |ui| {
                            for i in 0..7 {
                                ui.selectable_value(expires, i, if i >= 6 { c.never } else { c.durations[i] });
                            }
                        });
                });
                if ui.add(crate::ui::controls::primary(c.create_invite)).clicked() {
                    chat.ask(
                        "invite",
                        Method::POST,
                        format!("/api/v2/chat/groups/{group}/invites"),
                        Some(json!({"max_uses": (*uses > 0).then_some(*uses), "expires_s": EXPIRES[*expires]})),
                    );
                }
                if let Some(Answer::Done(v)) = answer(state, "invite")
                    && let Some(code) = v["code"].as_str()
                {
                    let server = chat.server_base();
                    let link = format!("{server}/chat/invite/{code}");
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(RichText::new(&link).monospace().color(TEXT)).truncate());
                        if ui.button(t().chat.copy_link).clicked() {
                            ui.ctx().copy_text(link.clone());
                            copied = true;
                        }
                    });
                }
                ui.add_space(6.0);
                ui.label(parts::muted(c.invite_friends));
                let members: Vec<String> = state.members.get(&group).map(|l| l.iter().map(|m| m.user.user_id.clone()).collect()).unwrap_or_default();
                egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                    if let Some(Answer::Done(v)) = answer(state, "friends") {
                        for f in v["friends"].as_array().into_iter().flatten() {
                            let user = f["user"]["user_id"].as_str().unwrap_or_default().to_owned();
                            if members.contains(&user) {
                                continue;
                            }
                            let name = f["user"]["nickname"].as_str().filter(|n| !n.is_empty()).or(f["user"]["username"].as_str()).unwrap_or_default();
                            ui.horizontal(|ui| {
                                ui.label(name);
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    let key = format!("add:{group}:{user}");
                                    let sent = matches!(answer(state, &key), Some(Answer::Done(_)));
                                    if ui.add_enabled(!sent, egui::Button::new(if sent { c.invite_sent } else { c.invite })).clicked() {
                                        chat.ask(&key, Method::POST, format!("/api/v2/chat/groups/{group}/add"), Some(json!({"user": user})));
                                    }
                                });
                            });
                        }
                    }
                });
                let (_, close) = dialog_actions(ui, None, Some(t().common.close));
                keep &= !close;
            });
            keep &= !response.should_close();
            if !keep {
                chat.forget("invite");
            }
        }
        Dialog::StartDm => {
            let response = egui::Modal::new(egui::Id::new("chat-start-dm")).frame(dialog_frame()).show(ctx, |ui| {
                ui.set_width(360.0);
                if dialog_header(ui, c.start_dm, DialogIcon::Info, true) {
                    keep = false;
                }
                egui::ScrollArea::vertical().max_height(360.0).min_scrolled_height(360.0).show(ui, |ui| match answer(state, "friends") {
                    Some(Answer::Done(v)) => {
                        for f in v["friends"].as_array().into_iter().flatten() {
                            let user = f["user"]["user_id"].as_str().unwrap_or_default().to_owned();
                            let name = f["user"]["nickname"].as_str().filter(|n| !n.is_empty()).or(f["user"]["username"].as_str()).unwrap_or_default();
                            if ui.add(egui::Button::new(name).min_size(vec2(ui.available_width(), 30.0))).clicked() {
                                chat.ask("open_dm", Method::POST, "/api/v2/chat/dm".into(), Some(json!({"user": user})));
                                keep = false;
                            }
                        }
                    }
                    _ => {
                        ui.add(egui::Spinner::new());
                    }
                });
            });
            keep &= !response.should_close();
        }
        Dialog::Share(draft) => {
            let v = &t().voice;
            let sharing = state.my_voice.as_ref().and_then(|m| m.screen.clone());
            // Windows come and go: the list follows every few seconds.
            if draft.windows && draft.listed.elapsed() > std::time::Duration::from_secs(3) {
                draft.list = crate::platform::windows::window_capture::list();
                draft.listed = std::time::Instant::now();
            }
            ui_repaint_soon(ctx, draft.windows);
            let response = egui::Modal::new(egui::Id::new("chat-share")).frame(dialog_frame()).show(ctx, |ui| {
                ui.set_width(440.0);
                let title = if sharing.is_some() { v.share_settings } else { v.share };
                if dialog_header(ui, title, DialogIcon::Info, true) {
                    keep = false;
                }
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut draft.windows, false, v.screens);
                    ui.selectable_value(&mut draft.windows, true, v.windows);
                });
                ui.add_space(4.0);
                egui::ScrollArea::vertical().max_height(220.0).min_scrolled_height(220.0).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    if draft.windows {
                        if draft.list.is_empty() {
                            ui.label(parts::muted(v.no_windows));
                        }
                        for w in &draft.list {
                            let label = if w.program.is_empty() { w.title.clone() } else { format!("{}  ·  {}", w.title, w.program) };
                            let chosen = draft.window == Some(w.hwnd);
                            if ui
                                .add(egui::Button::selectable(chosen, egui::RichText::new(label)).truncate().min_size(vec2(ui.available_width(), 30.0)))
                                .clicked()
                            {
                                draft.window = Some(w.hwnd);
                            }
                        }
                    } else {
                        for s in &draft.screens {
                            let label = format!("{}  ·  {}×{}", s.display_name, s.width, s.height);
                            let chosen = draft.screen == Some(s.id);
                            if ui
                                .add(egui::Button::selectable(chosen, label).min_size(vec2(ui.available_width(), 30.0)))
                                .clicked()
                            {
                                draft.screen = Some(s.id);
                            }
                        }
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(parts::muted(v.quality));
                    for q in crate::features::host::peer::chat_video::Quality::ALL {
                        ui.selectable_value(&mut draft.quality, q, q.name());
                    }
                });
                ui.horizontal(|ui| {
                    crate::ui::controls::switch(ui, &mut draft.audio);
                    ui.label(v.share_sound);
                });
                if draft.audio && draft.windows && !crate::platform::windows::process_loopback::supported() {
                    ui.label(parts::muted(v.system_sound));
                }
                let window = draft.window.and_then(|h| draft.list.iter().find(|w| w.hwnd == h));
                let ready = if draft.windows { window.is_some() } else { draft.screen.is_some() };
                let start = DialogAction::new(if sharing.is_some() { v.apply } else { v.start_share }).enabled(ready);
                let stop = sharing.is_some().then_some(v.stop_share);
                let (ok, stopped) = dialog_actions(ui, Some(start), stop.or(Some(t().common.cancel)));
                if ok {
                    let target = match window {
                        Some(w) if draft.windows => crate::features::chat::ShareTarget::Window { hwnd: w.hwnd, pid: w.pid },
                        _ => crate::features::chat::ShareTarget::Screen(draft.screen),
                    };
                    chat.share_screen(Some((target, draft.quality, draft.audio)));
                    keep = false;
                }
                if stopped {
                    if sharing.is_some() {
                        chat.share_screen(None);
                    }
                    keep = false;
                }
            });
            keep &= !response.should_close();
        }
        Dialog::Me => {
            let Some(snap) = &state.snap else { return };
            let mut prefs: Prefs = snap.prefs.clone();
            let before = prefs.clone();
            let response = egui::Modal::new(egui::Id::new("chat-me")).frame(dialog_frame()).show(ctx, |ui| {
                ui.set_width(if chat_ui.me_tab == 1 { 710.0 } else { 420.0 });
                if dialog_header(ui, c.settings, DialogIcon::Edit, true) {
                    keep = false;
                }
                ui.horizontal(|ui| {
                    for (tab, label) in [(0u8, c.settings), (1, t().voice.settings)] {
                        if ui.selectable_label(chat_ui.me_tab == tab, label).clicked() {
                            chat_ui.me_tab = tab;
                        }
                    }
                });
                ui.separator();
                if chat_ui.me_tab == 1 {
                    voice_settings(ui, chat, chat_ui);
                    let (_, close) = dialog_actions(ui, None, Some(t().common.close));
                    keep &= !close;
                    return;
                }
                ui.horizontal(|ui| {
                    parts::avatar(ui, chat, &snap.me, 54.0, None);
                    ui.vertical(|ui| {
                        if ui.button(c.change_avatar).clicked() && chat_ui.files.is_none() {
                            chat_ui.settings.avatar_pick = Some(crate::platform::windows::file_dialog::pick_files(c.change_avatar.into()));
                        }
                        if !snap.me.avatar.is_empty() && ui.button(c.remove_avatar).clicked() {
                            chat.fire(Method::DELETE, "/api/v2/me/avatar".into(), None);
                        }
                    });
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(parts::muted(c.status));
                    for s in [Status::Online, Status::Idle, Status::Dnd, Status::Invisible] {
                        if ui.selectable_label(snap.status == s, parts::status_text(s)).clicked() {
                            chat.set_status(s);
                        }
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(parts::muted(c.quiet_hours));
                    ui.add(crate::ui::controls::singleline(&mut prefs.quiet_start, crate::ui::controls::HEIGHT).desired_width(70.0).hint_text("23:00"));
                    ui.label(parts::muted(c.quiet_to));
                    ui.add(crate::ui::controls::singleline(&mut prefs.quiet_end, crate::ui::controls::HEIGHT).desired_width(70.0).hint_text("08:00"));
                });
                ui.horizontal(|ui| {
                    crate::ui::controls::switch(ui, &mut prefs.hide_content);
                    ui.label(c.hide_content);
                });
                ui.horizontal(|ui| {
                    crate::ui::controls::switch(ui, &mut prefs.read_receipts);
                    ui.label(c.read_receipts);
                });
                let (_, close) = dialog_actions(ui, None, Some(t().common.close));
                keep &= !close;
            });
            keep &= !response.should_close();
            let times_ok = [&prefs.quiet_start, &prefs.quiet_end]
                .iter()
                .all(|s| s.is_empty() || chrono::NaiveTime::parse_from_str(s, "%H:%M").is_ok());
            if prefs != before && times_ok && (prefs.hide_content != before.hide_content || prefs.read_receipts != before.read_receipts || !keep) {
                chat.save_prefs(prefs);
            }
        }
        Dialog::Link { href, trust } => {
            let response = egui::Modal::new(egui::Id::new("chat-link")).frame(dialog_frame()).show(ctx, |ui| {
                ui.set_width(440.0);
                if dialog_header(ui, c.link_title, DialogIcon::Warning, true) {
                    keep = false;
                }
                ui.add(egui::Label::new(RichText::new(href.as_str()).monospace().color(TEXT)).wrap());
                ui.checkbox(trust, c.link_trust);
                let (open, cancel) = dialog_actions(ui, Some(DialogAction::new(t().chat.open)), Some(t().common.cancel));
                if open {
                    if *trust
                        && let Some(host) = url::Url::parse(href).ok().and_then(|u| u.host_str().map(str::to_owned))
                    {
                        chat_ui.trusted.insert(host);
                    }
                    ctx.open_url(egui::OpenUrl::new_tab(href.clone()));
                    keep = false;
                }
                keep &= !cancel;
            });
            keep &= !response.should_close();
        }
        Dialog::Confirm { title, action } => {
            let response = egui::Modal::new(egui::Id::new("chat-confirm")).frame(dialog_frame()).show(ctx, |ui| {
                ui.set_width(380.0);
                if dialog_header(ui, title, DialogIcon::Warning, true) {
                    keep = false;
                }
                let (ok, cancel) = dialog_actions(ui, Some(DialogAction::new(t().common.ok).danger(true)), Some(t().common.cancel));
                if ok {
                    match action {
                        Confirmed::Delete(id) => chat.delete(*id),
                        Confirmed::Leave(g) => chat.fire(Method::DELETE, format!("/api/v2/chat/groups/{g}/members/@me"), None),
                        Confirmed::Dissolve(g) => chat.fire(Method::DELETE, format!("/api/v2/chat/groups/{g}"), None),
                        Confirmed::Kick(g, u) => chat.fire(Method::DELETE, format!("/api/v2/chat/groups/{g}/members/{u}"), None),
                        Confirmed::DeleteRole(g, r) => chat.fire(Method::DELETE, format!("/api/v2/chat/groups/{g}/roles/{r}"), None),
                        Confirmed::DeleteChannel(ch) => chat.fire(Method::DELETE, format!("/api/v2/chat/channels/{ch}"), None),
                    }
                    keep = false;
                }
                keep &= !cancel;
            });
            keep &= !response.should_close();
        }
        Dialog::GroupSettings => {
            // Drawn by `settings::show`.
            chat_ui.dialog = Some(dialog);
            return;
        }
    }
    if keep {
        chat_ui.dialog = Some(dialog);
    }
    if copied {
        chat_ui.toast = Some((c.copied.into(), std::time::Instant::now()));
    }
}

/// What answers bring: a new group, a joined one or a DM opens at once.
fn settle(chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi) {
    for key in ["create", "join"] {
        if let Some(Answer::Done(v)) = answer(state, key) {
            let g = if key == "join" { &v["joined"] } else { v };
            if let Some(id) = g["group"]["id"].as_str().and_then(|s| s.parse().ok()) {
                chat_ui.select_group(Some(Id(id)), state);
            } else if v["pending"].as_bool() == Some(true) {
                chat_ui.toast = Some((t().chat.join_pending.into(), std::time::Instant::now()));
            }
            chat.forget(key);
        }
    }
    if let Some(Answer::Done(v)) = answer(state, "open_dm")
        && let Some(id) = v["channel"]["id"].as_str().and_then(|s| s.parse().ok())
    {
        chat_ui.group = None;
        chat_ui.select_channel(Id(id));
        chat.forget("open_dm");
    }
    // A call from the friends page: the direct message opens and rings.
    if let Some(Answer::Done(v)) = answer(state, "call_dm")
        && let Some(id) = v["channel"]["id"].as_str().and_then(|s| s.parse().ok())
    {
        chat_ui.group = None;
        chat_ui.select_channel(Id(id));
        chat.forget("call_dm");
        chat.start_call(Id(id));
    }
    // The avatar file chosen.
    if let Some(rx) = &chat_ui.settings.avatar_pick
        && let Ok(paths) = rx.try_recv()
    {
        chat_ui.settings.avatar_pick = None;
        if let Some(p) = paths.into_iter().next()
            && let Ok(bytes) = std::fs::read(p)
        {
            chat.upload_square("/api/v2/me/avatar".into(), bytes);
        }
    }
}

pub(super) fn image_viewer(ctx: &egui::Context, chat: &ChatService, chat_ui: &mut ChatUi) {
    let Some(a) = chat_ui.image.clone() else { return };
    let screen = ctx.content_rect();
    let mut close = false;
    egui::Area::new(egui::Id::new("chat-image-viewer"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            let (rect, response) = ui.allocate_exact_size(screen.size(), egui::Sense::click());
            ui.painter().rect_filled(rect, 0.0, egui::Color32::from_black_alpha(220));
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                chat_ui.zoom = (chat_ui.zoom * (1.0 + scroll / 400.0)).clamp(0.2, 8.0);
            }
            let tex = chat.image(&ImageKey::Full(a.id, a.name.clone())).or_else(|| chat.image(&ImageKey::Thumb(a.id)));
            if let Some(t) = tex {
                let fit = ((rect.width() * 0.92) / a.width.max(1) as f32).min((rect.height() * 0.86) / a.height.max(1) as f32).min(1.0);
                let size = vec2(a.width as f32, a.height as f32) * fit * chat_ui.zoom;
                egui::Image::new(&t).paint_at(ui, egui::Rect::from_center_size(rect.center(), size));
            } else {
                ui.put(egui::Rect::from_center_size(rect.center(), vec2(40.0, 40.0)), egui::Spinner::new());
            }
            let bar = egui::Rect::from_min_size(rect.min, vec2(rect.width(), 44.0));
            ui.scope_builder(egui::UiBuilder::new().max_rect(bar.shrink2(vec2(14.0, 6.0))).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
                ui.label(RichText::new(&a.name).color(TEXT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if parts::glyph_button(ui, parts::Glyph::Cross, t().common.close, false).clicked() {
                        close = true;
                    }
                    if parts::glyph_button(ui, parts::Glyph::Down, t().chat.download, false).clicked() {
                        chat.download(&a);
                    }
                });
            });
            if response.clicked() && !bar.contains(response.interact_pointer_pos().unwrap_or_default()) {
                close = true;
            }
        });
    if close || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        chat_ui.image = None;
    }
}

/// The voice settings: devices, processing, how speaking starts, volume.
fn voice_settings(ui: &mut egui::Ui, chat: &ChatService, chat_ui: &mut ChatUi) {
    use crate::features::voice::capture::{Noise, endpoints};
    use crate::features::voice::settings::{Key, Mode};
    let v = &t().voice;
    let mut s = chat.voice_settings();
    let before = s.clone();
    let (inputs, outputs) = chat_ui.endpoints.get_or_insert_with(|| (endpoints(true), endpoints(false))).clone();
    let device = |ui: &mut egui::Ui, salt: &str, label: &str, value: &mut Option<String>, list: &[crate::features::voice::capture::Endpoint]| {
        ui.label(parts::muted(label));
        let shown = value
            .as_ref()
            .and_then(|id| list.iter().find(|e| &e.id == id))
            .map_or(v.default_device.to_owned(), |e| e.name.clone());
        egui::ComboBox::from_id_salt(salt).width(340.0).selected_text(shown).show_ui(ui, |ui| {
            ui.selectable_value(value, None, v.default_device);
            for e in list {
                ui.selectable_value(value, Some(e.id.clone()), &e.name);
            }
        });
    };
    // Two columns: the whole page fits a small window.
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.set_width(360.0);
            device(ui, "voice-input", v.input, &mut s.input, &inputs);
            device(ui, "voice-output", v.output, &mut s.output, &outputs);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(parts::muted(v.input_volume));
                let mut volume = u32::from(s.input_volume);
                if ui.add(egui::Slider::new(&mut volume, 0..=200).suffix("%")).changed() {
                    s.input_volume = volume as u8;
                }
            });
            // The microphone's level, in voice or not.
            let level = chat.input_meter();
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, 3.0, crate::ui::theme::SURFACE);
            let db = if level > 0.0 { 20.0 * level.log10() } else { -90.0 };
            let fill = ((db + 70.0) / 70.0).clamp(0.0, 1.0);
            let bar = egui::Rect::from_min_size(rect.min, vec2(rect.width() * fill, rect.height()));
            let open = s.mode == Mode::Activity && s.threshold_db.is_none_or(|t| db > t);
            ui.painter().rect_filled(bar, 3.0, if open && db > -60.0 { crate::ui::theme::GREEN } else { crate::ui::theme::MUTED });
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            {
                use crate::features::chat::MicTest;
                let state = chat.mic_test_state();
                let label = match state {
                    MicTest::Idle => v.mic_test.to_owned(),
                    MicTest::Recording(left) => (v.mic_recording)(left),
                    MicTest::Playing => v.mic_playing.to_owned(),
                };
                let in_voice = chat.view().my_voice.is_some();
                if ui
                    .add_enabled(state == MicTest::Idle && !in_voice, egui::Button::new(label))
                    .clicked()
                {
                    chat.mic_test();
                }
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                crate::ui::controls::switch(ui, &mut s.echo);
                ui.label(v.echo);
            });
            ui.horizontal(|ui| {
                crate::ui::controls::switch(ui, &mut s.gain);
                ui.label(v.agc);
            });
            ui.horizontal(|ui| {
                ui.label(parts::muted(v.noise));
                for (n, label) in [(Noise::Off, v.noise_off), (Noise::Standard, v.noise_standard), (Noise::Ai, v.noise_ai)] {
                    ui.selectable_value(&mut s.noise, n, label);
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(parts::muted(v.mode));
                ui.selectable_value(&mut s.mode, Mode::Activity, v.activity);
                ui.selectable_value(&mut s.mode, Mode::PushToTalk, v.push_to_talk);
            });
            match s.mode {
                Mode::Activity => {
                    ui.horizontal(|ui| {
                        ui.label(parts::muted(v.threshold));
                        let mut auto = s.threshold_db.is_none();
                        if ui.checkbox(&mut auto, v.auto).changed() {
                            s.threshold_db = if auto { None } else { Some(-45.0) };
                        }
                        if let Some(db) = &mut s.threshold_db {
                            ui.add(egui::Slider::new(db, -70.0..=-20.0).suffix(" dB"));
                        }
                    });
                }
                Mode::PushToTalk => {
                    ui.horizontal(|ui| {
                        ui.label(parts::muted(v.key));
                        let label = if chat_ui.capturing_key {
                            v.press_key.to_owned()
                        } else {
                            s.key.map_or(v.no_key.to_owned(), |k| {
                                crate::plugins::hotkeys::label(&crate::plugins::hotkeys::Binding { key: k.vk, modifiers: 0 })
                            })
                        };
                        if ui.button(label).clicked() {
                            chat_ui.capturing_key = !chat_ui.capturing_key;
                        }
                    });
                    if chat_ui.capturing_key {
                        let pressed = ui.input(|i| {
                            i.events.iter().find_map(|e| match e {
                                egui::Event::Key { key, pressed: true, .. } => crate::plugins::hotkeys::key_code(*key),
                                egui::Event::PointerButton { button: egui::PointerButton::Extra1, pressed: true, .. } => Some(5),
                                egui::Event::PointerButton { button: egui::PointerButton::Extra2, pressed: true, .. } => Some(6),
                                egui::Event::PointerButton { button: egui::PointerButton::Middle, pressed: true, .. } => Some(4),
                                _ => None,
                            })
                        });
                        if let Some(vk) = pressed {
                            s.key = Some(Key { vk });
                            chat_ui.capturing_key = false;
                        }
                    }
                }
            }
        });
        ui.add_space(20.0);
        ui.vertical(|ui| {
            ui.set_width(330.0);
            // Keys that toggle mute and deafen, wherever the focus is.
            for (which, label) in [(1u8, v.mute), (2u8, v.deafen)] {
                let slot = if which == 1 { &mut s.mute_key } else { &mut s.deafen_key };
                ui.horizontal(|ui| {
                    ui.label(parts::muted(format!("{label} · {}", v.key)));
                    let waiting = chat_ui.capturing_toggle == which;
                    let text = if waiting {
                        v.press_key.to_owned()
                    } else {
                        slot.map_or(v.no_key.to_owned(), |k| {
                            crate::plugins::hotkeys::label(&crate::plugins::hotkeys::Binding { key: k.vk, modifiers: 0 })
                        })
                    };
                    if ui.button(text).clicked() {
                        chat_ui.capturing_toggle = if waiting { 0 } else { which };
                    }
                    if slot.is_some() && ui.small_button("×").clicked() {
                        *slot = None;
                    }
                });
                if chat_ui.capturing_toggle == which
                    && let Some(vk) = ui.input(|i| {
                        i.events.iter().find_map(|e| match e {
                            egui::Event::Key { key, pressed: true, .. } => crate::plugins::hotkeys::key_code(*key),
                            _ => None,
                        })
                    })
                {
                    *slot = Some(Key { vk });
                    chat_ui.capturing_toggle = 0;
                }
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(parts::muted(v.volume));
                let mut volume = u32::from(s.volume);
                if ui.add(egui::Slider::new(&mut volume, 0..=200).suffix("%")).changed() {
                    s.volume = volume as u8;
                }
            });
            ui.add_space(6.0);
            ui.label(RichText::new(v.sounds).strong().color(TEXT));
            ui.horizontal(|ui| {
                ui.label(parts::muted(v.sound_volume));
                let mut volume = u32::from(s.sound_volume);
                if ui.add(egui::Slider::new(&mut volume, 0..=100).suffix("%")).changed() {
                    s.sound_volume = volume as u8;
                }
            });
            use crate::features::voice::sounds::SoundGroup;
            for (group, label) in SoundGroup::ALL.into_iter().zip(v.sound_groups) {
                ui.horizontal(|ui| {
                    let mut on = !s.sounds_off.contains(&group);
                    if crate::ui::controls::switch(ui, &mut on).changed() {
                        if on {
                            s.sounds_off.remove(&group);
                        } else {
                            s.sounds_off.insert(group);
                        }
                    }
                    ui.label(label);
                });
            }
        });
    });
    if s != before {
        chat.set_voice_settings(s);
    }
}
