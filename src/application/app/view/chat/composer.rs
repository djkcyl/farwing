//! The message box (`docs/protocol/chat-design.md` §4.1, §12.1): several
//! lines and the input method, @ / : / # completion, emoji, files (picked,
//! pasted, dropped), replying and editing, the length limit.
use super::parts::{self, Glyph};
use super::ChatUi;
use crate::features::chat::state::ChatState;
use crate::features::chat::{ChatService, ImageKey, emoji};
use crate::i18n::t;
use crate::ui::theme::{ACCENT, HOVER, LINE, MUTED, RED, SURFACE, TEXT};
use egui::text::{CCursor, CCursorRange};
use egui::{Color32, Key, Modifiers, Rect, RichText, Sense, Stroke, vec2};
use farwing_protocol::chat::{Channel, ChannelKind, Dm, GroupState, Id, perm};

pub(super) struct Suggest {
    channel: Id,
    /// Char index where the completed word starts (at its sigil).
    start: usize,
    items: Vec<(String, String)>,
    at: usize,
}

fn draft(chat_ui: &ChatUi, channel: Id) -> &str {
    chat_ui.drafts.get(&channel).map_or("", String::as_str)
}

/// How tall the composer is now.
pub(super) fn height(ui: &egui::Ui, chat_ui: &ChatUi, chat: &ChatService, channel: Id) -> f32 {
    let text = draft(chat_ui, channel);
    let width = (ui.max_rect().width() - 140.0).max(100.0);
    let galley = ui.painter().layout(text.to_owned(), egui::FontId::proportional(14.0), TEXT, width);
    let lines = (galley.size().y / 19.0).ceil().clamp(1.0, 8.0);
    let mut h = 22.0 + lines * 19.0 + 12.0 + 26.0;
    if chat_ui.reply.is_some() || chat_ui.editing.is_some() {
        h += 30.0;
    }
    if !chat.uploads(channel).is_empty() {
        h += 40.0;
    }
    h
}

pub(super) fn poll_files(chat: &ChatService, chat_ui: &mut ChatUi) {
    let Some(rx) = &chat_ui.files else { return };
    if let Ok(paths) = rx.try_recv() {
        chat_ui.files = None;
        if let Some(channel) = chat_ui.channel {
            attach_paths(chat, channel, paths);
        }
    }
}

pub(super) fn attach_paths(chat: &ChatService, channel: Id, paths: Vec<std::path::PathBuf>) {
    let chat = chat.clone();
    std::thread::spawn(move || {
        let files: Vec<(String, Vec<u8>)> = paths
            .into_iter()
            .filter_map(|p| {
                let name = p.file_name()?.to_string_lossy().into_owned();
                // Over the limit fails in `attach` with its name.
                let bytes = std::fs::read(&p).ok()?;
                Some((name, bytes))
            })
            .collect();
        chat.attach(channel, files);
    });
}

fn blocked_reason(channel: &Channel, group: Option<&GroupState>, dm: Option<&Dm>) -> Option<String> {
    if channel.permissions & perm::SEND != 0 {
        return None;
    }
    let c = &t().chat;
    Some(if dm.is_some_and(|d| !d.friends) {
        c.not_friends.into()
    } else if group.is_some_and(|g| g.group.frozen) {
        c.frozen.into()
    } else if let Some(until) = group.and_then(|g| g.me.muted_until).filter(|u| *u > chrono::Utc::now().timestamp()) {
        let at = chrono::DateTime::from_timestamp(until, 0)
            .map(|d| d.with_timezone(&chrono::Local).format("%Y/%m/%d %H:%M").to_string())
            .unwrap_or_default();
        (c.muted_until)(&at)
    } else {
        c.no_permission.into()
    })
}

