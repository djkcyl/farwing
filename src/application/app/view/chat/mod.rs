//! The chat page of the device center (`docs/protocol/chat-design.md`
//! §12.1): groups on the left, then channels or direct messages, the
//! messages with the composer, and the members or a panel on the right.
use super::*;
use crate::features::chat::ChatService;
use crate::features::chat::list::Heights;
use crate::features::chat::state::{ChatState, Connection};
use farwing_protocol::chat::{Attachment, Id, Message};
use std::collections::{HashMap, HashSet};

mod composer;
mod dialogs;
mod messages;
mod panels;
pub(super) mod parts;
mod popout;
mod settings;
mod sidebar;
mod stage;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Panel {
    Search,
    Pins,
}

#[derive(Clone, PartialEq)]
pub(super) enum Dialog {
    Create { name: String, description: String },
    Join { text: String },
    Discover { query: String, sent: String },
    Invite { group: Id, uses: u32, expires: usize },
    Me,
    GroupSettings,
    Link { href: String, trust: bool },
    Confirm { title: String, action: Confirmed },
    StartDm,
    Share(ShareDraft),
}

/// The share picker's choices.
#[derive(Clone, PartialEq)]
pub(super) struct ShareDraft {
    /// The windows tab is open (not the screens).
    pub windows: bool,
    pub screens: Vec<crate::media::capture::Screen>,
    pub list: Vec<crate::platform::windows::window_capture::Window>,
    pub listed: std::time::Instant,
    /// The chosen screen (by ID) or window (by handle).
    pub screen: Option<i32>,
    pub window: Option<isize>,
    pub quality: crate::features::host::peer::chat_video::Quality,
    pub audio: bool,
}

impl ShareDraft {
    /// The picker as the share is now (`current`), or for a new one.
    pub(super) fn new(current: Option<&farwing_protocol::chat::VoiceScreen>) -> Self {
        use crate::features::host::peer::chat_video::Quality;
        let screens = crate::platform::windows::capture::screens().unwrap_or_default();
        let primary = screens.iter().find(|s| s.primary).or(screens.first()).map(|s| s.id);
        Self {
            windows: current.is_some_and(|c| !c.whole),
            screens,
            list: crate::platform::windows::window_capture::list(),
            listed: std::time::Instant::now(),
            screen: primary,
            window: None,
            quality: current
                .and_then(|c| Quality::ALL.into_iter().find(|q| q.name() == c.quality))
                .unwrap_or(Quality::P1080F30),
            audio: current.is_none_or(|c| c.audio),
        }
    }
}

/// What a confirmation does when accepted.
#[derive(Clone, PartialEq)]
pub(super) enum Confirmed {
    Delete(Id),
    Leave(Id),
    Dissolve(Id),
    Kick(Id, String),
    DeleteRole(Id, Id),
    DeleteChannel(Id),
}

/// Where the list of a channel is scrolled.
#[derive(Default)]
pub(super) struct Scroll {
    pub channel: Option<Id>,
    pub first: Option<i64>,
    pub at_bottom: bool,
    pub offset: Option<f32>,
    /// The offset last drawn.
    pub last: f32,
    pub width: f32,
    /// What was read when the channel opened (for the "new" line).
    pub read_at_open: Option<Id>,
}

