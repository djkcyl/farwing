//! The pictures of the voice channel or call this device is in, above its
//! messages (`docs/protocol/chat-design.md` §8, §7.6): cameras and screens
//! in a grid, one of them large when chosen. In a channel the grid shows
//! each camera's small picture, a chosen camera its large one, and a screen
//! is received only once opened (at 720p30 if asked for); the picture shown
//! before a choice stays two seconds, so the new one starts without a gap.
//! In a call everything the other device sends comes anyway.

use super::ChatUi;
use super::parts::{self, Glyph};
use crate::features::chat::ChatService;
use crate::features::chat::state::ChatState;
use crate::i18n::t;
use crate::ui::theme::{LINE, MUTED, SURFACE, TEXT};
use egui::{Align, Color32, Rect, RichText, Sense, Stroke, vec2};
use farwing_protocol::chat::{Id, VoiceScreen, track};
use std::time::{Duration, Instant};

/// How long the picture shown before a choice is still received.
const KEEP: Duration = Duration::from_secs(2);
const TILE: egui::Vec2 = vec2(224.0, 126.0);
/// The other device of a call.
const PEER: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Kind {
    Camera,
    Screen,
}

#[derive(Default)]
pub(super) struct StageUi {
    /// The chosen picture: (participant, kind).
    focus: Option<(u32, Kind)>,
    /// The one chosen before, until when it is still received.
    previous: Option<((u32, Kind), Instant)>,
    /// A chosen screen at 720p30.
    low: bool,
    /// The chosen picture fills the screen.
    pub(super) theater: bool,
    /// The tracks last asked for, by this device's participant number
    /// (asked again in a new voice session).
    watched: (u32, Vec<(u32, String)>),
}

impl StageUi {
    fn choose(&mut self, next: Option<(u32, Kind)>) {
        if let Some(focus) = self.focus {
            self.previous = Some((focus, Instant::now() + KEEP));
        }
        self.focus = next;
        self.low = false;
    }
}

/// A picture someone shows.
#[derive(Clone)]
pub(super) struct Source {
    participant: u32,
    kind: Kind,
    name: String,
    user_id: String,
    device_id: String,
    /// A channel share as its sender describes it.
    screen: Option<VoiceScreen>,
    mine: bool,
}

/// The pictures shown in `channel` (a voice channel or a call's direct
/// message) while this device is in it; true for a call.
pub(super) fn sources(chat: &ChatService, state: &ChatState, channel: Id) -> (Vec<Source>, bool) {
    let Some(my) = state.my_voice.as_ref().filter(|m| m.channel == channel) else {
        return (Vec::new(), false);
    };
    let you = t().voice.you.to_owned();
    let mut out = Vec::new();
    if my.call.is_some() {
        let peer = state.snap.as_ref().and_then(|s| s.dms.iter().find(|d| d.channel.id == channel)).map(|d| d.user.clone());
        let peer_name = peer.as_ref().map(|u| parts::display_name(u, "")).unwrap_or_default();
        let peer_id = peer.map(|u| u.user_id).unwrap_or_default();
        for (kind, sent) in [(Kind::Camera, track::CAM_HI), (Kind::Screen, track::SCREEN)] {
            if chat.picture(PEER, sent).is_some_and(|p| p.fresh()) {
                out.push(Source {
                    participant: PEER,
                    kind,
                    name: peer_name.clone(),
                    user_id: peer_id.clone(),
                    device_id: String::new(),
                    screen: None,
                    mine: false,
                });
            }
        }
        for (kind, on) in [(Kind::Camera, my.camera), (Kind::Screen, my.screen.is_some())] {
            if on {
                out.push(Source {
                    participant: 0,
                    kind,
                    name: you.clone(),
                    user_id: String::new(),
                    device_id: String::new(),
                    screen: None,
                    mine: true,
                });
            }
        }
        return (out, true);
    }
    let Some(here) = state.snap.as_ref().and_then(|s| s.voice.iter().find(|v| v.channel_id == channel)) else {
        return (Vec::new(), false);
    };
    let group = state.channel(channel).and_then(|(_, g, _)| g.map(|g| g.group.id));
    for person in &here.participants {
        let mine = my.participant == person.participant;
        let name = if mine {
            you.clone()
        } else {
            group
                .and_then(|g| state.members.get(&g))
                .and_then(|members| members.iter().find(|m| m.user.user_id == person.user_id))
                .map(|m| parts::display_name(&m.user, &m.nickname))
                .unwrap_or_default()
        };
        let source = |kind| Source {
            participant: person.participant,
            kind,
            name: name.clone(),
            user_id: person.user_id.clone(),
            device_id: person.device_id.clone(),
            screen: person.screen.clone(),
            mine,
        };
        if person.tracks.iter().any(|k| k == track::CAM_HI) || (mine && my.camera) {
            out.push(source(Kind::Camera));
        }
        if person.tracks.iter().any(|k| k == track::SCREEN) {
            out.push(source(Kind::Screen));
        }
    }
    (out, false)
}