pub(super) fn composer(
    ui: &mut egui::Ui,
    chat: &ChatService,
    state: &ChatState,
    chat_ui: &mut ChatUi,
    channel: &Channel,
    group: Option<&GroupState>,
    dm: Option<&Dm>,
) {
    let rect = ui.max_rect().shrink2(vec2(16.0, 0.0));
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        if let Some(why) = blocked_reason(channel, group, dm) {
            box_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add_space(4.0);
                ui.label(parts::muted(why));
                ui.add_space(4.0);
            });
            return;
        }
        let c = &t().chat;
        // Reply or edit bar.
        if chat_ui.reply.is_some() || chat_ui.editing.is_some() {
            let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::hover());
            ui.painter().rect_filled(bar, egui::CornerRadius { nw: 8, ne: 8, sw: 0, se: 0 }, SURFACE);
            ui.scope_builder(egui::UiBuilder::new().max_rect(bar.shrink2(vec2(10.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
                let text = match (&chat_ui.editing, &chat_ui.reply) {
                    (Some(_), _) => c.editing.to_owned(),
                    (None, Some(r)) => (c.replying_to)(&parts::display_name(&r.author, "")),
                    _ => String::new(),
                };
                ui.label(RichText::new(text).size(12.5).color(MUTED));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if parts::glyph_button(ui, Glyph::Cross, t().common.cancel, false).clicked() {
                        if chat_ui.editing.take().is_some() {
                            chat_ui.drafts.remove(&channel.id);
                        }
                        chat_ui.reply = None;
                    }
                    if chat_ui.reply.is_some() {
                        let label = RichText::new(format!("@ {}", c.mention)).size(12.0).color(if chat_ui.mention_reply { ACCENT } else { MUTED });
                        if ui.add(egui::Label::new(label).sense(Sense::click())).clicked() {
                            chat_ui.mention_reply = !chat_ui.mention_reply;
                        }
                    }
                });
            });
        }
        // Files being attached.
        let uploads = chat.uploads(channel.id);
        if !uploads.is_empty() {
            let (bar, _) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::hover());
            ui.painter().rect_filled(bar, 0.0, SURFACE);
            ui.scope_builder(egui::UiBuilder::new().max_rect(bar.shrink2(vec2(8.0, 6.0))).layout(egui::Layout::left_to_right(egui::Align::Center)), |ui| {
                for u in &uploads {
                    let color = if u.error.is_some() { RED } else if u.done.is_some() { TEXT } else { MUTED };
                    egui::Frame::new()
                        .stroke(Stroke::new(1.0, if u.error.is_some() { RED } else { LINE }))
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(8, 3))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if u.done.is_none() && u.error.is_none() {
                                    ui.add(egui::Spinner::new().size(12.0));
                                }
                                let label = ui.add(egui::Label::new(RichText::new(&u.name).size(12.5).color(color)).truncate());
                                if let Some(e) = &u.error {
                                    label.on_hover_text(e);
                                }
                                if parts::glyph_button(ui, Glyph::Cross, t().common.remove, false).clicked() {
                                    chat.remove_upload(channel.id, u.key);
                                }
                            });
                        });
                    ui.add_space(6.0);
                }
            });
        }
        let id = ui.make_persistent_id(("chat-input", channel.id));
        let focused = ui.memory(|m| m.has_focus(id));
        // Keys the completion list takes first.
        let mut picked: Option<usize> = None;
        if focused
            && let Some(s) = &mut chat_ui.suggest
            && s.channel == channel.id
            && !s.items.is_empty()
        {
            let n = s.items.len();
            ui.input_mut(|i| {
                if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                    s.at = (s.at + 1) % n;
                }
                if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                    s.at = (s.at + n - 1) % n;
                }
                if i.consume_key(Modifiers::NONE, Key::Enter) || i.consume_key(Modifiers::NONE, Key::Tab) {
                    picked = Some(s.at);
                }
            });
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                chat_ui.suggest = None;
            }
        }
        let mut send = false;
        if focused && picked.is_none() && chat_ui.suggest.as_ref().is_none_or(|s| s.items.is_empty() || s.channel != channel.id) {
            send = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                if chat_ui.editing.take().is_some() {
                    chat_ui.drafts.remove(&channel.id);
                }
                chat_ui.reply = None;
            }
            if draft(chat_ui, channel.id).is_empty() && chat_ui.editing.is_none() && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                edit_last(state, chat_ui, channel.id);
            }
            // A pasted picture becomes an attachment.
            if ui.input(|i| i.modifiers.command && i.key_pressed(Key::V))
                && channel.permissions & perm::ATTACH != 0
                && let Some(png) = crate::platform::windows::file_dialog::clipboard_png()
            {
                chat.attach(channel.id, vec![("image.png".into(), png)]);
            }
        }
        let rounded = if chat_ui.reply.is_some() || chat_ui.editing.is_some() || !uploads.is_empty() {
            egui::CornerRadius { nw: 0, ne: 0, sw: 8, se: 8 }
        } else {
            egui::CornerRadius::same(8)
        };
        let mut output = None;
        let inner = box_frame().corner_radius(rounded).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                if channel.permissions & perm::ATTACH != 0
                    && chat_ui.editing.is_none()
                    && parts::glyph_button(ui, Glyph::Clip, c.attach, false).clicked()
                    && chat_ui.files.is_none()
                {
                    chat_ui.files = Some(crate::platform::windows::file_dialog::pick_files(c.attach.into()));
                }
                let width = ui.available_width() - 36.0;
                let hint = match dm {
                    Some(d) => (c.to_user)(&parts::display_name(&d.user, "")),
                    None => (c.to_channel)(&channel.name),
                };
                let text = chat_ui.drafts.entry(channel.id).or_default();
                let before = text.clone();
                let out = egui::TextEdit::multiline(text)
                    .id(id)
                    .frame(egui::Frame::NONE)
                    .desired_rows(1)
                    .desired_width(width)
                    .hint_text(RichText::new(hint).color(Color32::from_rgb(110, 120, 136)))
                    .font(egui::FontId::proportional(14.0))
                    .show(ui);
                if *text != before && !text.is_empty() {
                    chat.typing(channel.id);
                }
                output = Some(out);
                if parts::glyph_button(ui, Glyph::Smile, c.emoji, chat_ui.picker).clicked() {
                    chat_ui.picker = !chat_ui.picker;
                }
            });
        });
        let box_rect = inner.response.rect;
        let max = state.snap.as_ref().map_or(4000, |s| s.limits.max_content_chars as usize);
        let chars = draft(chat_ui, channel.id).chars().count();
        // Completion from the word before the cursor.
        if let Some(out) = &output {
            if let Some(range) = out.cursor_range {
                let cursor = range.primary.index.into();
                complete(chat_ui, state, channel, group, dm, cursor);
            }
            if let Some(i) = picked {
                apply(ui.ctx(), chat_ui, channel.id, id, i, out.state.clone());
            }
        }
        if send {
            submit(chat, chat_ui, channel.id, max);
        }
        suggestions(ui, chat, chat_ui, channel.id, id, box_rect, output.as_ref().map(|o| o.state.clone()));
        if chat_ui.picker {
            picker(ui, chat, state, chat_ui, channel.id, box_rect);
        }
        // Typing, slow mode, length.
        ui.horizontal(|ui| {
            ui.set_height(24.0);
            let names: Vec<String> = state
                .typing
                .get(&channel.id)
                .map(|t| {
                    let me = state.me().map(|m| m.user_id.clone()).unwrap_or_default();
                    let n = state.names(group);
                    t.keys().filter(|u| **u != me).map(|u| crate::features::chat::markdown::Names::user(&n, u)).collect()
                })
                .unwrap_or_default();
            let typing = match names.len() {
                0 => String::new(),
                1 => (c.typing_one)(&names[0]),
                2 => (c.typing_two)(&names[0], &names[1]),
                _ => c.typing_many.into(),
            };
            ui.label(parts::muted(typing));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if chars > max && channel.permissions & perm::ATTACH != 0 && ui.link(c.to_file).clicked() {
                    let text = chat_ui.drafts.remove(&channel.id).unwrap_or_default();
                    chat.attach(channel.id, vec![("message.txt".into(), text.into_bytes())]);
                }
                if chars + 200 > max {
                    ui.label(RichText::new(format!("{chars}/{max}")).size(12.0).color(if chars > max { RED } else { MUTED }));
                }
                if channel.slow_mode_s > 0 && channel.permissions & perm::MANAGE_MESSAGES == 0 {
                    ui.label(parts::muted((c.slow_mode)(channel.slow_mode_s)));
                }
            });
        });
    });
}

