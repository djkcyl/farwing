//! Fullscreen chrome floats over video; it never changes the video/input geometry.
//! Each player draws its own caption inside the floating bar.
use crate::application::viewer_shortcuts::{self, Action};
use crate::ui::{controls, theme};
use std::time::{Duration, Instant};

const AUTO_COLLAPSE: Duration = Duration::from_millis(1500);
const ENTRY_HINT: Duration = Duration::from_secs(7);

#[derive(Default)]
pub(in crate::application::viewer) struct FullscreenChrome {
    active: bool,
    expanded: bool,
    last_used: Option<Instant>,
    hint_until: Option<Instant>,
    rect: Option<egui::Rect>,
}

impl FullscreenChrome {
    pub(in crate::application::viewer) fn menu_inset(&self, ctx: &egui::Context) -> f32 {
        if self.active {
            (ctx.content_rect().width() - floating_width(ctx)) / 2.0
        } else {
            0.0
        }
    }

    pub(in crate::application::viewer) fn sync(&mut self, active: bool) {
        if self.active == active {
            return;
        }
        *self = Self {
            active,
            ..Self::default()
        };
        if active {
            self.expanded = true;
            self.last_used = Some(Instant::now());
            self.hint_until = Some(Instant::now() + ENTRY_HINT);
        }
    }

    pub(in crate::application::viewer) fn owns_pointer(
        &self,
        ctx: &egui::Context,
        relative: bool,
    ) -> bool {
        // Game input keeps its locked cursor. The existing release-control
        // shortcut makes this UI reachable without changing fullscreen state.
        self.active
            && !relative
            && self.rect.is_some_and(|rect| {
                ctx.input(|i| i.pointer.hover_pos().is_some_and(|p| rect.contains(p)))
            })
    }

    /// `menu_open` covers the player's own menus and `pending` any work the
    /// caption shows in progress; both keep the bar expanded.
    pub(in crate::application::viewer) fn show<R: Default>(
        &mut self,
        ctx: &egui::Context,
        relative: bool,
        menu_open: bool,
        pending: bool,
        caption: impl FnOnce(&mut egui::Ui) -> R,
    ) -> R {
        let now = Instant::now();
        let hovering = self.owns_pointer(ctx, relative);
        let menu_open = menu_open || ctx.any_popup_open();
        let busy = menu_open || pending || ctx.input(|i| i.pointer.any_down());
        if hovering || busy {
            self.last_used = Some(now);
        }
        if self.expanded && !busy && !hovering {
            let elapsed = now.saturating_duration_since(self.last_used.unwrap_or(now));
            if elapsed >= AUTO_COLLAPSE {
                self.expanded = false;
            } else {
                ctx.request_repaint_after(AUTO_COLLAPSE - elapsed);
            }
        }
        let mut action = R::default();
        let mut caption = Some(caption);
        let width = floating_width(ctx);
        let surface_width = if self.expanded {
            width + 2.0 // popup frame stroke
        } else {
            theme::VIEWER_FULLSCREEN_HANDLE_HIT_SIZE.x
        };
        // Separate area sizes and an explicit position avoid a one-frame jump
        // when switching between the narrow handle and the full toolbar.
        let response = egui::Area::new(egui::Id::new(("viewer-fullscreen-toolbar", self.expanded)))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(
                ctx.content_rect().center().x - surface_width / 2.0,
                ctx.content_rect().top(),
            ))
            .movable(false)
            .show(ctx, |ui| {
                if self.expanded {
                    egui::Frame::popup(ui.style())
                        .fill(theme::BG)
                        .corner_radius(theme::PANEL_RADIUS)
                        .inner_margin(egui::Margin::symmetric(8, 4))
                        .show(ui, |ui| {
                            ui.set_width(width - 16.0);
                            if let Some(caption) = caption.take() {
                                action = caption(ui);
                            }
                        });
                } else {
                    let (expand, hit_rect) = controls::viewer_fullscreen_handle(ui);
                    // egui's nearest-widget hit tolerance must not enlarge the
                    // explicitly narrow top-edge activation strip.
                    let inside =
                        ctx.input(|i| i.pointer.hover_pos().is_some_and(|p| hit_rect.contains(p)));
                    if !relative && inside && (expand.hovered() || expand.clicked()) {
                        self.expanded = true;
                        self.last_used = Some(now);
                        ctx.request_repaint();
                    }
                }
            });
        self.rect = Some(response.response.rect);
        if let Some(until) = self.hint_until.filter(|until| now < *until && !menu_open) {
            show_hint(ctx, response.response.rect.bottom() + 12.0);
            ctx.request_repaint_after(until.saturating_duration_since(now));
        }
        action
    }
}

fn floating_width(ctx: &egui::Context) -> f32 {
    (ctx.content_rect().width() - 24.0).clamp(32.0, theme::VIEWER_FLOATING_WIDTH)
}

fn show_hint(ctx: &egui::Context, top: f32) {
    let fullscreen = viewer_shortcuts::label(Action::Fullscreen);
    let release = viewer_shortcuts::label(Action::ReleaseMouse);
    let mut text = String::from("已进入全屏 · 鼠标移至顶部横条可展开操作栏并退出全屏");
    if fullscreen != "未绑定" {
        text.push_str(&format!("\n退出全屏：{fullscreen}"));
    }
    if release != "未绑定" {
        text.push_str(&format!("\n游戏鼠标锁定时，先按 {release} 释放键鼠"));
    }
    // Paint only: this reminder must never intercept desktop input or focus.
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("viewer-fullscreen-hint"),
    ));
    let galley = painter.layout(
        text,
        egui::FontId::proportional(theme::SMALL),
        theme::TEXT,
        (ctx.content_rect().width() - 48.0).max(1.0),
    );
    let rect = egui::Rect::from_min_size(
        egui::pos2(
            ctx.content_rect().center().x - galley.size().x / 2.0 - 12.0,
            top,
        ),
        galley.size() + egui::vec2(24.0, 16.0),
    );
    painter.rect_filled(rect, theme::PANEL_RADIUS, theme::VIEWER_SCRIM);
    painter.galley(rect.min + egui::vec2(12.0, 8.0), galley, theme::TEXT);
}