/// The stage's height for `count` pictures in a message area of
/// `available` height.
pub(super) fn height(chat_ui: &ChatUi, count: usize, width: f32, available: f32) -> f32 {
    if count == 0 {
        return 0.0;
    }
    if chat_ui.stage.focus.is_some() {
        return (available * 0.62).max(240.0).min(available - 120.0).max(0.0);
    }
    let per_row = (((width - 16.0) / (TILE.x + 8.0)).floor() as usize).max(1);
    let rows = count.div_ceil(per_row);
    (rows as f32 * (TILE.y + 8.0) + 16.0).min(available * 0.45)
}

/// Keeps what this device receives from the forwarder in step with what
/// it shows.
fn watch(chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi, all: &[Source]) {
    let me = state.my_voice.as_ref().map(|m| m.participant);
    let stage = &mut chat_ui.stage;
    let mut refs: Vec<(u32, String)> = Vec::new();
    for s in all.iter().filter(|s| !s.mine) {
        let p = s.participant;
        let big = stage.focus == Some((p, s.kind));
        let was = stage.previous.is_some_and(|(f, _)| f == (p, s.kind));
        match s.kind {
            Kind::Camera => {
                refs.push((p, if big { track::CAM_HI } else { track::CAM_LO }.to_owned()));
                if was && !big {
                    refs.push((p, track::CAM_HI.to_owned()));
                }
            }
            Kind::Screen if big || was => {
                refs.push((p, if stage.low && big { track::SCREEN_720 } else { track::SCREEN }.to_owned()));
                if s.screen.as_ref().is_none_or(|sc| sc.audio) {
                    refs.push((p, track::SCREEN_AUDIO.to_owned()));
                }
            }
            Kind::Screen => {}
        }
    }
    refs.sort();
    refs.dedup();
    let key = (me.unwrap_or(0), refs);
    if key != stage.watched {
        stage.watched = key.clone();
        chat.watch(key.1);
    }
}

/// The kinds to show for `source`, best first.
fn kinds(source: &Source, big: bool, low: bool, call: bool) -> [&'static str; 2] {
    match source.kind {
        Kind::Camera if big || call => [track::CAM_HI, track::CAM_LO],
        Kind::Camera => [track::CAM_LO, track::CAM_HI],
        Kind::Screen if low => [track::SCREEN_720, track::SCREEN],
        Kind::Screen => [track::SCREEN, track::SCREEN_720],
    }
}