fn box_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(Color32::from_rgb(37, 44, 56))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(6, 6))
}

fn edit_last(state: &ChatState, chat_ui: &mut ChatUi, channel: Id) {
    let me = state.me().map(|m| m.user_id.clone()).unwrap_or_default();
    if let Some(e) = state
        .messages
        .get(&channel)
        .and_then(|b| b.list.iter().rev().find(|e| e.message.author.user_id == me && !e.message.deleted && !e.sending))
    {
        chat_ui.drafts.insert(channel, e.message.content.clone());
        chat_ui.editing = Some(e.message.clone());
    }
}

fn submit(chat: &ChatService, chat_ui: &mut ChatUi, channel: Id, max: usize) {
    let text = draft(chat_ui, channel).to_owned();
    let content = if text.trim().is_empty() { String::new() } else { text };
    if content.chars().count() > max {
        return;
    }
    if let Some(m) = chat_ui.editing.take() {
        if !content.is_empty() {
            chat.edit(m.id, content);
        }
        chat_ui.drafts.remove(&channel);
        return;
    }
    let attachments = if chat.uploads(channel).is_empty() {
        Vec::new()
    } else {
        match chat.take_uploads(channel) {
            Some(ids) => ids,
            // Still uploading: sent once they are done.
            None => return,
        }
    };
    if content.is_empty() && attachments.is_empty() {
        return;
    }
    let reply = chat_ui.reply.take();
    chat.send(channel, content, reply, chat_ui.mention_reply, attachments);
    chat_ui.drafts.remove(&channel);
    chat_ui.mention_reply = true;
    chat_ui.scroll.offset = Some(f32::MAX);
}

