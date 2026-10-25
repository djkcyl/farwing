//! What happens in the session, shown over the picture to everyone in it:
//! who joined or left, who took control, how the picture travels; that a
//! lower quality may help when the picture stalls often at a chosen one;
//! and a scrim while a switch of the picture's path holds it still. Paint
//! only: no egui area, response or hit region, so input still reaches the
//! picture.
use crate::diagnostics::performance::PerformanceMonitor;
use crate::features::stream_control::StreamControlHandle;
use crate::i18n::t;
use crate::ui::theme;
use egui::{Align2, Color32, FontId, Rect, Stroke, pos2, vec2};
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

/// A notice stays this long, fading out over the last part.
const SHOWN: Duration = Duration::from_secs(4);
const FADE: Duration = Duration::from_millis(600);
/// At most this many notices at once; older ones give way.
const MOST: usize = 3;
/// A switch of the picture's path normally completes within this, the
/// old path showing until then...
const SWITCHING: Duration = Duration::from_millis(1500);
/// ...after which a picture held this long is covered.
const HELD: Duration = Duration::from_millis(300);
/// The picture held this long while video kept arriving: a stall (a still
/// screen sends nothing, a dead link neither)...
const STALL: Duration = Duration::from_millis(300);
/// ...this many within this long, or one this long, at a quality the viewer
/// chose, suggest a lower one...
const STALLS: usize = 3;
const STALLS_WITHIN: Duration = Duration::from_secs(30);
const LONG_STALL: Duration = Duration::from_secs(3);
/// ...at most this often for the same quality.
const SUGGEST_AGAIN: Duration = Duration::from_secs(300);
const MARGIN: f32 = 14.0;
const HEIGHT: f32 = 28.0;
const GAP: f32 = 6.0;
const PADDING: f32 = 12.0;

#[derive(Default)]
pub(crate) struct RoomNotices {
    /// The other participants (id → nickname); `None` until the first list,
    /// which is not announced.
    known: Option<BTreeMap<u64, String>>,
    controlling: u64,
    /// The forwarding last seen (`Some(e2ee)` through the forwarder);
    /// `None` until the first look.
    forwarded: Option<Option<bool>>,
    shown: VecDeque<(Instant, String)>,
    /// The picture is held in a stall now.
    stalling: bool,
    stalls: VecDeque<Instant>,
    /// When a lower quality was last suggested, by the quality in use.
    suggested: BTreeMap<&'static str, Instant>,
}

impl RoomNotices {
    /// Notices for what changed since the last frame, then paint them (and
    /// the switch scrim) over `bounds`, the picture's area.
    pub(crate) fn show(
        &mut self,
        ctx: &egui::Context,
        control: &StreamControlHandle,
        performance: &PerformanceMonitor,
        bounds: Rect,
    ) {
        let now = Instant::now();
        self.observe(control, now);
        self.watch_stalls(control, performance, now);
        // The stall watch runs while the picture stands still too.
        ctx.request_repaint_after(Duration::from_millis(250));
        self.paint_scrim(ctx, performance, bounds);
        self.paint_notices(ctx, bounds, now);
    }