pub(crate) struct ChatUi {
    pub(super) group: Option<Id>,
    pub(super) channel: Option<Id>,
    last: HashMap<Id, Id>,
    drafts: HashMap<Id, String>,
    reply: Option<Message>,
    editing: Option<Message>,
    mention_reply: bool,
    panel: Option<Panel>,
    members: bool,
    heights: HashMap<Id, Heights>,
    scroll: Scroll,
    pub(super) dialog: Option<Dialog>,
    spoilers: HashSet<(Id, usize)>,
    image: Option<Attachment>,
    zoom: f32,
    card: Option<(String, egui::Pos2)>,
    picker: bool,
    picker_query: String,
    picker_tone: usize,
    suggest: Option<composer::Suggest>,
    search_query: String,
    search_files: bool,
    search_scope: u8,
    files: Option<std::sync::mpsc::Receiver<Vec<std::path::PathBuf>>>,
    settings: settings::SettingsUi,
    folded: HashSet<Id>,
    /// The ringing call whose dialog was answered here.
    dismissed_call: Option<String>,
    /// Mute and deafen chosen outside a channel (they hold when joining).
    muted: bool,
    deafened: bool,
    /// A voice participant's card: (channel, participant, where).
    voice_card: Option<(Id, u32, egui::Pos2)>,
    /// The settings dialog's page: chat (0) or voice (1).
    me_tab: u8,
    /// Audio endpoints for the voice settings (inputs, outputs).
    endpoints: Option<(Vec<crate::features::voice::capture::Endpoint>, Vec<crate::features::voice::capture::Endpoint>)>,
    /// The voice settings wait for a push-to-talk key.
    capturing_key: bool,
    /// ...or a mute (1) or deafen (2) key.
    capturing_toggle: u8,
    trusted: HashSet<String>,
    /// Open this channel once it is known (a new DM or group).
    pub(super) open: Option<Id>,
    picker_group: usize,
    /// More search results, after the first page.
    search_more: Vec<Message>,
    search_more_left: bool,
    search_sent: String,
    /// A short note at the bottom (copied, request sent).
    toast: Option<(String, std::time::Instant)>,
    /// The voice channel's pictures.
    stage: stage::StageUi,
}

impl Default for ChatUi {
    fn default() -> Self {
        Self {
            group: None,
            channel: None,
            last: HashMap::new(),
            drafts: HashMap::new(),
            reply: None,
            editing: None,
            mention_reply: true,
            panel: None,
            members: true,
            heights: HashMap::new(),
            scroll: Scroll::default(),
            dialog: None,
            spoilers: HashSet::new(),
            image: None,
            zoom: 1.0,
            card: None,
            picker: false,
            picker_query: String::new(),
            picker_tone: 0,
            suggest: None,
            search_query: String::new(),
            search_files: false,
            search_scope: 0,
            files: None,
            settings: settings::SettingsUi::default(),
            folded: HashSet::new(),
            dismissed_call: None,
            muted: false,
            deafened: false,
            voice_card: None,
            me_tab: 0,
            endpoints: None,
            capturing_key: false,
            capturing_toggle: 0,
            trusted: HashSet::new(),
            open: None,
            picker_group: 0,
            search_more: Vec::new(),
            search_more_left: false,
            search_sent: String::new(),
            toast: None,
            stage: stage::StageUi::default(),
        }
    }
}

impl ChatUi {
    pub(super) fn select_group(&mut self, group: Option<Id>, state: &ChatState) {
        self.group = group;
        self.channel = match group {
            Some(g) => self.last.get(&g).copied().or_else(|| {
                state.group(g).and_then(|gs| {
                    let ordered = crate::features::chat::state::ordered(&gs.channels);
                    ordered
                        .iter()
                        .find(|c| c.kind == farwing_protocol::chat::ChannelKind::Text)
                        .or_else(|| ordered.iter().find(|c| c.kind.has_messages() && c.kind != farwing_protocol::chat::ChannelKind::Voice))
                        .map(|c| c.id)
                })
            }),
            None => None,
        };
        self.reply = None;
        self.editing = None;
    }

    pub(super) fn select_channel(&mut self, channel: Id) {
        if let Some(g) = self.group {
            self.last.insert(g, channel);
        }
        self.channel = Some(channel);
        self.reply = None;
        self.editing = None;
        self.suggest = None;
    }

}

pub(super) const RAIL: f32 = 66.0;
pub(super) const SIDE: f32 = 232.0;
pub(super) const RIGHT: f32 = 232.0;
pub(super) const PANEL: f32 = 340.0;
pub(super) const SIDE_BG: Color32 = Color32::from_rgb(25, 30, 38);

impl DeviceCenterApp {
    /// Every frame: chat's notifications, and no focused channel while
    /// another page is shown.
    pub(crate) fn tick_chat(&mut self) {
        let Some(chat) = &self.chat else {
            crate::ui::tray::set_voice(false, false, false, false);
            return;
        };
        let view = chat.view();
        let voice = view.my_voice.clone();
        crate::ui::tray::set_voice(
            voice.is_some(),
            voice.as_ref().is_some_and(|v| v.muted),
            voice.as_ref().is_some_and(|v| v.deafened),
            voice.as_ref().is_some_and(|v| v.call.is_some()),
        );
        if let Some(v) = &voice {
            match crate::ui::tray::take_voice_action() {
                1 => chat.set_voice_self(!v.muted, v.deafened),
                2 => chat.set_voice_self(v.muted, !v.deafened),
                3 => chat.leave_voice(),
                _ => {}
            }
        }
        chat.tick_sounds(&view);
        if self.center_ui.page != super::Page::Chat {
            chat.set_focus(None);
        }
        for notice in chat.take_notices() {
            if notice.message {
                chat.play_sound(crate::features::voice::sounds::Sound::Message);
            }
            self.notifications.show_chat(notice.channel.0, notice.title, notice.body);
        }
    }