/// Paints `source`'s newest picture fitted in `rect`; false when there is
/// none yet.
fn paint_picture(ui: &egui::Ui, chat: &ChatService, source: &Source, rect: Rect, big: bool, low: bool, call: bool) -> bool {
    for kind in kinds(source, big, low, call) {
        let Some(picture) = chat.picture(source.participant, kind) else { continue };
        let Some(id) = crate::ui::video_textures::texture(picture.handle) else {
            ui.ctx().request_repaint();
            return false;
        };
        let size = vec2(picture.width.max(1) as f32, picture.height.max(1) as f32);
        let scale = (rect.width() / size.x).min(rect.height() / size.y);
        let fitted = Rect::from_center_size(rect.center(), size * scale);
        ui.painter().image(id, fitted, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        return true;
    }
    false
}

pub(super) fn stage(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi, all: &[Source], call: bool) {
    if chat_ui.stage.previous.is_some_and(|(_, until)| Instant::now() >= until) {
        chat_ui.stage.previous = None;
    }
    if !call {
        watch(chat, state, chat_ui, all);
    }
    let rect = ui.max_rect();
    ui.painter().rect_filled(rect, 0.0, Color32::from_rgb(12, 14, 18));
    ui.painter().hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, LINE));
    let focused = chat_ui
        .stage
        .focus
        .and_then(|(p, kind)| all.iter().position(|s| s.participant == p && s.kind == kind));
    if chat_ui.stage.focus.is_some() && focused.is_none() {
        chat_ui.stage.choose(None);
    }
    ui.ctx().request_repaint_after(Duration::from_millis(33));
    if let Some(index) = focused {
        large(ui, chat, state, chat_ui, &all[index], rect, call);
        return;
    }
    let per_row = (((rect.width() - 16.0) / (TILE.x + 8.0)).floor() as usize).clamp(1, all.len().max(1));
    // The grid in the middle.
    let left = ((rect.width() - per_row as f32 * (TILE.x + 8.0) + 8.0) / 2.0).max(8.0);
    for (i, s) in all.iter().enumerate() {
        let (row, col) = (i / per_row, i % per_row);
        let tile = Rect::from_min_size(rect.min + vec2(left + col as f32 * (TILE.x + 8.0), 8.0 + row as f32 * (TILE.y + 8.0)), TILE);
        if !rect.intersects(tile) {
            continue;
        }
        let response = ui.interact(tile, ui.id().with(("stage-tile", s.participant, s.kind, s.mine)), Sense::click());
        ui.painter().rect_filled(tile, 6.0, SURFACE);
        // A channel's screens come once opened; a call's come anyway.
        let shown = !s.mine && (s.kind == Kind::Camera || call) && paint_picture(ui, chat, s, tile.shrink(1.0), false, false, call);
        if !shown {
            let glyph = if s.kind == Kind::Screen { Glyph::Screen } else { Glyph::Camera };
            let icon = Rect::from_center_size(tile.center() - vec2(0.0, 10.0), vec2(26.0, 26.0));
            parts::paint_glyph(ui.painter(), icon, glyph, MUTED);
            if s.kind == Kind::Screen && !s.mine && !call {
                ui.painter().text(
                    tile.center() + vec2(0.0, 16.0),
                    egui::Align2::CENTER_CENTER,
                    t().voice.watch,
                    egui::FontId::proportional(12.5),
                    TEXT,
                );
            }
        }
        if response.hovered() {
            ui.painter().rect_stroke(tile, 6.0, Stroke::new(1.5, crate::ui::theme::ACCENT), egui::StrokeKind::Inside);
        }
        name_line(ui, tile, &s.name, s.kind == Kind::Screen);
        if response.clicked() && !s.mine {
            chat_ui.stage.choose(Some((s.participant, s.kind)));
        }
    }
}

/// A tile's name, live first.
fn name_line(ui: &egui::Ui, tile: Rect, name: &str, live: bool) {
    let y = tile.bottom() - 15.0;
    let mut x = tile.left() + 8.0;
    if live {
        let width = parts::live_width(ui.painter());
        parts::paint_live(ui.painter(), egui::pos2(x + width, y));
        x += width + 8.0;
    }
    if name.is_empty() {
        return;
    }
    let galley = ui.painter().layout_no_wrap(name.to_owned(), egui::FontId::proportional(12.5), Color32::WHITE);
    let back = Rect::from_min_size(egui::pos2(x - 4.0, y - 10.0), vec2(galley.size().x + 8.0, 20.0));
    ui.painter().rect_filled(back, 4.0, Color32::from_black_alpha(150));
    ui.painter().galley(egui::pos2(x, y - galley.size().y / 2.0), galley, Color32::WHITE);
}

