//! Playback window UI, local input and repaint orchestration.
use super::chrome::{
    PlayerChromeAction, PlayerTitleBar, borderless_resize, constrain_window_aspect,
    fit_window_to_aspect, player_title_bar, resize_window_one_to_one,
};
use super::connection::WindowsConnectionApp;
use super::screen_windows::ScreenTabBar;
use super::swapchain::fit_rect;
use super::worker::{RenderCommand, RenderWorker};
use crate::application::viewer::windows_ui::{UiPresenter, UiTimingAudit, VideoWindow};
use crate::application::viewer::{
    ConnectionProgressApp, NativeViewerSession, PerformancePanelMode, StreamControlUi, mutex_lock,
    show_stream_control_window,
};
use crate::application::viewer_shortcuts::Action as ViewerShortcut;
use crate::diagnostics::performance::PerformanceMonitor;
use crate::features::stream_control::StreamControlHandle;
use crate::platform::graphics::window_hwnd;
use crate::ui::chrome::{
    WindowMoveState, WindowResizeState, set_fullscreen, title_bar_height_pixels, title_bar_panel,
};
use anyhow::{Result, bail};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D11::*;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

pub(in crate::application::viewer) fn viewer_shortcut(
    modifiers: winit::keyboard::ModifiersState,
    key: winit::keyboard::PhysicalKey,
) -> Option<ViewerShortcut> {
    if crate::application::viewer_shortcuts::suspended() {
        return None;
    }
    crate::application::viewer_shortcuts::match_key(
        crate::application::viewer_shortcuts::physical_key(key)?,
        crate::application::viewer_shortcuts::modifiers(modifiers),
    )
}

pub(in crate::application::viewer) struct ThreadedWindowsApp {
    fullscreen_chrome: crate::application::viewer::fullscreen_chrome::FullscreenChrome,
    drag: Option<crate::application::viewer::windows_drag::WindowDrag>,
    _shortcuts: crate::application::viewer_shortcuts::Watcher,
    pub(in crate::application::viewer) annotation:
        crate::application::viewer::annotation::AnnotationUi,
    pub(in crate::application::viewer) modifiers: winit::keyboard::ModifiersState,
    pub(in crate::application::viewer) mouse:
        crate::application::viewer::windows_mouse::WindowMouse,
    pub(in crate::application::viewer) last_mouse_mode: crate::features::remote_input::MouseMode,
    pub(super) screen_tabs: ScreenTabBar,
    pub(in crate::application::viewer) close_requested: bool,
    pub(in crate::application::viewer) title: String,
    pub(in crate::application::viewer) performance: PerformanceMonitor,
    pub(in crate::application::viewer) stream_control: StreamControlHandle,
    pub(in crate::application::viewer) stream_control_ui: StreamControlUi,
    pub(in crate::application::viewer) plugin_menu_open: bool,
    pub(in crate::application::viewer) display_transition_visible: bool,
    pub(in crate::application::viewer) bound_screen: u64,
    pub(in crate::application::viewer) shutdown: Arc<AtomicBool>,
    pub(in crate::application::viewer) fatal_error: Arc<Mutex<Option<String>>>,
    pub(in crate::application::viewer) egui_context: egui::Context,
    pub(in crate::application::viewer) egui_winit: egui_winit::State,
    pub(in crate::application::viewer) performance_mode: PerformancePanelMode,
    pub(in crate::application::viewer) intercept_shortcuts: bool,
    pub(in crate::application::viewer) startup_backdrop: Option<ConnectionProgressApp>,
    pub(in crate::application::viewer) last_window_size: PhysicalSize<u32>,
    pub(in crate::application::viewer) pending_aspect_size: Option<PhysicalSize<u32>>,
    pub(in crate::application::viewer) last_aspect_video_size: Option<(u32, u32)>,
    display_aspect: Option<(u32, u32)>,
    pub(in crate::application::viewer) window_move: WindowMoveState,
    pub(in crate::application::viewer) window_resize: WindowResizeState,
    pub(in crate::application::viewer) timing_audit: Option<UiTimingAudit>,
    // Field order is the shutdown order: join render, close decoder, destroy child, release UI.
    pub(in crate::application::viewer) renderer: RenderWorker,
    pub(in crate::application::viewer) _session: Arc<NativeViewerSession>,
    pub(in crate::application::viewer) video_window: VideoWindow,
    pub(in crate::application::viewer) ui_presenter: UiPresenter,
    // Only successive video workers use this context; each previous worker is joined first.
    pub(in crate::application::viewer) cpu_device:
        Arc<Mutex<Option<(ID3D11Device, ID3D11DeviceContext)>>>,
}