    /// Show a channel (a chat notification's "view").
    pub(crate) fn open_chat(&mut self, channel: Id) {
        self.center_ui.page = super::Page::Chat;
        self.chat_ui.open = Some(channel);
    }

    pub(super) fn chat_page(&mut self, ui: &mut egui::Ui) {
        let Some(chat) = self.chat.clone() else {
            ui.centered_and_justified(|ui| ui.label(parts::muted(t().chat.connecting)));
            return;
        };
        if ui.input(|i| !i.events.is_empty() || i.pointer.is_moving()) {
            chat.activity();
        }
        for error in chat.take_errors() {
            self.status = StatusMessage::warning(error);
        }
        let state = chat.view();
        let ui_state = &mut self.chat_ui;
        if let Some(open) = ui_state.open
            && let Some((_, group, _)) = state.channel(open)
        {
            ui_state.group = group.map(|g| g.group.id);
            ui_state.select_channel(open);
            ui_state.open = None;
        }
        // A channel that is gone (left the group, deleted) is not shown.
        if let Some(c) = ui_state.channel
            && state.snap.is_some()
            && state.channel(c).is_none()
            && ui_state.open.is_none()
        {
            ui_state.channel = None;
        }
        if let Some(g) = ui_state.group
            && state.snap.is_some()
            && state.group(g).is_none()
        {
            ui_state.group = None;
            ui_state.channel = None;
        }
        if ui_state.group.is_some() && ui_state.channel.is_none() {
            let g = ui_state.group;
            ui_state.select_group(g, &state);
        }
        chat.set_focus(ui_state.channel);
        chat.set_hidden(!ui.ctx().input(|i| i.viewport().focused.unwrap_or(true)));

        let full = ui.max_rect();
        ui.painter().rect_filled(full, 0.0, BG);
        if state.snap.is_none() {
            ui.scope_builder(egui::UiBuilder::new().max_rect(full), |ui| {
                ui.centered_and_justified(|ui| {
                    if state.connection == Connection::Elsewhere {
                        ui.vertical_centered(|ui| {
                            ui.label(parts::muted(t().chat.elsewhere));
                            if ui.button(t().chat.use_here).clicked() {
                                chat.take_over();
                            }
                        });
                    } else {
                        ui.label(parts::muted(t().chat.connecting));
                    }
                });
            });
            return;
        }
        let rail = egui::Rect::from_min_max(full.min, egui::pos2(full.left() + RAIL, full.bottom()));
        let side = egui::Rect::from_min_max(egui::pos2(rail.right(), full.top()), egui::pos2(rail.right() + SIDE, full.bottom()));
        let show_members = self.chat_ui.panel.is_none() && self.chat_ui.members && self.chat_ui.group.is_some();
        let right_w = if self.chat_ui.panel.is_some() {
            PANEL
        } else if show_members {
            RIGHT
        } else {
            0.0
        };
        let main = egui::Rect::from_min_max(egui::pos2(side.right(), full.top()), egui::pos2(full.right() - right_w, full.bottom()));
        let right = egui::Rect::from_min_max(egui::pos2(main.right(), full.top()), full.max);

        ui.painter().rect_filled(rail, 0.0, SIDEBAR);
        ui.painter().rect_filled(side, 0.0, SIDE_BG);
        ui.painter().vline(rail.right(), rail.y_range(), Stroke::new(1.0, LINE));
        ui.painter().vline(side.right(), side.y_range(), Stroke::new(1.0, LINE));
        ui.scope_builder(egui::UiBuilder::new().max_rect(rail).id_salt("chat-rail"), |ui| {
            sidebar::rail(ui, &chat, &state, &mut self.chat_ui);
        });
        ui.scope_builder(egui::UiBuilder::new().max_rect(side).id_salt("chat-side"), |ui| {
            sidebar::side(ui, &chat, &state, &mut self.chat_ui);
        });
        ui.scope_builder(egui::UiBuilder::new().max_rect(main).id_salt("chat-main"), |ui| {
            self.chat_main(ui, &chat, &state);
        });
        if right_w > 0.0 {
            ui.painter().rect_filled(right, 0.0, SIDE_BG);
            ui.painter().vline(right.left(), right.y_range(), Stroke::new(1.0, LINE));
            ui.scope_builder(egui::UiBuilder::new().max_rect(right).id_salt("chat-right"), |ui| match self.chat_ui.panel {
                Some(Panel::Search) => panels::search(ui, &chat, &state, &mut self.chat_ui),
                Some(Panel::Pins) => panels::pins(ui, &chat, &state, &mut self.chat_ui),
                None => panels::members(ui, &chat, &state, &mut self.chat_ui),
            });
        }
        panels::user_card(ui, &chat, &state, &mut self.chat_ui);
        panels::voice_card(ui, &chat, &state, &mut self.chat_ui);
        dialogs::show(ui.ctx(), &chat, &state, &mut self.chat_ui);
        settings::show(ui.ctx(), &chat, &state, &mut self.chat_ui);
        dialogs::image_viewer(ui.ctx(), &chat, &mut self.chat_ui);
        if self.chat_ui.stage.theater {
            stage::theater(ui.ctx(), &chat, &state, &mut self.chat_ui);
        }
        composer::poll_files(&chat, &mut self.chat_ui);
        // Files dropped on the window go to the open channel.
        let dropped: Vec<std::path::PathBuf> = ui
            .ctx()
            .input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        if !dropped.is_empty()
            && let Some(channel) = self.chat_ui.channel
        {
            composer::attach_paths(&chat, channel, dropped);
        }
        if let Some((text, at)) = &self.chat_ui.toast {
            if at.elapsed() > std::time::Duration::from_millis(2500) {
                self.chat_ui.toast = None;
            } else {
                let galley = ui.painter().layout_no_wrap(text.clone(), egui::FontId::proportional(13.0), TEXT);
                let size = galley.size() + vec2(28.0, 14.0);
                let rect = egui::Rect::from_center_size(egui::pos2(full.center().x, full.bottom() - 90.0), size);
                let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("chat-toast")));
                painter.rect(rect, 8.0, SURFACE, Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
                painter.galley(rect.center() - galley.size() / 2.0, galley, TEXT);
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
            }
        }
    }

    fn chat_main(&mut self, ui: &mut egui::Ui, chat: &ChatService, state: &ChatState) {
        let rect = ui.max_rect();
        let Some(channel_id) = self.chat_ui.channel else {
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                ui.centered_and_justified(|ui| {
                    let empty = self.chat_ui.group.is_none() && state.snap.as_ref().is_some_and(|s| s.dms.is_empty());
                    ui.label(parts::muted(if empty { t().chat.no_dms } else { t().chat.pick_channel }));
                });
            });
            return;
        };
        let Some((channel, group, dm)) = state.channel(channel_id) else { return };
        let (channel, group, dm) = (channel.clone(), group.cloned(), dm.cloned());
        let head = egui::Rect::from_min_size(rect.min, vec2(rect.width(), 48.0));
        let mut body = egui::Rect::from_min_max(egui::pos2(rect.left(), head.bottom()), rect.max);
        ui.painter().hline(rect.x_range(), head.bottom(), Stroke::new(1.0, LINE));
        if state.connection != Connection::Ready {
            let banner = egui::Rect::from_min_size(body.min, vec2(body.width(), 26.0));
            ui.painter().rect_filled(banner, 0.0, Color32::from_rgb(74, 58, 28));
            ui.painter().text(
                banner.left_center() + vec2(16.0, 0.0),
                egui::Align2::LEFT_CENTER,
                if state.connection == Connection::Elsewhere { t().chat.elsewhere } else { t().chat.offline },
                egui::FontId::proportional(12.5),
                AMBER,
            );
            body.min.y = banner.bottom();
        }
        ui.scope_builder(egui::UiBuilder::new().max_rect(head.shrink2(vec2(14.0, 0.0))).layout(egui::Layout::left_to_right(Align::Center)), |ui| {
            self.chat_header(ui, chat, state, &channel, group.as_ref(), dm.as_ref());
        });
        // A call in this direct message: its bar over the messages.
        if dm.is_some()
            && let Some(call) = state.snap.as_ref().and_then(|s| s.calls.iter().find(|c| c.channel_id == channel_id)).cloned()
        {
            let strip = egui::Rect::from_min_size(body.min, vec2(body.width(), 44.0));
            ui.painter().rect_filled(strip, 0.0, Color32::from_rgb(28, 45, 40));
            ui.scope_builder(
                egui::UiBuilder::new().max_rect(strip.shrink2(vec2(16.0, 6.0))).layout(egui::Layout::left_to_right(Align::Center)),
                |ui| self.call_bar(ui, chat, state, &call),
            );
            body.min.y = strip.bottom();
        }
        // The voice channel's or call's pictures over its messages.
        let (sources, call) = stage::sources(chat, state, channel_id);
        let stage_h = stage::height(&self.chat_ui, &sources, body.width(), body.height());
        if stage_h > 0.0 {
            let area = egui::Rect::from_min_size(body.min, vec2(body.width(), stage_h));
            ui.scope_builder(egui::UiBuilder::new().max_rect(area).id_salt("chat-stage"), |ui| {
                stage::stage(ui, chat, state, &mut self.chat_ui, &sources, call);
            });
            body.min.y = area.bottom();
        }
        let composer_h = composer::height(ui, &self.chat_ui, chat, channel_id);
        let list = egui::Rect::from_min_max(body.min, egui::pos2(body.right(), body.bottom() - composer_h));
        let input = egui::Rect::from_min_max(egui::pos2(body.left(), list.bottom()), body.max);
        ui.scope_builder(egui::UiBuilder::new().max_rect(list).id_salt(("chat-list", channel_id)), |ui| {
            messages::list(ui, chat, state, &mut self.chat_ui, &channel, group.as_ref(), dm.as_ref());
        });
        ui.scope_builder(egui::UiBuilder::new().max_rect(input).id_salt("chat-composer"), |ui| {
            composer::composer(ui, chat, state, &mut self.chat_ui, &channel, group.as_ref(), dm.as_ref());
        });
    }

    /// A call's state in its direct message, and what can be done.
    fn call_bar(&mut self, ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, call: &farwing_protocol::chat::Call) {
        use parts::{Glyph, glyph_button};
        let v = &t().voice;
        let me = state.me().map(|u| u.user_id.clone()).unwrap_or_default();
        let here = state.my_voice.as_ref().is_some_and(|m| m.call.as_deref() == Some(call.id.as_str()));
        let (text, color) = if call.state == "ringing" {
            if call.caller.user_id == me {
                (v.calling.to_owned(), crate::ui::theme::AMBER)
            } else {
                ((v.incoming)(call.caller.display_name()), crate::ui::theme::AMBER)
            }
        } else if here {
            let seconds = call.answered_at.map_or(0, |at| ((chrono::Utc::now().timestamp_millis() - at).max(0) / 1000) as u64);
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            (format!("{} · {:02}:{:02}", v.in_call, seconds / 60, seconds % 60), crate::ui::theme::GREEN)
        } else {
            (v.in_call_elsewhere.to_owned(), MUTED)
        };
        let (rect, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
        parts::paint_glyph(ui.painter(), rect, Glyph::Phone, color);
        ui.add_space(6.0);
        ui.label(RichText::new(text).strong().color(color));
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            if here || (call.state == "ringing" && call.caller.user_id == me) {
                if glyph_button(ui, Glyph::PhoneDown, v.hang_up, false).clicked() {
                    chat.leave_voice();
                }
                if let Some(mine) = state.my_voice.as_ref().filter(|_| here) {
                    if mine.connected {
                        let hint = if mine.screen.is_some() { v.share_settings } else { v.share };
                        if glyph_button(ui, Glyph::Screen, hint, mine.screen.is_some()).clicked() {
                            self.chat_ui.dialog = Some(Dialog::Share(ShareDraft::new(mine.screen.as_ref())));
                        }
                        let (glyph, hint) = if mine.camera { (Glyph::Camera, v.camera_off) } else { (Glyph::CameraOff, v.camera_on) };
                        if glyph_button(ui, glyph, hint, mine.camera).clicked() {
                            chat.set_camera(!mine.camera);
                        }
                    }
                    let (muted, deafened) = (mine.muted, mine.deafened);
                    let (glyph, hint) = if deafened { (Glyph::HeadsetOff, v.undeafen) } else { (Glyph::Headset, v.deafen) };
                    if glyph_button(ui, glyph, hint, deafened).clicked() {
                        chat.set_voice_self(muted, !deafened);
                    }
                    let (glyph, hint) = if muted { (Glyph::MicOff, v.unmute) } else { (Glyph::Mic, v.mute) };
                    if glyph_button(ui, glyph, hint, muted).clicked() {
                        chat.set_voice_self(!muted, deafened);
                    }
                }
            } else if call.state == "ringing" {
                if ui.button(v.decline).clicked() {
                    chat.decline_call(&call.id);
                }
                if ui.add(crate::ui::controls::primary(v.answer)).clicked() {
                    chat.answer_call(&call.id, false);
                }
            } else if ui.button(v.take_here).clicked() {
                chat.answer_call(&call.id, true);
            }
        });
    }

    /// Over every page: a call ringing for this account.
    pub(crate) fn chat_overlays(&mut self, ctx: &egui::Context) {
        let Some(chat) = self.chat.clone() else { return };
        let state = chat.view();
        let me = state.me().map(|u| u.user_id.clone()).unwrap_or_default();
        self.control_overlays(ctx, &chat, &state);
        if state.do_not_disturb() {
            return;
        }
        let ringing = state
            .snap
            .as_ref()
            .and_then(|s| s.calls.iter().find(|c| c.state == "ringing" && c.callee.user_id == me).cloned());
        let Some(call) = ringing else { return };
        if self.chat_ui.dismissed_call.as_deref() == Some(call.id.as_str()) {
            return;
        }
        let v = &t().voice;
        let mut open_chat = false;
        egui::Modal::new(egui::Id::new("chat-incoming-call")).frame(crate::ui::controls::dialog_frame()).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                parts::avatar(ui, &chat, &call.caller, 64.0, None);
                ui.add_space(8.0);
                ui.label(RichText::new((v.incoming)(call.caller.display_name())).size(16.0).strong().color(TEXT));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let width = (ui.available_width() - 8.0) / 2.0;
                    if ui.add_sized([width, 32.0], egui::Button::new(v.decline)).clicked() {
                        chat.decline_call(&call.id);
                    }
                    if ui.add_sized([width, 32.0], crate::ui::controls::primary(v.answer)).clicked() {
                        chat.answer_call(&call.id, false);
                        open_chat = true;
                    }
                });
            });
        });
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
        if open_chat {
            self.chat_ui.dismissed_call = Some(call.id.clone());
            self.open_chat(call.channel_id);
        }
    }

    /// A request to control this device's share, and a session the share's
    /// sharer allowed.
    fn control_overlays(&mut self, ctx: &egui::Context, chat: &ChatService, state: &ChatState) {
        if let Some(device) = chat.take_open_device() {
            let name = state
                .snap
                .as_ref()
                .and_then(|s| s.voice.iter().flat_map(|v| &v.participants).find(|p| p.device_id == device))
                .and_then(|p| state.members.values().flatten().find(|m| m.user.user_id == p.user_id))
                .map(|m| parts::display_name(&m.user, &m.nickname))
                .unwrap_or_else(|| crate::account::api::device_id_text(&device));
            self.spawn_viewer(name, Some(device), None, false);
        }
        let Some(request) = state.control_asks.first().cloned() else { return };
        let v = &t().voice;
        let host_off = self.host.is_none();
        let mut clipboard = ctx.data(|d| d.get_temp::<bool>(egui::Id::new("chat-control-clipboard"))).unwrap_or(false);
        egui::Modal::new(egui::Id::new("chat-control-request")).frame(crate::ui::controls::dialog_frame()).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                parts::avatar(ui, chat, &request.from, 56.0, None);
                ui.add_space(8.0);
                ui.label(RichText::new((v.control_ask)(request.from.display_name())).size(15.0).strong().color(TEXT));
                ui.add_space(8.0);
            });
            ui.horizontal(|ui| {
                crate::ui::controls::switch(ui, &mut clipboard);
                ui.label(v.allow_clipboard);
            });
            if host_off {
                ui.label(parts::muted(v.host_off));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 8.0) / 2.0;
                if ui.add_sized([width, 32.0], egui::Button::new(v.deny)).clicked() {
                    chat.answer_control(&request.id, false, false);
                }
                if ui
                    .add_enabled_ui(!host_off, |ui| ui.add_sized([width, 32.0], crate::ui::controls::primary(v.allow)))
                    .inner
                    .clicked()
                {
                    chat.answer_control(&request.id, true, clipboard);
                }
            });
        });
        ctx.data_mut(|d| d.insert_temp(egui::Id::new("chat-control-clipboard"), clipboard));
    }

    /// Connect to a device a friend shares (from a direct message).
    fn connect_shared_device(&mut self, device: DeviceInfo) {
        self.selected_device_id = Some(device.device_id);
        self.start_viewer();
    }

    fn chat_header(
        &mut self,
        ui: &mut egui::Ui,
        chat: &ChatService,
        state: &ChatState,
        channel: &farwing_protocol::chat::Channel,
        group: Option<&farwing_protocol::chat::GroupState>,
        dm: Option<&farwing_protocol::chat::Dm>,
    ) {
        use farwing_protocol::chat::ChannelKind;
        use parts::{Glyph, glyph_button};
        if let Some(dm) = dm {
            let status = state.presences.get(&dm.user.user_id).copied();
            parts::avatar(ui, chat, &dm.user, 26.0, status);
            ui.add_space(6.0);
            ui.label(RichText::new(parts::display_name(&dm.user, "")).strong().color(TEXT));
            let shared: Vec<DeviceInfo> = self
                .devices
                .as_ref()
                .map(|l| {
                    l.shared
                        .iter()
                        .filter(|d| d.owner.as_ref().is_some_and(|o| o.user_id == dm.user.user_id))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            for device in shared.iter().take(4) {
                ui.add_space(6.0);
                let label = if device.alias.is_empty() { device.device_id.clone() } else { device.alias.clone() };
                let response = ui
                    .add_enabled(device.status == "online", egui::Button::new(RichText::new(label).size(12.0)).small())
                    .on_hover_text(t().chat.shared_devices);
                if response.clicked() {
                    self.connect_shared_device(device.clone());
                }
            }
        } else {
            let (rect, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
            let glyph = match channel.kind {
                ChannelKind::Voice => Glyph::Speaker,
                ChannelKind::Announcement => Glyph::Megaphone,
                _ => Glyph::Hash,
            };
            parts::paint_glyph(ui.painter(), rect, glyph, MUTED);
            ui.add_space(4.0);
            ui.label(RichText::new(&channel.name).strong().color(TEXT));
            if !channel.topic.is_empty() {
                ui.add_space(8.0);
                ui.label(parts::muted(&channel.topic));
            }
        }
        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
            let c = &t().chat;
            if let Some(dm) = dm
                && dm.friends
                && state.snap.as_ref().is_some_and(|s| !s.calls.iter().any(|x| x.channel_id == channel.id))
                && glyph_button(ui, Glyph::Phone, t().voice.call, false).clicked()
            {
                chat.start_call(channel.id);
            }
            if glyph_button(ui, Glyph::Search, c.search, self.chat_ui.panel == Some(Panel::Search)).clicked() {
                self.chat_ui.panel = if self.chat_ui.panel == Some(Panel::Search) { None } else { Some(Panel::Search) };
            }
            if group.is_some() && glyph_button(ui, Glyph::People, c.members, self.chat_ui.members && self.chat_ui.panel.is_none()).clicked() {
                if self.chat_ui.panel.is_some() {
                    self.chat_ui.panel = None;
                    self.chat_ui.members = true;
                } else {
                    self.chat_ui.members = !self.chat_ui.members;
                }
            }
            let notify = glyph_button(ui, Glyph::Bell, c.notify, false);
            egui::Popup::menu(&notify).show(|ui| {
                panels::notify_menu(ui, chat, state, channel, group);
            });
            if glyph_button(ui, Glyph::Pin, c.pins, self.chat_ui.panel == Some(Panel::Pins)).clicked() {
                self.chat_ui.panel = if self.chat_ui.panel == Some(Panel::Pins) { None } else { Some(Panel::Pins) };
            }
        });
    }
}