    fn observe(&mut self, control: &StreamControlHandle, now: Instant) {
        let members = control.participants().members();
        let others: BTreeMap<u64, String> = members
            .iter()
            .filter(|m| !m.you)
            .map(|m| (m.id, m.nickname.clone()))
            .collect();
        let controlling = members
            .iter()
            .find(|m| m.controlling && !m.you)
            .map_or(0, |m| m.id);
        let viewer = &t().viewer;
        let mut news = Vec::new();
        // An empty list is a session between lists, not everyone leaving.
        if let Some(known) = &self.known
            && !members.is_empty()
        {
            for (id, name) in &others {
                if !known.contains_key(id) {
                    news.push((viewer.room_joined)(name));
                }
            }
            for (id, name) in known {
                if !others.contains_key(id) {
                    news.push((viewer.room_left)(name));
                }
            }
            if controlling != 0 && controlling != self.controlling {
                let name = others.get(&controlling).cloned().unwrap_or_default();
                news.push((viewer.room_control_taken)(&name));
            }
        }
        for text in news {
            self.push(now, text);
        }
        if self.known.is_none() || !members.is_empty() {
            self.known = Some(others);
            self.controlling = controlling;
        }
        let forwarded = control.forwarded_e2ee();
        if let Some(before) = self.forwarded
            && before != forwarded
        {
            self.push(
                now,
                match forwarded {
                    Some(true) => viewer.participants_forwarded_e2ee.to_owned(),
                    Some(false) => viewer.participants_forwarded_compatible.to_owned(),
                    None => viewer.room_forwarding_ended.to_owned(),
                },
            );
        }
        self.forwarded = Some(forwarded);
        while self
            .shown
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) >= SHOWN)
        {
            self.shown.pop_front();
        }
    }

    /// Counts stalls; frequent ones at a quality the viewer chose suggest the
    /// next lower one (自动 for a custom bitrate). A switch of the picture's
    /// path is not a stall: its scrim covers it.
    fn watch_stalls(&mut self, control: &StreamControlHandle, performance: &PerformanceMonitor, now: Instant) {
        use crate::features::stream_control::StreamQuality;
        let held = performance.picture_age().is_some_and(|age| age >= STALL);
        let stalled = held
            && performance.switch_pending().is_none()
            && performance.video_arrived_within(STALL / 2);
        if stalled && !self.stalling {
            self.stalls.push_back(now);
        }
        self.stalling = held && (self.stalling || stalled);
        while self
            .stalls
            .front()
            .is_some_and(|at| now.duration_since(*at) >= STALLS_WITHIN)
        {
            self.stalls.pop_front();
        }
        let long = self.stalling && performance.picture_age().is_some_and(|age| age >= LONG_STALL);
        if self.stalls.len() < STALLS && !long {
            return;
        }
        self.stalls.clear();
        let viewer = &t().viewer;
        // Sharing the picture: a stored higher tier counts as 高清.
        let quality = match control.snapshot().settings.quality {
            StreamQuality::Original | StreamQuality::High | StreamQuality::Custom
                if control.participants().shared_view() =>
            {
                StreamQuality::Clear
            }
            other => other,
        };
        let (current, lower) = match quality {
            StreamQuality::Original => ("original", viewer.quality_very_high),
            StreamQuality::High => ("high", viewer.quality_high),
            StreamQuality::Clear => ("clear", viewer.quality_smooth),
            StreamQuality::Custom => ("custom", t().features.stream_quality_auto),
            StreamQuality::Auto | StreamQuality::Fast => return,
        };
        if self
            .suggested
            .get(current)
            .is_some_and(|at| now.duration_since(*at) < SUGGEST_AGAIN)
        {
            return;
        }
        self.suggested.insert(current, now);
        tracing::info!(quality = current, "frequent stalls; a lower quality suggested");
        self.push(now, (viewer.stalling_try)(lower));
    }

    fn push(&mut self, now: Instant, text: String) {
        if text.trim().is_empty() {
            return;
        }
        if self.shown.len() == MOST {
            self.shown.pop_front();
        }
        self.shown.push_back((now, text));
    }

    /// While a switch of the picture's path takes unusually long, cover a
    /// picture held still.
    fn paint_scrim(&self, ctx: &egui::Context, performance: &PerformanceMonitor, bounds: Rect) {
        let Some(pending) = performance.switch_pending() else {
            return;
        };
        ctx.request_repaint_after(Duration::from_millis(50));
        if pending < SWITCHING || performance.picture_age().is_none_or(|age| age < HELD) {
            return;
        }
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("picture-switch-scrim"),
        ));
        painter.rect_filled(bounds, 0.0, theme::VIEWER_SCRIM);
        let center = bounds.center();
        let angle = (ctx.input(|i| i.time) * 6.0) as f32;
        let radius = 11.0;
        let points: Vec<_> = (0..=24)
            .map(|i| {
                let a = angle + i as f32 / 24.0 * std::f32::consts::TAU * 0.75;
                pos2(center.x + radius * a.cos(), center.y - 14.0 + radius * a.sin())
            })
            .collect();
        painter.add(egui::Shape::line(points, Stroke::new(2.5, theme::ACCENT)));
        painter.text(
            pos2(center.x, center.y + 16.0),
            Align2::CENTER_CENTER,
            t().viewer.picture_switching,
            FontId::proportional(theme::BODY),
            theme::TEXT,
        );
    }

    fn paint_notices(&self, ctx: &egui::Context, bounds: Rect, now: Instant) {
        if self.shown.is_empty() {
            return;
        }
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("room-notices"),
        ));
        let font = FontId::proportional(theme::SMALL + 1.0);
        let mut y = bounds.top() + MARGIN;
        for (at, text) in &self.shown {
            let age = now.duration_since(*at);
            let left = SHOWN.saturating_sub(age);
            let alpha = (left.as_secs_f32() / FADE.as_secs_f32()).clamp(0.0, 1.0);
            let galley = painter.layout_no_wrap(text.clone(), font.clone(), theme::TEXT);
            let width = (galley.size().x + PADDING * 2.0).min(bounds.width() - MARGIN * 2.0);
            let pill = Rect::from_center_size(
                pos2(bounds.center().x, y + HEIGHT / 2.0),
                vec2(width, HEIGHT),
            );
            painter.rect(
                pill,
                theme::PANEL_RADIUS,
                theme::BG.gamma_multiply(0.92 * alpha),
                Stroke::new(1.0, theme::LINE.gamma_multiply(alpha)),
                egui::StrokeKind::Inside,
            );
            painter.galley(
                pos2(pill.center().x - galley.size().x / 2.0, pill.center().y - galley.size().y / 2.0),
                galley,
                Color32::WHITE.gamma_multiply(alpha),
            );
            y += HEIGHT + GAP;
        }
        ctx.request_repaint_after(Duration::from_millis(50));
    }
}
