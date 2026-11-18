//! Fullscreen chrome floats over video; it never changes the video/input geometry.
//! Each player draws its own caption inside the floating bar.
use crate::application::viewer_shortcuts::{self, Action};
use crate::ui::controls;
use crate::ui::theme;
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
    ball_enabled: bool,
    touch_opened: bool,
    ball_rect: Option<egui::Rect>,
    touches: Vec<(winit::event::DeviceId, u64)>,
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
        let ball_enabled = crate::media::preferences::fullscreen_floating_ball();
        self.sync_mode(active, ball_enabled);
    }

    fn sync_mode(&mut self, active: bool, ball_enabled: bool) {
        if self.active == active && self.ball_enabled == ball_enabled {
            return;
        }
        *self = Self {
            active,
            ball_enabled,
            ..Self::default()
        };
        if active && !ball_enabled {
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
        // Game input keeps its locked cursor: hovering never opens the bar
        // then, the release-control shortcut does. A tap on the ball or the
        // strip keeps the unlocked local pointer while the bar is open, even
        // after the finger lifts or moves away from the floating controls.
        let rect = if self.ball_enabled {
            self.ball_rect
        } else {
            self.rect
        };
        self.active
            && (((self.ball_enabled || self.touch_opened) && self.expanded)
                || (!relative
                    && rect.is_some_and(|rect| {
                        ctx.input(|i| i.pointer.hover_pos().is_some_and(|p| rect.contains(p)))
                    })))
    }

    /// A touchscreen can reach local controls while the game owns the pointer.
    /// Consume its whole activation gesture so it cannot also click the remote.
    pub(in crate::application::viewer) fn intercept_touch(
        &mut self,
        ctx: &egui::Context,
        event: &winit::event::WindowEvent,
    ) -> bool {
        use winit::event::{TouchPhase, WindowEvent};
        if matches!(event, WindowEvent::Focused(false)) {
            self.touches.clear();
        }
        let WindowEvent::Touch(touch) = event else {
            return false;
        };
        let id = (touch.device_id, touch.id);
        if self.touches.contains(&id) {
            if matches!(touch.phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                self.touches.retain(|captured| *captured != id);
            }
            return true;
        }
        let position =
            egui::pos2(touch.location.x as f32, touch.location.y as f32) / ctx.pixels_per_point();
        let activation = self.ball_rect.is_some_and(|rect| rect.contains(position));
        let dismiss = !self.ball_enabled && self.touch_opened && outside_controls(ctx, position);
        if self.active && touch.phase == TouchPhase::Started && (activation || dismiss) {
            if self.touches.is_empty() {
                if dismiss {
                    self.expanded = false;
                    self.touch_opened = false;
                } else {
                    self.expanded = !self.expanded;
                    self.touch_opened = !self.ball_enabled && self.expanded;
                }
            }
            self.touches.push(id);
            ctx.request_repaint();
            return true;
        }
        false
    }

    /// A press on the video while a player menu is open closes the menu, so
    /// the floating bar can collapse again. The menu blocks remote input, so
    /// this press would otherwise do nothing.
    pub(in crate::application::viewer) fn dismisses_menu(&self, ctx: &egui::Context) -> bool {
        self.active
            && ctx
                .input(|i| {
                    i.pointer
                        .primary_pressed()
                        .then(|| i.pointer.interact_pos())
                        .flatten()
                })
                .is_some_and(|p| outside_controls(ctx, p))
    }

    pub(in crate::application::viewer) fn show<R: Default>(
        &mut self,
        ctx: &egui::Context,
        relative: bool,
        menu_open: bool,
        pending: bool,
        caption: impl FnOnce(&mut egui::Ui) -> R,
    ) -> R {
        if self.ball_enabled {
            return self.show_ball(ctx, caption);
        }
        self.show_strip(ctx, relative, menu_open, pending, caption)
    }

    /// Neither the activation ball nor the caption reserves video space.
    fn show_ball<R: Default>(
        &mut self,
        ctx: &egui::Context,
        caption: impl FnOnce(&mut egui::Ui) -> R,
    ) -> R {
        let screen = ctx.content_rect();
        let size = 44.0;
        let ball = egui::Area::new(egui::Id::new("viewer-fullscreen-ball"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(
                (screen.right() - size - 12.0).max(screen.left()),
                screen.center().y - size / 2.0,
            ))
            .movable(false)
            .show(ctx, |ui| {
                let (rect, response) =
                    ui.allocate_exact_size(egui::Vec2::splat(size), egui::Sense::click());
                let fill = if response.hovered() || self.expanded {
                    theme::HOVER
                } else {
                    theme::VIEWER_SCRIM
                };
                ui.painter().circle_filled(rect.center(), size / 2.0, fill);
                for y in [-5.0, 0.0, 5.0] {
                    let center = rect.center() + egui::vec2(0.0, y);
                    ui.painter().line_segment(
                        [center - egui::vec2(8.0, 0.0), center + egui::vec2(8.0, 0.0)],
                        egui::Stroke::new(1.5, theme::TEXT),
                    );
                }
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Button,
                        true,
                        if self.expanded {
                            "收起操作栏"
                        } else {
                            "展开操作栏"
                        },
                    )
                });
                if response.clicked()
                    && response
                        .interact_pointer_pos()
                        .is_some_and(|p| rect.contains(p))
                {
                    self.expanded = !self.expanded;
                    ctx.request_repaint();
                }
                rect
            });
        self.ball_rect = Some(ball.inner);
        self.rect = None;
        if !self.expanded {
            return R::default();
        }
        let width = floating_width(ctx);
        let mut action = R::default();
        let toolbar = egui::Area::new(egui::Id::new("viewer-fullscreen-toolbar"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(
                screen.center().x - (width + 2.0) / 2.0,
                screen.top(),
            ))
            .movable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .fill(theme::BG)
                    .corner_radius(theme::PANEL_RADIUS)
                    .inner_margin(egui::Margin::symmetric(8, 4))
                    .show(ui, |ui| {
                        ui.set_width(width - 16.0);
                        action = caption(ui);
                    });
            });
        self.rect = Some(toolbar.response.rect);
        action
    }

    /// `menu_open` covers the player's own menus and `pending` any work the
    /// caption shows in progress; both keep the bar expanded.
    fn show_strip<R: Default>(
        &mut self,
        ctx: &egui::Context,
        relative: bool,
        menu_open: bool,
        pending: bool,
        caption: impl FnOnce(&mut egui::Ui) -> R,
    ) -> R {
        {
            self.ball_rect = None;
            let pressed = ctx.input(|i| {
                i.pointer
                    .primary_pressed()
                    .then(|| i.pointer.interact_pos())
                    .flatten()
            });
            let dismiss = self.touch_opened && pressed.is_some_and(|p| outside_controls(ctx, p));
            if dismiss && !menu_open && !pending {
                self.expanded = false;
                self.touch_opened = false;
            }
        }
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
                    {
                        self.ball_rect = Some(hit_rect);
                    }
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
    text.push_str("\n触屏可点击顶部横条展开操作栏，点回画面收起");
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

fn outside_controls(ctx: &egui::Context, position: egui::Pos2) -> bool {
    !ctx.any_popup_open()
        && ctx.memory(|memory| memory.top_modal_layer().is_none())
        && ctx
            .layer_id_at(position)
            .is_none_or(|layer| layer.order == egui::Order::Background)
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::event::{DeviceId, Touch, TouchPhase, WindowEvent};

    fn touch(id: u64, phase: TouchPhase, x: f64, y: f64) -> WindowEvent {
        WindowEvent::Touch(Touch {
            device_id: DeviceId::dummy(),
            phase,
            location: winit::dpi::PhysicalPosition::new(x, y),
            force: None,
            id,
        })
    }

    #[test]
    fn a_ball_tap_unlocks_local_controls_until_explicitly_collapsed() {
        let ctx = egui::Context::default();
        let mut chrome = FullscreenChrome::default();
        chrome.sync_mode(true, true);
        assert!(!chrome.expanded);
        chrome.ball_rect = Some(egui::Rect::from_min_size(
            egui::pos2(100.0, 100.0),
            egui::Vec2::splat(44.0),
        ));
        assert!(!chrome.intercept_touch(&ctx, &touch(1, TouchPhase::Started, 40.0, 40.0)));
        assert!(chrome.intercept_touch(&ctx, &touch(2, TouchPhase::Started, 120.0, 120.0)));
        assert!(chrome.expanded);
        assert!(chrome.owns_pointer(&ctx, true));
        assert!(chrome.intercept_touch(&ctx, &touch(4, TouchPhase::Started, 120.0, 120.0)));
        assert!(chrome.expanded);
        assert!(chrome.intercept_touch(&ctx, &touch(2, TouchPhase::Moved, 80.0, 80.0)));
        assert!(chrome.intercept_touch(&ctx, &touch(2, TouchPhase::Ended, 80.0, 80.0)));
        assert!(chrome.intercept_touch(&ctx, &touch(4, TouchPhase::Cancelled, 120.0, 120.0)));
        assert!(chrome.owns_pointer(&ctx, true));
        assert!(chrome.intercept_touch(&ctx, &touch(3, TouchPhase::Started, 120.0, 120.0)));
        assert!(!chrome.expanded);
        assert!(chrome.intercept_touch(&ctx, &touch(3, TouchPhase::Ended, 120.0, 120.0)));
        assert!(!chrome.owns_pointer(&ctx, true));
        chrome.sync_mode(false, true);
        assert!(chrome.ball_rect.is_none());
        assert!(chrome.touches.is_empty());
    }

    #[test]
    fn switching_fullscreen_controls_clears_hits_and_touch_capture() {
        let ctx = egui::Context::default();
        let mut chrome = FullscreenChrome::default();
        chrome.sync_mode(true, false);
        assert!(chrome.expanded);
        assert!(!chrome.ball_enabled);
        chrome.expanded = false;
        chrome.ball_rect = Some(egui::Rect::from_min_size(
            egui::pos2(100.0, 0.0),
            egui::vec2(72.0, 16.0),
        ));
        assert!(chrome.intercept_touch(&ctx, &touch(2, TouchPhase::Started, 120.0, 8.0)));
        assert!(chrome.intercept_touch(&ctx, &touch(2, TouchPhase::Ended, 120.0, 8.0)));
        assert!(chrome.owns_pointer(&ctx, true));
        assert!(chrome.intercept_touch(&ctx, &touch(3, TouchPhase::Started, 300.0, 300.0)));
        assert!(!chrome.expanded);
        assert!(!chrome.owns_pointer(&ctx, true));
        assert!(chrome.intercept_touch(&ctx, &touch(3, TouchPhase::Ended, 300.0, 300.0)));
        chrome.touches.push((DeviceId::dummy(), 1));
        chrome.touch_opened = true;
        chrome.ball_rect = Some(egui::Rect::EVERYTHING);
        chrome.sync_mode(true, true);
        assert!(!chrome.expanded);
        assert!(chrome.ball_enabled);
        assert!(chrome.touches.is_empty());
        assert!(chrome.ball_rect.is_none());
        assert!(!chrome.touch_opened);
        chrome.sync_mode(true, false);
        assert!(chrome.expanded);
        assert!(!chrome.ball_enabled);
    }
    #[test]
    fn a_press_on_the_video_closes_menus_but_not_one_on_the_controls() {
        fn menu(ui: &mut egui::Ui) {
            egui::Window::new("settings")
                .fixed_pos(egui::pos2(500.0, 40.0))
                .show(ui.ctx(), |ui| ui.label("menu"));
        }
        fn press(ctx: &egui::Context, chrome: &FullscreenChrome, at: egui::Pos2) -> bool {
            let mut input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            };
            input.events = vec![
                egui::Event::PointerMoved(at),
                egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ];
            let mut dismissed = false;
            let mut output = ctx.run_ui(input, |ui| {
                dismissed = chrome.dismisses_menu(ui.ctx());
                menu(ui);
            });
            output.textures_delta.clear();
            // Release so the next frame starts a fresh press.
            let mut output = ctx.run_ui(
                egui::RawInput {
                    events: vec![egui::Event::PointerButton {
                        pos: at,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    ..Default::default()
                },
                menu,
            );
            output.textures_delta.clear();
            dismissed
        }
        let ctx = egui::Context::default();
        let mut chrome = FullscreenChrome::default();
        assert!(!press(&ctx, &chrome, egui::pos2(100.0, 400.0)));
        chrome.sync_mode(true, false);
        assert!(press(&ctx, &chrome, egui::pos2(100.0, 400.0)));
        assert!(!press(&ctx, &chrome, egui::pos2(510.0, 50.0)));
    }
}