impl ThreadedWindowsApp {
    pub(in crate::application::viewer) fn replace_session(
        &mut self,
        window: &Window,
        session: Arc<NativeViewerSession>,
    ) -> Result<()> {
        // One composition target per HWND. Keep UI composition and the video
        // child window; retire the old swap chain before attaching a new track.
        self.stream_control
            .cancel_display_change(self.bound_screen as u32 as i32);
        self.drag.take();
        self.drag = Some(crate::application::viewer::windows_drag::WindowDrag::new(
            window,
            &session.stream_control,
        )?);
        self.annotation.bind(session.stream_control.clone());
        self.stream_control_ui = StreamControlUi::default();
        self.mouse.release(window);
        self.mouse = crate::application::viewer::windows_mouse::WindowMouse::new(
            window,
            &session.stream_control,
            &self.egui_context,
        );
        self.last_mouse_mode = crate::features::remote_input::MouseMode::View;
        self.renderer.stop();
        self.performance.pause_presentation();
        let size = self.video_window.resize(window, window.inner_size())?;
        self.renderer = RenderWorker::spawn(
            self.video_window.handle(),
            size,
            &session,
            &self.egui_context,
            Arc::clone(&self.cpu_device),
        )?;
        self.title = session.title.clone();
        self.performance = session.performance.clone();
        self.stream_control = session.stream_control.clone();
        self.shutdown = Arc::clone(&session.shutdown);
        self.fatal_error = Arc::clone(&session.fatal_error);
        self.bound_screen = session.screen_binding();
        self._session = session;
        self.last_aspect_video_size = None;
        Ok(())
    }

    pub(in crate::application::viewer) fn from_session(
        window: &Window,
        session: Arc<NativeViewerSession>,
        connecting: WindowsConnectionApp,
    ) -> Result<Self> {
        let title = session.title.clone();
        let performance = session.performance.clone();
        let stream_control = session.stream_control.clone();
        let preferences = stream_control.device_preferences();
        let shutdown = Arc::clone(&session.shutdown);
        let fatal_error = Arc::clone(&session.fatal_error);
        let video_window = VideoWindow::new(window)?;
        let size = video_window.resize(window, window.inner_size())?;
        let cpu_device = Arc::new(Mutex::new(None));
        let renderer = RenderWorker::spawn(
            video_window.handle(),
            size,
            &session,
            &connecting.egui_context,
            Arc::clone(&cpu_device),
        )?;
        Ok(Self {
            fullscreen_chrome:
                crate::application::viewer::fullscreen_chrome::FullscreenChrome::default(),
            drag: Some(crate::application::viewer::windows_drag::WindowDrag::new(
                window,
                &stream_control,
            )?),
            _shortcuts: crate::application::viewer_shortcuts::Watcher::new()?,
            annotation: crate::application::viewer::annotation::AnnotationUi::new(
                stream_control.clone(),
                window_hwnd(window)?.0 as u64,
            ),
            mouse: crate::application::viewer::windows_mouse::WindowMouse::new(
                window,
                &stream_control,
                &connecting.egui_context,
            ),
            last_mouse_mode: crate::features::remote_input::MouseMode::View,
            modifiers: winit::keyboard::ModifiersState::empty(),
            screen_tabs: ScreenTabBar::default(),
            close_requested: false,
            title,
            performance,
            stream_control,
            stream_control_ui: StreamControlUi::default(),
            plugin_menu_open: false,
            display_transition_visible: false,
            bound_screen: session.screen_binding(),
            shutdown,
            fatal_error,
            egui_context: connecting.egui_context,
            egui_winit: connecting.egui_winit,
            performance_mode: preferences.performance_mode,
            intercept_shortcuts: preferences.intercept_shortcuts,
            startup_backdrop: Some(connecting.progress),
            last_window_size: window.inner_size(),
            pending_aspect_size: None,
            last_aspect_video_size: None,
            display_aspect: None,
            window_move: WindowMoveState::default(),
            window_resize: WindowResizeState::default(),
            timing_audit: connecting.timing_audit,
            renderer,
            _session: session,
            video_window,
            ui_presenter: connecting.presenter,
            cpu_device,
        })
    }