fn complete(chat_ui: &mut ChatUi, state: &ChatState, channel: &Channel, group: Option<&GroupState>, dm: Option<&Dm>, cursor: usize) {
    let text = draft(chat_ui, channel.id);
    let before: Vec<char> = text.chars().take(cursor).collect();
    let mut start = before.len();
    while start > 0 && !before[start - 1].is_whitespace() && !"@#:".contains(before[start - 1]) {
        start -= 1;
    }
    let Some(sigil) = start.checked_sub(1).map(|i| before[i]).filter(|c| "@#:".contains(*c)) else {
        chat_ui.suggest = None;
        return;
    };
    if start >= 2 && !before[start - 2].is_whitespace() {
        chat_ui.suggest = None;
        return;
    }
    let query: String = before[start..].iter().collect::<String>().to_lowercase();
    let mut items: Vec<(String, String)> = Vec::new();
    match sigil {
        '@' => {
            if let Some(g) = group {
                if let Some(members) = state.members.get(&g.group.id) {
                    for m in members {
                        let name = parts::display_name(&m.user, &m.nickname);
                        if name.to_lowercase().contains(&query) || m.user.username.to_lowercase().contains(&query) {
                            items.push((name, format!("<@{}> ", m.user.user_id)));
                        }
                        if items.len() >= 8 {
                            break;
                        }
                    }
                }
                let all = channel.permissions & perm::MENTION_ALL != 0;
                for r in &g.roles {
                    if r.id != g.group.id && (r.mentionable || all) && r.name.to_lowercase().contains(&query) {
                        items.push((format!("@{}", r.name), format!("<@&{}> ", r.id)));
                    }
                }
                if all {
                    for (key, label) in [("everyone", t().chat.mention_all), ("here", t().chat.mention_here)] {
                        if key.starts_with(&query) || label.contains(&query) {
                            items.push((format!("@{label}"), format!("<@{key}> ")));
                        }
                    }
                }
            } else if let Some(d) = dm {
                items.push((parts::display_name(&d.user, ""), format!("<@{}> ", d.user.user_id)));
            }
        }
        '#' => {
            if let Some(g) = group {
                for c in &g.channels {
                    if c.kind != ChannelKind::Category && c.name.to_lowercase().contains(&query) {
                        items.push((format!("#{}", c.name), format!("<#{}> ", c.id)));
                    }
                }
            }
        }
        _ => {
            if query.chars().count() >= 2 {
                for e in emoji::search(&state.emoji, &query, 8) {
                    let name = if crate::i18n::language() == crate::i18n::Language::ZhCn && !e.zn.is_empty() { e.zn.clone() } else { e.n.clone() };
                    items.push((format!("{} {name}", e.e), e.e.clone()));
                }
            }
        }
    }
    items.truncate(10);
    if items.is_empty() {
        chat_ui.suggest = None;
        return;
    }
    let at = chat_ui.suggest.as_ref().filter(|s| s.start == start - 1 && s.channel == channel.id).map_or(0, |s| s.at.min(items.len() - 1));
    chat_ui.suggest = Some(Suggest {
        channel: channel.id,
        start: start - 1,
        items,
        at,
    });
}