/// The chosen picture, large, with its bar.
fn large(ui: &mut egui::Ui, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi, s: &Source, rect: Rect, call: bool) {
    let v = &t().voice;
    let bar = Rect::from_min_size(egui::pos2(rect.left(), rect.bottom() - 44.0), vec2(rect.width(), 44.0));
    let picture_rect = Rect::from_min_max(rect.min, egui::pos2(rect.right(), bar.top()));
    if !paint_picture(ui, chat, s, picture_rect, true, chat_ui.stage.low, call) {
        ui.put(Rect::from_center_size(picture_rect.center(), vec2(24.0, 24.0)), egui::Spinner::new());
    }
    if s.kind == Kind::Screen && s.screen.as_ref().is_some_and(|sc| sc.minimized) {
        let galley = ui.painter().layout_no_wrap(v.minimized.to_owned(), egui::FontId::proportional(13.0), Color32::WHITE);
        let note = Rect::from_center_size(egui::pos2(picture_rect.center().x, picture_rect.top() + 24.0), galley.size() + vec2(20.0, 10.0));
        ui.painter().rect_filled(note, 6.0, Color32::from_black_alpha(170));
        ui.painter().galley(note.center() - galley.size() / 2.0, galley, Color32::WHITE);
    }
    ui.painter().rect_filled(bar, 0.0, Color32::from_rgb(20, 23, 29));
    ui.scope_builder(
        egui::UiBuilder::new().max_rect(bar.shrink2(vec2(10.0, 6.0))).layout(egui::Layout::left_to_right(Align::Center)),
        |ui| {
            if ui.add(egui::Button::new(v.back)).clicked() {
                chat_ui.stage.choose(None);
                if chat_ui.stage.theater {
                    chat_ui.stage.theater = false;
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                }
            }
            ui.add_space(6.0);
            if s.kind == Kind::Screen {
                let width = parts::live_width(ui.painter());
                let (live, _) = ui.allocate_exact_size(vec2(width, 16.0), Sense::hover());
                parts::paint_live(ui.painter(), live.right_center());
            }
            ui.add(egui::Label::new(RichText::new(&s.name).color(TEXT)).truncate());
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let (label, on) = if chat_ui.stage.theater { (v.exit_fullscreen, true) } else { (v.fullscreen, false) };
                if ui.add(egui::Button::new(label)).clicked() {
                    chat_ui.stage.theater = !on;
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(!on));
                }
                if s.kind != Kind::Screen {
                    return;
                }
                // Control is asked for a channel's whole-screen share.
                if !call && s.screen.as_ref().is_some_and(|sc| sc.whole) {
                    let control = state.control.as_ref().filter(|c| c.sharer_device == s.device_id);
                    match control {
                        Some(c) if c.allowed => {
                            if ui.button(v.give_back).clicked() {
                                chat.end_control();
                            }
                        }
                        Some(_) => {
                            ui.add_enabled(false, egui::Button::new(v.control_asked));
                        }
                        None => {
                            if ui.button(v.request_control).clicked() {
                                chat.request_control(&s.device_id);
                            }
                        }
                    }
                }
                if !call && s.screen.as_ref().is_none_or(|sc| sc.quality != "720p30") {
                    ui.selectable_value(&mut chat_ui.stage.low, true, "720p30");
                    ui.selectable_value(&mut chat_ui.stage.low, false, v.original);
                }
                if s.screen.as_ref().is_none_or(|sc| sc.audio) {
                    let mut settings = chat.voice_settings();
                    let before = settings.clone();
                    let volume = settings.share_volumes.entry(s.user_id.clone()).or_insert(100);
                    let mut value = u32::from(*volume);
                    ui.add(egui::Slider::new(&mut value, 0..=200).show_value(false)).on_hover_text(v.share_volume);
                    *volume = value as u8;
                    if *volume == 100 {
                        settings.share_volumes.remove(&s.user_id);
                    }
                    if settings != before {
                        chat.set_voice_settings(settings);
                    }
                }
            });
        },
    );
}

/// The chosen picture over the whole window (full screen).
pub(super) fn theater(ctx: &egui::Context, chat: &ChatService, state: &ChatState, chat_ui: &mut ChatUi) {
    let leave = |chat_ui: &mut ChatUi| {
        chat_ui.stage.theater = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
    };
    let Some(channel) = state.my_voice.as_ref().map(|m| m.channel) else {
        leave(chat_ui);
        return;
    };
    let (all, call) = sources(chat, state, channel);
    if chat_ui.stage.focus.is_none() || all.is_empty() || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        leave(chat_ui);
        return;
    }
    let screen = ctx.content_rect();
    egui::Area::new(egui::Id::new("chat-theater")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
        ui.set_min_size(screen.size());
        ui.scope_builder(egui::UiBuilder::new().max_rect(screen), |ui| {
            stage(ui, chat, state, chat_ui, &all, call);
        });
    });
}