    pub(in crate::application::viewer) fn on_window_event(
        &mut self,
        window: &Window,
        event: &WindowEvent,
        event_loop: &ActiveEventLoop,
    ) -> Result<()> {
        if let WindowEvent::ModifiersChanged(modifiers) = event {
            self.modifiers = modifiers.state();
        }
        if matches!(event, WindowEvent::KeyboardInput {event:key,..}
            if key.state == winit::event::ElementState::Pressed
                && key.physical_key == winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::Escape))
            && self.stream_control.drag_drop().interactive()
        {
            self.stream_control.drag_drop().cancel_gesture(false);
            window.request_redraw();
            return Ok(());
        }
        if let WindowEvent::KeyboardInput {
            event: key,
            is_synthetic: false,
            ..
        } = event
            && window.has_focus()
            && (!crate::platform::windows::input::system::own_message()
                || self.stream_control.mouse().accepts_host_input())
            && !self.egui_context.text_edit_focused()
            && key.state == winit::event::ElementState::Pressed
            && let Some(shortcut) = viewer_shortcut(self.modifiers, key.physical_key)
        {
            if !key.repeat {
                self.apply_shortcut(window, shortcut)?;
                window.request_redraw();
                if !self.close_requested {
                    self.refresh_mouse(window, event_loop);
                }
            }
            return Ok(());
        }
        let response = self.egui_winit.on_window_event(window, event);
        if let WindowEvent::Focused(focused) = event {
            tracing::debug!(focused, mouse_mode = ?self.stream_control.mouse().mode(),
                "viewer input focus changed");
        }
        if matches!(
            event,
            WindowEvent::Focused(false) | WindowEvent::Occluded(true)
        ) {
            self.annotation.finish();
            self.renderer.plugins.disarm();
            self.mouse.release(window);
        }
        if matches!(event, WindowEvent::Focused(false)) {
            self.stream_control.drag_drop().cancel_gesture(true);
            self.modifiers = winit::keyboard::ModifiersState::empty();
            if let Ok(hwnd) = window_hwnd(window) {
                crate::application::viewer_shortcuts::set_text_owner(hwnd.0 as u64, false);
            }
        }
        if response.repaint && !matches!(event, WindowEvent::RedrawRequested) {
            window.request_redraw();
        }
        match event {
            WindowEvent::Resized(size) => {
                self.enforce_aspect_after_resize(window, *size);
                self.resize_targets(window, *size)?;
                window.request_redraw();
            }
            WindowEvent::RedrawRequested => self.draw_ui(window)?,
            WindowEvent::Occluded(occluded) => {
                self.renderer.send(RenderCommand::Occluded(*occluded));
            }
            _ => {}
        }
        self.refresh_mouse(window, event_loop);
        if let WindowEvent::MouseWheel { delta, .. } = event {
            self.mouse.wheel(*delta);
        }
        Ok(())
    }

    pub(in crate::application::viewer) fn apply_shortcut(
        &mut self,
        window: &Window,
        shortcut: ViewerShortcut,
    ) -> Result<()> {
        self.renderer.plugins.disarm();
        match shortcut {
            ViewerShortcut::ReleaseMouse => {
                self.mouse.release(window);
                if let Err(error) = self
                    .stream_control
                    .set_mouse_mode(crate::features::remote_input::MouseMode::View)
                {
                    self.stream_control.mouse().fail(error.to_string());
                }
            }
            ViewerShortcut::Fullscreen => {
                self.mouse.release(window);
                self.pending_aspect_size = None;
                self.window_move = WindowMoveState::default();
                self.window_resize = WindowResizeState::default();
                self.stream_control_ui.open = false;
                self.plugin_menu_open = false;
                let fullscreen = window
                    .fullscreen()
                    .is_none()
                    .then(|| winit::window::Fullscreen::Borderless(window.current_monitor()));
                set_fullscreen(window, fullscreen);
                self.fullscreen_chrome.sync(window.fullscreen().is_some());
                // A maximized window may keep the same outer size. Update
                // the video origin even if no size-change event follows.
                self.resize_targets(window, window.inner_size())?;
            }
            ViewerShortcut::Close => {
                self.mouse.release(window);
                self.close_requested = true;
            }
            ViewerShortcut::Performance => {
                self.performance_mode = self
                    .stream_control
                    .device_preferences()
                    .performance_mode
                    .next();
                self.stream_control
                    .set_performance_mode(self.performance_mode);
            }
        }
        window.request_redraw();
        Ok(())
    }

    fn sync_device_view_preferences(&mut self, window: &Window) {
        let preferences = self.stream_control.device_preferences();
        self.performance_mode = preferences.performance_mode;
        if self.intercept_shortcuts != preferences.intercept_shortcuts {
            self.mouse.release(window);
            self.intercept_shortcuts = preferences.intercept_shortcuts;
        }
    }

    pub(super) fn refresh_mouse(&mut self, window: &Window, event_loop: &ActiveEventLoop) {
        self.sync_device_view_preferences(window);
        let display_aspect = self
            .stream_control
            .display_aspect(self._session.track_index);
        if self.display_aspect != display_aspect {
            self.mouse.release(window);
            self.display_aspect = display_aspect;
            self.renderer
                .send(RenderCommand::DisplayAspect(display_aspect));
        }
        if let Ok(hwnd) = window_hwnd(window) {
            crate::application::viewer::windows_keyboard::finish_lock_releases(hwnd.0 as u64);
        }
        if let Ok(hwnd) = window_hwnd(window)
            && let Some(shortcut) =
                crate::application::viewer::windows_keyboard::take_shortcut(hwnd.0 as u64)
            && window.has_focus()
            && let Err(error) = self.apply_shortcut(window, shortcut)
        {
            self.stream_control.mouse().fail(error.to_string());
        }
        if self.close_requested {
            self.renderer.plugins.disarm();
            self.mouse.release(window);
            return;
        }
        let mode = self.stream_control.mouse().mode();
        if self.last_mouse_mode != crate::features::remote_input::MouseMode::View
            && mode == crate::features::remote_input::MouseMode::View
            && self.stream_control.mouse().error().is_some()
        {
            self.stream_control.reconcile_mouse_failure();
        }
        if self.last_mouse_mode != mode && mode != crate::features::remote_input::MouseMode::View {
            self.stream_control_ui.open = false;
            window.request_redraw();
        }
        self.last_mouse_mode = mode;
        // The visible transition owns the obscured desktop, like a local menu.
        // Release held input while it is shown; the title bar remains interactive.
        let display_transition = self.stream_control.annotation_snapshot().enabled
            || self.fullscreen_chrome.owns_pointer(
                &self.egui_context,
                self.stream_control.mouse().relative_mode(),
            )
            || self
                .stream_control
                .remote_upgrade()
                .is_some_and(|upgrade| upgrade.owns_input(window.id()))
            || self.startup_backdrop.is_some()
            || self.display_transition_visible
            || crate::application::viewer::stream_menu::topology_menu::owns_input(
                &self.egui_context,
            )
            || self.egui_context.memory(|m| m.top_modal_layer().is_some())
            || self.bound_screen != self._session.screen_binding()
            || self
                .stream_control
                .display_input_blocked(self._session.track_index, self.bound_screen)
            || self
                .stream_control
                .pending_display_resolution(self._session.screen_id())
                .is_some();
        if let Some(drag) = &self.drag {
            let available = !display_transition
                && !self.stream_control_ui.open
                && !self.plugin_menu_open
                && !self.screen_tabs.is_pending()
                && !window.is_minimized().unwrap_or(false);
            drag.geometry(
                available
                    .then(|| {
                        crate::application::viewer::windows_mouse::Target::for_window(
                            window,
                            &self.stream_control,
                            &self.egui_context,
                            self._session.track_index,
                            &self.renderer.current_video_size,
                        )
                    })
                    .flatten(),
            );
        }
        if window.has_focus()
            && !window.is_minimized().unwrap_or(false)
            && !display_transition
            && !self.stream_control.drag_drop().interactive()
            && !self.stream_control_ui.open
            && !self.plugin_menu_open
            && !self.screen_tabs.is_pending()
            && !self.egui_context.any_popup_open()
            && !self.egui_context.text_edit_focused()
            && let Err(error) = self.stream_control.restore_input_control()
        {
            self.stream_control.mouse().fail(error.to_string());
        }
        self.renderer.plugins.input_context(
            self.stream_control.mouse().clone(),
            window_hwnd(window).map_or(0, |h| h.0 as u64),
            window.has_focus()
                && !window.is_minimized().unwrap_or(false)
                && !self.plugin_menu_open
                && !self.stream_control_ui.open
                && !display_transition
                && !self.screen_tabs.is_pending()
                && !self.egui_context.any_popup_open()
                && !self.egui_context.text_edit_focused(),
        );
        self.mouse.refresh(
            window,
            &self.egui_context,
            &self.stream_control,
            self._session.track_index,
            &self.renderer.current_video_size,
            self.intercept_shortcuts,
            self.stream_control_ui.open
                || self.plugin_menu_open
                || self.screen_tabs.is_pending()
                || display_transition,
            event_loop,
        );
    }

    pub(in crate::application::viewer) fn draw_ui(&mut self, window: &Window) -> Result<()> {
        self.sync_device_view_preferences(window);
        self.egui_context
            .request_repaint_after(Duration::from_millis(500));
        if self.renderer.first_presented.load(Ordering::Acquire) {
            self.startup_backdrop = None;
        }
        let started = Instant::now();
        let input = self.egui_winit.take_egui_input(window);
        let mut view = crate::application::viewer::stream_menu::LocalViewSettings {
            performance_mode: self.performance_mode,
            performance_always_visible: self
                .stream_control
                .device_preferences()
                .performance_always_visible,
            intercept_shortcuts: self.intercept_shortcuts,
            send_ctrl_alt_del: false,
        };
        let mut chrome_action = PlayerChromeAction::default();
        let fullscreen = window.fullscreen().is_some();
        self.fullscreen_chrome.sync(fullscreen);
        let mut resize = None;
        let resize_aspect = self.current_presentation_size();
        let plugin_video_size = self.current_presentation_size();
        let output = self.egui_context.run_ui(input, |ui| {
            let ctx = ui.ctx().clone();
            if fullscreen {
                let relative = self.stream_control.mouse().relative_mode();
                let menu_open = self.stream_control_ui.open || self.plugin_menu_open;
                let pending = self.screen_tabs.is_pending();
                chrome_action =
                    self.fullscreen_chrome
                        .show(&ctx, relative, menu_open, pending, |ui| {
                            player_title_bar(
                                ui,
                                PlayerTitleBar {
                                    screens: &mut self.screen_tabs,
                                    window,
                                    title: &self.title,
                                    performance: &self.performance,
                                    stream_control: &self.stream_control,
                                    stream_control_ui: &mut self.stream_control_ui,
                                    plugin_menu_open: &mut self.plugin_menu_open,
                                    move_state: None,
                                },
                            )
                        });
            } else {
                resize =
                    borderless_resize(ui, window, Some((&mut self.window_resize, resize_aspect)));
                title_bar_panel(
                    ui,
                    "viewer-toolbar",
                    title_bar_height_pixels(window) as f32 / ctx.pixels_per_point(),
                    |ui| {
                        chrome_action = player_title_bar(
                            ui,
                            PlayerTitleBar {
                                screens: &mut self.screen_tabs,
                                window,
                                title: &self.title,
                                performance: &self.performance,
                                stream_control: &self.stream_control,
                                stream_control_ui: &mut self.stream_control_ui,
                                plugin_menu_open: &mut self.plugin_menu_open,
                                move_state: Some(&mut self.window_move),
                            },
                        );
                    },
                );
            }
            if let Some(loading) = &mut self.startup_backdrop {
                loading.draw(ui);
                return;
            }
            self.display_transition_visible = crate::application::viewer::display_transition::show(
                &ctx,
                &self.stream_control,
                self._session.screen_id(),
                ui.available_rect_before_wrap(),
            );
            let menu_inset = self.fullscreen_chrome.menu_inset(&ctx);
            show_stream_control_window(
                &ctx,
                &self.stream_control,
                &mut self.stream_control_ui,
                &mut view,
                self._session.screen_id(),
                window
                    .current_monitor()
                    .map(|m| (m.size().width, m.size().height)),
                egui::vec2(-114.0 - menu_inset, 52.0),
            );
            self.renderer.plugins.window(
                &ctx,
                &mut self.plugin_menu_open,
                crate::application::viewer::stream_menu::menu_style,
                egui::vec2(-148.0 - menu_inset, 52.0),
            );
            let size = window.inner_size();
            let top = title_bar_height_pixels(window);
            if let Some((w, h)) = plugin_video_size {
                let r = fit_rect(w, h, size.width, size.height.saturating_sub(top));
                let scale = ctx.pixels_per_point();
                self.renderer.plugins.paint(
                    &ctx,
                    size.width as f32 / scale,
                    size.height as f32 / scale,
                    [
                        r.left as f32 / scale,
                        (r.top as f32 + top as f32) / scale,
                        r.right as f32 / scale,
                        (r.bottom as f32 + top as f32) / scale,
                    ],
                );
            }
            let annotation_rect = plugin_video_size.map(|(w, h)| {
                let r = fit_rect(w, h, size.width, size.height.saturating_sub(top));
                let scale = ctx.pixels_per_point();
                egui::Rect::from_min_max(
                    egui::pos2(r.left as f32 / scale, (r.top as f32 + top as f32) / scale),
                    egui::pos2(
                        r.right as f32 / scale,
                        (r.bottom as f32 + top as f32) / scale,
                    ),
                )
            });
            self.annotation.draw(
                ui,
                self._session.screen_id(),
                annotation_rect,
                self.display_transition_visible
                    || self.screen_tabs.is_pending()
                    || self.stream_control_ui.open
                    || self.plugin_menu_open
                    || self
                        .stream_control
                        .display_input_blocked(self._session.track_index, self.bound_screen),
                window.has_focus(),
            );
            crate::application::viewer::show_performance_overlay(
                &ctx,
                &self.performance,
                &self.stream_control.audio(),
                view.performance_mode,
                view.performance_always_visible,
                annotation_rect.unwrap_or_else(|| ui.available_rect_before_wrap()),
                "performance-grid-d3d11",
            );
            if let Some(upgrade) = self.stream_control.remote_upgrade() {
                upgrade.show(&ctx, window.id(), &self.stream_control);
            }
            if let Some(drag) = &self.drag {
                drag.show(&ctx);
            }
            crate::ui::controls::show_notices(&ctx);
            if window.has_focus() && !self.stream_control_ui.open && !self.plugin_menu_open {
                crate::application::viewer::polling_warning::show(
                    &ctx,
                    self.stream_control.mouse(),
                    ui.available_rect_before_wrap(),
                );
            }
        });
        if self.performance_mode != view.performance_mode {
            self.stream_control
                .set_performance_mode(view.performance_mode);
            self.performance_mode = view.performance_mode;
        }
        self.stream_control
            .set_performance_always_visible(view.performance_always_visible);
        if self.intercept_shortcuts != view.intercept_shortcuts {
            self.mouse.release(window);
            self.intercept_shortcuts = view.intercept_shortcuts;
            self.stream_control
                .set_intercept_shortcuts(view.intercept_shortcuts);
        }
        if view.send_ctrl_alt_del && window.has_focus() {
            self.mouse.release(window);
            let result = window_hwnd(window)
                .and_then(|hwnd| self.stream_control.mouse().send_ctrl_alt_del(hwnd.0 as u64));
            match result {
                Ok(()) => {
                    self.stream_control_ui.local_error = None;
                    self.stream_control_ui.open = false;
                }
                Err(error) => self.stream_control_ui.local_error = Some(error.to_string()),
            }
            window.request_redraw();
        }
        if let Ok(hwnd) = window_hwnd(window) {
            crate::application::viewer_shortcuts::set_text_owner(
                hwnd.0 as u64,
                self.egui_context.text_edit_focused()
                    || crate::plugins::capturing_shortcut(&self.egui_context),
            );
        }
        let (renderer_output, platform_output, viewports) = egui_directx11::split_output(output);
        let immediate = viewports
            .get(&egui::ViewportId::ROOT)
            .is_some_and(|viewport| viewport.repaint_delay.is_zero());
        chrome_action.close |= viewports
            .get(&egui::ViewportId::ROOT)
            .is_some_and(|viewport| {
                viewport
                    .commands
                    .iter()
                    .any(|cmd| matches!(cmd, egui::ViewportCommand::Close))
            });
        self.egui_winit
            .handle_platform_output(window, platform_output);
        let layout_elapsed = started.elapsed();
        let presented = self
            .ui_presenter
            .render(&self.egui_context, renderer_output, true)?;
        if let Some(audit) = UiTimingAudit::active(&mut self.timing_audit, started) {
            audit.record(
                started,
                layout_elapsed,
                started.elapsed().saturating_sub(layout_elapsed),
                immediate,
                presented.presented(),
            );
        }
        if let Some(size) = self.window_resize.requested_render_size.take() {
            self.resize_targets(window, size)?;
        }
        let current_video_size = self.current_presentation_size();
        if current_video_size.is_some() && current_video_size != self.last_aspect_video_size {
            self.fit_window_to_current_aspect(window);
        }
        self.last_aspect_video_size = current_video_size;
        if chrome_action.exit_fullscreen && window.fullscreen().is_some() {
            self.apply_shortcut(window, ViewerShortcut::Fullscreen)?;
        }
        if chrome_action.close {
            self.mouse.release(window);
            self.close_requested = true;
        }
        if chrome_action.toggle_annotation {
            self.mouse.release(window);
            self.renderer.plugins.disarm();
            self.stream_control_ui.open = false;
            self.plugin_menu_open = false;
            if !self.stream_control.annotation_snapshot().supported
                && !self.stream_control.annotation_snapshot().enabled
                && let Some(upgrade) = self.stream_control.remote_upgrade()
            {
                upgrade.prompt(window.id(), "批注");
            } else {
                self.annotation.toggle();
            }
            window.request_redraw();
        }
        if chrome_action.toggle_mouse {
            let state = self.stream_control.snapshot();
            let mode = if state.mouse_mode != crate::features::remote_input::MouseMode::View
                || state.mouse_pending
            {
                self.mouse.release(window);
                crate::features::remote_input::MouseMode::View
            } else {
                state.mouse_preference
            };
            if mode != crate::features::remote_input::MouseMode::View {
                crate::ui::controls::clear_notice(&self.egui_context, "remote-input-error");
            }
            if let Err(error) = self.stream_control.set_mouse_mode(mode) {
                self.stream_control.mouse().fail(error.to_string());
            }
        }
        if chrome_action.one_to_one {
            self.resize_window_one_to_one(window);
        }
        if chrome_action.drag_window {
            let _ = window.drag_window();
        }
        if let Some(direction) = resize {
            let _ = window.drag_resize_window(direction);
        }
        if let Some(error) = mutex_lock(&self.fatal_error).clone() {
            bail!(error);
        }
        Ok(())
    }

    pub(in crate::application::viewer) fn resize_targets(
        &mut self,
        window: &Window,
        size: PhysicalSize<u32>,
    ) -> Result<()> {
        self.ui_presenter.resize(size)?;
        let video_size = self.video_window.resize(window, size)?;
        self.renderer.send(RenderCommand::Resize(
            if size.width == 0 || size.height == 0 {
                PhysicalSize::new(0, 0)
            } else {
                video_size
            },
        ));
        Ok(())
    }

    pub(in crate::application::viewer) fn current_presentation_size(&self) -> Option<(u32, u32)> {
        mutex_lock(&self.renderer.current_video_size).map(|(width, height, rotation)| {
            let size = if rotation == 90 || rotation == 270 {
                (height, width)
            } else {
                (width, height)
            };
            self.display_aspect.unwrap_or(size)
        })
    }

    pub(in crate::application::viewer) fn enforce_aspect_after_resize(
        &mut self,
        window: &Window,
        size: PhysicalSize<u32>,
    ) {
        constrain_window_aspect(
            window,
            size,
            self.current_presentation_size(),
            &mut self.last_window_size,
            &mut self.pending_aspect_size,
        );
    }

    pub(in crate::application::viewer) fn fit_window_to_current_aspect(&mut self, window: &Window) {
        if let Some(video_size) = self.current_presentation_size() {
            fit_window_to_aspect(window, video_size, &mut self.pending_aspect_size);
        }
    }

    pub(in crate::application::viewer) fn resize_window_one_to_one(&mut self, window: &Window) {
        let video_size = mutex_lock(&self.renderer.current_video_size).map(|(w, h, rotation)| {
            let size = if matches!(rotation, 90 | 270) {
                (h, w)
            } else {
                (w, h)
            };
            crate::media::geometry::DisplayAspect::size(size, self.display_aspect)
        });
        if let Some(video_size) = video_size {
            resize_window_one_to_one(window, video_size, &mut self.pending_aspect_size);
        }
    }
}

impl Drop for ThreadedWindowsApp {
    fn drop(&mut self) {
        self.stream_control
            .cancel_display_change(self.bound_screen as u32 as i32);
        self.renderer.stop();
        self.performance.pause_presentation();
    }
}