fn apply(ctx: &egui::Context, chat_ui: &mut ChatUi, channel: Id, id: egui::Id, index: usize, mut state: egui::text_edit::TextEditState) {
    let Some(s) = chat_ui.suggest.take() else { return };
    let Some((_, insert)) = s.items.get(index) else { return };
    let text = chat_ui.drafts.entry(channel).or_default();
    let cursor = state.cursor.char_range().map_or(text.chars().count(), |r| r.primary.index.into());
    let chars: Vec<char> = text.chars().collect();
    let mut out: String = chars[..s.start.min(chars.len())].iter().collect();
    out.push_str(insert);
    let end = out.chars().count();
    out.extend(&chars[cursor.min(chars.len())..]);
    *text = out;
    state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(end))));
    state.store(ctx, id);
}

fn suggestions(
    ui: &mut egui::Ui,
    _chat: &ChatService,
    chat_ui: &mut ChatUi,
    channel: Id,
    id: egui::Id,
    above: Rect,
    edit: Option<egui::text_edit::TextEditState>,
) {
    let Some(s) = &chat_ui.suggest else { return };
    if s.channel != channel {
        return;
    }
    let mut clicked = None;
    let height = s.items.len() as f32 * 30.0 + 12.0;
    egui::Area::new(ui.id().with("chat-suggest"))
        .order(egui::Order::Foreground)
        .fixed_pos(above.left_top() - vec2(0.0, height + 6.0))
        .show(ui.ctx(), |ui| {
            crate::ui::controls::dialog_frame().inner_margin(egui::Margin::same(6)).show(ui, |ui| {
                ui.set_width(above.width() - 12.0);
                for (i, (label, _)) in s.items.iter().enumerate() {
                    let (r, response) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::click());
                    if i == s.at || response.hovered() {
                        ui.painter().rect_filled(r, 5.0, if i == s.at { crate::ui::theme::SELECTED } else { HOVER });
                    }
                    ui.painter().text(r.left_center() + vec2(8.0, 0.0), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(14.0), TEXT);
                    if response.clicked() {
                        clicked = Some(i);
                    }
                }
            });
        });
    if let (Some(i), Some(state)) = (clicked, edit) {
        apply(ui.ctx(), chat_ui, channel, id, i, state);
        ui.memory_mut(|m| m.request_focus(id));
    }
}

fn picker(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi, channel: Id, above: Rect) {
    let size = vec2(356.0, 380.0);
    let pos = egui::pos2(above.right() - size.x, above.top() - size.y - 8.0);
    let mut chosen: Option<String> = None;
    let area = egui::Area::new(ui.id().with("chat-emoji"))
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .show(ui.ctx(), |ui| {
            crate::ui::controls::dialog_frame().inner_margin(egui::Margin::same(8)).show(ui, |ui| {
                ui.set_min_size(size - vec2(16.0, 16.0));
                ui.set_max_width(size.x - 16.0);
                ui.horizontal(|ui| {
                    ui.add(crate::ui::controls::singleline(&mut chat_ui.picker_query, 28.0).desired_width(200.0).hint_text(t().chat.search));
                    for (i, tone) in [Color32::from_rgb(255, 200, 61), Color32::from_rgb(247, 215, 196), Color32::from_rgb(216, 176, 148), Color32::from_rgb(187, 145, 103), Color32::from_rgb(142, 86, 46), Color32::from_rgb(97, 61, 48)].into_iter().enumerate() {
                        let (r, response) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::click());
                        ui.painter().circle_filled(r.center(), 7.0, tone);
                        if chat_ui.picker_tone == i {
                            ui.painter().circle_stroke(r.center(), 8.0, Stroke::new(1.5, TEXT));
                        }
                        if response.clicked() {
                            chat_ui.picker_tone = i;
                        }
                    }
                });
                ui.add_space(6.0);
                let browsing = chat_ui.picker_query.trim().is_empty();
                if browsing {
                    ui.horizontal(|ui| {
                        for (i, (_, sample)) in emoji::GROUPS.iter().enumerate() {
                            let (r, response) = ui.allocate_exact_size(vec2(32.0, 30.0), Sense::click());
                            if chat_ui.picker_group == i {
                                ui.painter().rect_filled(r, 5.0, crate::ui::theme::SELECTED);
                            } else if response.hovered() {
                                ui.painter().rect_filled(r, 5.0, HOVER);
                            }
                            if let Some(tex) = chat.image(&ImageKey::Emoji(emoji::key(sample))) {
                                egui::Image::new(&tex).paint_at(ui, r.shrink(6.0));
                            }
                            if response.clicked() {
                                chat_ui.picker_group = i;
                            }
                        }
                    });
                    ui.add_space(4.0);
                }
                let entries: Vec<&emoji::Entry> = if browsing {
                    let group = emoji::GROUPS[chat_ui.picker_group].0;
                    state.emoji.iter().filter(|e| e.g == group).collect()
                } else {
                    emoji::search(&state.emoji, &chat_ui.picker_query, 160)
                };
                const COLS: usize = 8;
                let rows = entries.len().div_ceil(COLS);
                egui::ScrollArea::vertical().id_salt("chat-emoji-grid").max_height(size.y - if browsing { 106.0 } else { 70.0 }).show_rows(ui, 40.0, rows, |ui, range| {
                    for row in range {
                        ui.horizontal(|ui| {
                            for e in entries.iter().skip(row * COLS).take(COLS) {
                                let value = emoji::with_tone(e, chat_ui.picker_tone);
                                let (r, response) = ui.allocate_exact_size(vec2(38.0, 38.0), Sense::click());
                                if response.hovered() {
                                    ui.painter().rect_filled(r, 5.0, HOVER);
                                }
                                if let Some(tex) = chat.image(&ImageKey::Emoji(emoji::key(&value))) {
                                    egui::Image::new(&tex).paint_at(ui, r.shrink(5.0));
                                }
                                if response.on_hover_text(if e.zn.is_empty() { &e.n } else { &e.zn }).clicked() {
                                    chosen = Some(value);
                                }
                            }
                        });
                    }
                });
            });
        });
    if let Some(e) = chosen {
        chat_ui.drafts.entry(channel).or_default().push_str(&e);
        chat_ui.picker = false;
    }
    // A click elsewhere closes it.
    if ui.input(|i| i.pointer.any_pressed())
        && let Some(p) = ui.input(|i| i.pointer.interact_pos())
        && !area.response.rect.contains(p)
        && !above.contains(p)
    {
        chat_ui.picker = false;
    }
}
