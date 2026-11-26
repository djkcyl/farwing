use super::{
    ScreenStreams, VideoPicture, annotation::AnnotationUi, audio_view::AudioView,
    frames::mutex_lock, hud, key_usage, participants, stream_menu, waiting,
};
use crate::connection_progress::{ConnectionProgress, ConnectionProgressState};
use crate::features::stream_control::DisplayTopologyAction;
use crate::features::stream_control::{MouseMode, RemoteScreen, StreamControlHandle};
use crate::i18n::t;
use crate::ui::controls::{ViewerCaptionIcon as Icon, viewer_caption_button as button};
use crate::ui::{branding, controls, theme};
use anyhow::Result;
use egui::{
    Align, Event, Layout, MouseWheelUnit, PointerButton, Rect, RichText, Sense,
    ViewportCommand as WindowCommand, ViewportId, vec2,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub(crate) enum Playback {
    Video(Box<Screens>),
    Audio(AudioView),
}

impl Playback {
    pub(crate) fn video(
        streams: ScreenStreams,
        ctx: &egui::Context,
        id: ViewportId,
        screen: i32,
        picture: VideoPicture,
    ) -> Self {
        let mut main = Pane::new(streams.control(), ctx, id, screen);
        main.picture = Some(picture);
        Self::Video(Box::new(Screens {
            streams,
            main,
            extras: BTreeMap::new(),
            topology_sequence: None,
            runtime: tokio::runtime::Handle::current(),
        }))
    }

    pub(crate) fn audio(control: StreamControlHandle) -> Self {
        Self::Audio(AudioView::new(control, None))
    }
}

pub(crate) struct ViewerWindow {
    pub(crate) alias: String,
    pub(crate) cancel: CancellationToken,
    pub(crate) playback: Option<Playback>,
    pub(crate) progress: ConnectionProgress,
}

impl ViewerWindow {
    pub(crate) fn show(&mut self, root: &mut egui::Ui, parent: ViewportId) {
        if root.input(|i| i.viewport().close_requested()) {
            self.cancel.cancel();
        }
        if self.cancel.is_cancelled() {
            self.playback = None;
            root.ctx().request_repaint_of(parent);
            return;
        }
        let close = match self.playback.as_mut() {
            Some(Playback::Video(screens)) => screens.show(root, &self.alias),
            Some(Playback::Audio(audio)) => {
                let close = caption(root, &self.alias, |_| {});
                let disconnect = audio.show(root);
                if let Some(compact) = audio.take_layout_change() {
                    let min = if compact {
                        vec2(365.0, 135.0)
                    } else {
                        vec2(460.0, 320.0)
                    };
                    root.ctx()
                        .send_viewport_cmd(WindowCommand::MinInnerSize(min));
                    root.ctx()
                        .send_viewport_cmd(WindowCommand::InnerSize(if compact {
                            min
                        } else {
                            vec2(560.0, 490.0)
                        }));
                }
                close || disconnect
            }
            None => {
                let close = caption(root, &self.alias, |_| {});
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().fill(theme::BG))
                    .show(root, |ui| {
                        waiting(ui, &self.progress.title);
                        if !self.progress.detail.is_empty() {
                            let failed =
                                matches!(self.progress.state, ConnectionProgressState::Failed);
                            ui.colored_label(
                                if failed { theme::RED } else { theme::MUTED },
                                &self.progress.detail,
                            );
                        }
                        if let Some(retry) = &self.progress.retry
                            && ui.button(t().viewer.retry_now).clicked()
                        {
                            retry.trigger();
                        }
                    });
                close
            }
        };
        controls::show_notices(root.ctx());
        if close {
            self.cancel.cancel();
            self.playback = None;
            root.ctx().request_repaint_of(parent);
        }
    }
}

pub(crate) struct Screens {
    streams: ScreenStreams,
    main: Pane,
    extras: BTreeMap<i32, Arc<Mutex<Pane>>>,
    topology_sequence: Option<i64>,
    runtime: tokio::runtime::Handle,
}

impl Screens {
    fn show(&mut self, root: &mut egui::Ui, alias: &str) -> bool {
        let ctx = root.ctx().clone();
        let snapshot = self.streams.control().snapshot();
        let displays = &snapshot.screens;
        let main_id = ctx.viewport_id();
        self.extras
            .retain(|id, pane| !mutex_lock(pane).closed && displays.iter().any(|s| s.id == *id));
        let topology = snapshot.topology;
        if topology.reported && !topology.dismissed && topology.sequence != self.topology_sequence {
            self.topology_sequence = topology.sequence;
            if let Some(id) = topology
                .selected_screen
                .filter(|id| displays.iter().any(|s| s.id == *id))
            {
                self.extras.remove(&id);
                self.main.select(id, self.streams.control());
            }
        }
        if !displays.iter().any(|s| s.id == self.main.screen)
            && let Some(screen) = displays
                .iter()
                .filter(|s| s.id >= 0)
                .min_by_key(|s| !s.primary)
        {
            self.extras.remove(&screen.id);
            self.main.select(screen.id, self.streams.control());
        }

        let mut action = None;
        let close = self
            .main
            .show(root, alias, &self.streams, &self.runtime, |ui, pane| {
                if displays.len() <= 1 && !snapshot.topology_support.create_visible {
                    return;
                }
                let title = displays
                    .iter()
                    .find(|s| s.id == pane.screen)
                    .map(|s| s.label(displays))
                    .unwrap_or_else(|| t().viewer.display.to_owned());
                ui.menu_button(title, |ui| {
                    for screen in displays {
                        ui.push_id(screen.id, |ui| {
                            ui.horizontal(|ui| {
                                if ui
                                    .selectable_label(
                                        pane.screen == screen.id,
                                        screen.label(displays),
                                    )
                                    .clicked()
                                {
                                    action = Some((screen.id, false));
                                    ui.close();
                                }
                                if pane.screen != screen.id
                                    && ui.small_button(t().viewer.open_in_window).clicked()
                                {
                                    action = Some((screen.id, true));
                                    ui.close();
                                }
                            });
                        });
                    }
                    if snapshot.topology_support.create_visible {
                        let enabled =
                            snapshot.ready && !topology.pending && snapshot.topology_support.create;
                        let add = ui.add_enabled(
                            enabled,
                            egui::Button::new(t().features.stream_add_virtual_screen),
                        );
                        if let Some(reason) = snapshot.topology_support.create_unavailable {
                            add.clone().on_disabled_hover_text(reason);
                        }
                        if add.clicked() {
                            let (width, height, _) = stream_menu::topology_menu::local_parameters(
                                &ctx,
                                self.streams.control(),
                                local_monitor_size(&ctx),
                            );
                            stream_menu::topology_menu::request(
                                &ctx,
                                self.streams.control(),
                                pane.screen,
                                DisplayTopologyAction::Create { width, height },
                            );
                            ui.close();
                        }
                    }
                });
            });
        if let Some((id, open_window)) = action {
            if let Some(pane) = self.extras.get(&id) {
                ctx.send_viewport_cmd_to(mutex_lock(pane).viewport, WindowCommand::Focus);
            } else if open_window {
                let viewport = ViewportId::from_hash_of((main_id, "display", id));
                self.extras.insert(
                    id,
                    Arc::new(Mutex::new(Pane::new(
                        self.streams.control(),
                        &ctx,
                        viewport,
                        id,
                    ))),
                );
            } else {
                self.main.select(id, self.streams.control());
            }
            ctx.request_repaint();
        }

        self.streams.set_visible(
            std::iter::once(self.main.screen)
                .chain(self.extras.keys().copied())
                .collect(),
        );
        for (id, pane) in &self.extras {
            let pane = Arc::clone(pane);
            let viewport = mutex_lock(&pane).viewport;
            let streams = self.streams.clone();
            let runtime = self.runtime.clone();
            let title = displays
                .iter()
                .find(|screen| screen.id == *id)
                .map(|screen| screen.label(displays))
                .unwrap_or_default();
            ctx.show_viewport_deferred(
                viewport,
                viewport_builder(format!("{alias} — {title}")),
                move |root, _| {
                    let mut pane = mutex_lock(&pane);
                    if !pane.closed
                        && (root.input(|i| i.viewport().close_requested())
                            || pane.show(root, &title, &streams, &runtime, |_, _| {}))
                    {
                        pane.closed = true;
                        streams.control().mouse().pause_owner(pane.owner());
                        root.ctx().request_repaint_of(main_id);
                    }
                    controls::show_notices(root.ctx());
                },
            );
        }
        close
    }
}

pub(crate) fn viewport_builder(title: String) -> egui::ViewportBuilder {
    egui::ViewportBuilder::default()
        .with_decorations(false)
        .with_title(title)
        .with_icon(branding::icon())
        .with_app_id("farwing")
        .with_inner_size([1000.0, 680.0])
        .with_min_inner_size([520.0, 320.0])
}

fn caption(root: &mut egui::Ui, title: &str, toolbar: impl FnOnce(&mut egui::Ui)) -> bool {
    let ctx = root.ctx().clone();
    let expanded = ctx.input(|i| i.viewport().maximized == Some(true));
    let fullscreen = ctx.input(|i| i.viewport().fullscreen == Some(true));
    let text = t();
    let mut close = false;
    egui::Panel::top("viewer-caption")
        .resizable(false)
        .frame(
            egui::Frame::new()
                .fill(theme::SIDEBAR)
                .inner_margin(theme::WINDOW_TITLE_MARGIN),
        )
        .show(root, |ui| {
            ui.horizontal_centered(|ui| {
                ui.set_height(theme::WINDOW_TITLE_CONTENT_HEIGHT);
                let (rect, _) = ui.allocate_exact_size(
                    egui::Vec2::splat(theme::WINDOW_LOGO_SIZE),
                    Sense::hover(),
                );
                branding::paint(ui.painter(), rect, branding::Art::App, egui::Color32::WHITE);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    close = button(ui, Icon::Close, false, text.common.disconnect).clicked();
                    if button(
                        ui,
                        if expanded || fullscreen {
                            Icon::Restore
                        } else {
                            Icon::Maximize
                        },
                        false,
                        if expanded || fullscreen {
                            text.controls.restore
                        } else {
                            text.controls.maximize
                        },
                    )
                    .clicked()
                    {
                        ctx.send_viewport_cmd(if fullscreen {
                            WindowCommand::Fullscreen(false)
                        } else {
                            WindowCommand::Maximized(!expanded)
                        });
                    }
                    if !fullscreen
                        && button(ui, Icon::Minimize, false, text.controls.minimize).clicked()
                    {
                        ctx.send_viewport_cmd(WindowCommand::Minimized(true));
                    }
                    toolbar(ui);
                    let (rect, drag) = ui.allocate_exact_size(
                        vec2(ui.available_width(), theme::WINDOW_TITLE_CONTENT_HEIGHT),
                        Sense::click_and_drag(),
                    );
                    ui.place(
                        rect,
                        egui::Label::new(RichText::new(title).strong())
                            .halign(Align::Min)
                            .truncate()
                            .sense(Sense::hover()),
                    );
                    if !fullscreen && drag.double_clicked() {
                        ctx.send_viewport_cmd(WindowCommand::Maximized(!expanded));
                    } else if !fullscreen
                        && drag.is_pointer_button_down_on()
                        && ui.input(|i| i.pointer.primary_pressed())
                    {
                        ctx.send_viewport_cmd(WindowCommand::StartDrag);
                    }
                });
            });
        });
    close
}

struct Pane {
    viewport: ViewportId,
    screen: i32,
    picture: Option<VideoPicture>,
    opening: Option<(
        tokio::task::JoinHandle<()>,
        oneshot::Receiver<Result<VideoPicture>>,
    )>,
    error: Option<String>,
    closed: bool,
    menu: stream_menu::StreamControlUi,
    annotation: AnnotationUi,
    control: StreamControlHandle,
    annotating: bool,
    presence: participants::Presence,
    modifiers: [bool; 4],
    _repaint: Arc<dyn Fn() + Send + Sync>,
}

impl Pane {
    fn new(
        control: &StreamControlHandle,
        ctx: &egui::Context,
        viewport: ViewportId,
        screen: i32,
    ) -> Self {
        let context = ctx.clone();
        let owner = viewport.0.value();
        Self {
            viewport,
            screen,
            picture: None,
            opening: None,
            error: None,
            closed: false,
            control: control.clone(),
            menu: Default::default(),
            annotation: AnnotationUi::new(control.clone(), owner),
            annotating: false,
            presence: participants::Presence::new(control, ctx),
            modifiers: [false; 4],
            _repaint: control
                .mouse()
                .subscribe(move || context.request_repaint_of(viewport)),
        }
    }

    fn owner(&self) -> u64 {
        self.viewport.0.value()
    }

    fn select(&mut self, screen: i32, control: &StreamControlHandle) {
        if self.screen != screen {
            control.mouse().pause_owner(self.owner());
            self.modifiers = [false; 4];
            self.annotation.finish();
            self.reset_picture();
            self.error = None;
            self.screen = screen;
        }
    }

    fn reset_picture(&mut self) {
        if let Some((task, _)) = self.opening.take() {
            task.abort();
        }
        self.picture = None;
    }

    fn load(
        &mut self,
        ctx: &egui::Context,
        streams: &ScreenStreams,
        runtime: &tokio::runtime::Handle,
        screen: Option<&RemoteScreen>,
    ) {
        if let Some((_, result)) = self.opening.as_mut() {
            match result.try_recv() {
                Ok(Ok(picture)) => {
                    self.picture = Some(picture);
                    self.opening = None;
                }
                Ok(Err(error)) => {
                    self.error = Some(format!("{error:#}"));
                    self.opening = None;
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    self.error = Some(t().viewer.screen_load_cancelled.into());
                    self.opening = None;
                }
                Err(oneshot::error::TryRecvError::Empty) => {}
            }
        }
        if self.picture.as_ref().is_some_and(|p| {
            screen.is_none_or(|s| p.track != s.video_track_index || p.binding != s.binding())
        }) {
            self.reset_picture();
        }
        if self.picture.is_none()
            && self.opening.is_none()
            && self.error.is_none()
            && screen.is_some()
        {
            let streams = streams.clone();
            let ctx = ctx.clone();
            let viewport = self.viewport;
            let screen = self.screen;
            let (sender, receiver) = oneshot::channel();
            let task = runtime.spawn(async move {
                let result = streams.open(ctx.clone(), viewport, screen).await;
                let _ = sender.send(result);
                ctx.request_repaint_of(viewport);
            });
            self.opening = Some((task, receiver));
        }
    }

    fn show(
        &mut self,
        root: &mut egui::Ui,
        title: &str,
        streams: &ScreenStreams,
        runtime: &tokio::runtime::Handle,
        selectors: impl FnOnce(&mut egui::Ui, &Self),
    ) -> bool {
        let ctx = root.ctx().clone();
        let control = streams.control();
        let text = t();
        let focused = ctx.input(|i| i.focused);
        let fullscreen = ctx.input(|i| i.viewport().fullscreen == Some(true));
        let screen = streams.screen(self.screen);
        let camera = screen
            .as_ref()
            .is_some_and(|screen| screen.display.is_camera());
        self.load(&ctx, streams, runtime, screen.as_ref());
        if let Some(picture) = self.picture.as_ref().filter(|_| focused) {
            streams.focus(picture.track);
        }
        let state = control.snapshot();
        let preferences = control.device_preferences();
        let mut show_caption = !fullscreen || self.menu.open;
        if fullscreen && !show_caption {
            let (handle, _) = controls::viewer_fullscreen_handle(root);
            show_caption = handle.hovered()
                || ctx.input(|i| {
                    i.pointer
                        .hover_pos()
                        .is_some_and(|p| p.y < theme::WINDOW_TITLE_CONTENT_HEIGHT)
                });
        }
        let size = self
            .picture
            .as_ref()
            .and_then(|picture| picture.native_size(control));
        let close = show_caption
            && caption(root, title, |ui| {
                if button(
                    ui,
                    Icon::Quality,
                    self.menu.open,
                    text.viewer.quality_and_stream_settings,
                )
                .clicked()
                {
                    self.menu.open = !self.menu.open;
                }
                if let Some(size) = size
                    && button(ui, Icon::OneToOne, false, text.viewer.one_to_one).clicked()
                {
                    let pixels = ctx.pixels_per_point();
                    ctx.send_viewport_cmd(WindowCommand::Fullscreen(false));
                    ctx.send_viewport_cmd(WindowCommand::Maximized(false));
                    ctx.send_viewport_cmd(WindowCommand::InnerSize(vec2(
                        size.0 as f32 / pixels,
                        size.1 as f32 / pixels
                            + theme::WINDOW_TITLE_CONTENT_HEIGHT
                            + 2.0 * f32::from(theme::WINDOW_TITLE_MARGIN.top),
                    )));
                }
                if !camera {
                    let annotation = control.annotation_snapshot();
                    if ui
                        .add_enabled_ui(annotation.supported || annotation.enabled, |ui| {
                            button(
                                ui,
                                Icon::Annotation,
                                self.annotating,
                                text.controls.annotation,
                            )
                        })
                        .inner
                        .clicked()
                    {
                        self.annotation.toggle();
                        self.annotating = !self.annotating;
                    }
                    let active = state.mouse_mode != MouseMode::View;
                    let hint = if active {
                        text.viewer.shortcut_release_control
                    } else if control.mouse().keyboard_supported() {
                        text.viewer.control_keyboard_mouse
                    } else {
                        text.viewer.control_mouse
                    };
                    if ui
                        .add_enabled_ui(size.is_some() && !state.mouse_pending, |ui| {
                            button(ui, Icon::Mouse, active, hint)
                        })
                        .inner
                        .clicked()
                    {
                        self.menu.local_error = control
                            .set_mouse_mode(if active {
                                MouseMode::View
                            } else {
                                MouseMode::Remote
                            })
                            .err()
                            .map(|e| format!("{e:#}"));
                    } else if !active
                        && state.remote_notice.as_ref().is_some_and(|(_, _, ask)| *ask)
                        && button(ui, Icon::AskControl, false, text.viewer.ask_control).clicked()
                    {
                        self.menu.local_error =
                            control.ask_control().err().map(|e| format!("{e:#}"));
                    }
                }
                let audio = control.audio();
                let mut settings = audio.settings();
                if button(
                    ui,
                    Icon::Audio {
                        muted: settings.muted,
                    },
                    settings.muted,
                    if settings.muted {
                        text.viewer.unmute
                    } else {
                        text.viewer.mute
                    },
                )
                .clicked()
                {
                    settings.muted = !settings.muted;
                    audio.set_settings(settings);
                }
                let members = control.participants().members();
                if !members.is_empty() {
                    let response = controls::viewer_participants_button(
                        ui,
                        members.len(),
                        &participants::button_hint(&members),
                    );
                    participants::menu(&response, &members, control.forwarded_e2ee());
                }
                selectors(ui, self);
            });
        if !ctx.egui_wants_keyboard_input()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F11))
        {
            ctx.send_viewport_cmd(WindowCommand::Fullscreen(!fullscreen));
        }
        let mut options = stream_menu::LocalViewSettings {
            performance_mode: preferences.performance_mode,
            performance_always_visible: preferences.performance_always_visible,
            intercept_shortcuts: false,
            system_command: None,
        };
        let local_size = local_monitor_size(&ctx);
        stream_menu::show_stream_control_window(
            &ctx,
            control,
            &mut self.menu,
            &mut options,
            self.screen,
            local_size,
            vec2(-12.0, theme::WINDOW_TITLE_CONTENT_HEIGHT + 8.0),
        );
        control.set_performance_mode(options.performance_mode);
        control.set_performance_always_visible(options.performance_always_visible);
        if let Some(command) = options.system_command {
            self.menu.local_error = control
                .mouse()
                .system_command(self.owner(), command)
                .err()
                .map(|e| format!("{e:#}"));
        }

        if let Some((_, message, _)) = &state.remote_notice {
            egui::Panel::top("viewer-remote-notice")
                .frame(egui::Frame::new().fill(theme::SURFACE).inner_margin(8))
                .show(root, |ui| {
                    ui.label(message);
                });
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(egui::Color32::BLACK))
            .show(root, |ui| {
                let mut blocked = true;
                let rect = if let Some(picture) = &mut self.picture {
                    blocked = control.display_input_blocked(picture.track, picture.binding);
                    let rect = picture
                        .paint(ui, control, self.screen)
                        .unwrap_or_else(|error| {
                            self.error = Some(format!("{error:#}"));
                            None
                        });
                    if let Some(rect) = rect
                        && !camera
                    {
                        hud::show_performance_overlay(
                            &ctx,
                            picture.performance(),
                            &control.audio(),
                            options.performance_mode,
                            options.performance_always_visible,
                            rect,
                            "linux-performance",
                        );
                    }
                    rect
                } else if self.error.is_none() {
                    waiting(
                        ui,
                        if state.topology.blocks(self.screen) {
                            &state.topology.message
                        } else {
                            text.session.progress_waiting_picture
                        },
                    );
                    None
                } else {
                    None
                };
                if let Some(error) = &self.error {
                    ui.colored_label(theme::RED, error);
                    if ui.button(text.common.retry).clicked() {
                        self.reset_picture();
                        self.error = None;
                    }
                }
                if !camera {
                    let overlay = self.menu.open
                        || stream_menu::topology_menu::owns_input(&ctx)
                        || ctx.any_popup_open();
                    self.annotation
                        .draw(ui, self.screen, rect, blocked || overlay, focused);
                    self.presence.share(
                        &ctx,
                        control,
                        self.screen,
                        rect,
                        focused && !blocked && !overlay,
                    );
                    if let Some(rect) = rect {
                        participants::paint_pointers(&ctx, control, self.screen, rect);
                        participants::paint_turn_hint(&ctx, control, rect);
                    }
                    self.input(
                        ui,
                        control,
                        rect.filter(|_| !blocked && !overlay && !self.annotating),
                        focused,
                    );
                }
            });
        close
    }

    fn input(
        &mut self,
        ui: &egui::Ui,
        control: &StreamControlHandle,
        rect: Option<Rect>,
        focused: bool,
    ) {
        let mouse = control.mouse();
        let owner = self.owner();
        let Some(rect) = rect.filter(|_| {
            focused && mouse.mode() != MouseMode::View && !mouse.transport_recovering()
        }) else {
            mouse.pause_owner(owner);
            self.modifiers = [false; 4];
            return;
        };
        let (mods, pointer, neutral, events) = ui.input(|i| {
            (
                i.modifiers,
                i.pointer.hover_pos(),
                i.keys_down.is_empty() && !i.pointer.any_down() && i.modifiers.is_none(),
                i.events.clone(),
            )
        });
        if mouse.waiting_for_neutral() {
            if neutral {
                mouse.confirm_neutral(mouse.activation_generation());
            }
            return;
        }
        let in_picture = pointer.is_some_and(|pos| rect.contains(pos));
        let move_to = |pos: egui::Pos2| {
            let p = (pos - rect.min) / rect.size();
            mouse.absolute(
                owner,
                self.screen,
                f64::from(p.x.clamp(0.0, 0.999_999)),
                f64::from(p.y.clamp(0.0, 0.999_999)),
            );
        };
        for (i, (held, next)) in self
            .modifiers
            .iter_mut()
            .zip([mods.ctrl, mods.shift, mods.alt, mods.mac_cmd])
            .enumerate()
        {
            if *held != next && (!next || in_picture) {
                mouse.key(owner, 0x0007_00e0 + i as u32, next);
                *held = next;
            }
        }
        for event in events {
            match event {
                Event::PointerMoved(pos) if rect.contains(pos) => move_to(pos),
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } if !pressed || rect.contains(pos) => {
                    if rect.contains(pos) {
                        move_to(pos);
                    }
                    mouse.button(
                        owner,
                        match button {
                            PointerButton::Primary => 1,
                            PointerButton::Secondary => 2,
                            PointerButton::Middle => 16,
                            PointerButton::Extra1 => 32,
                            PointerButton::Extra2 => 64,
                        },
                        pressed,
                    );
                }
                Event::Key {
                    physical_key: Some(key),
                    pressed,
                    ..
                } if !pressed || in_picture => {
                    if let Some(usage) = key_usage(key) {
                        mouse.key(owner, usage, pressed);
                    }
                }
                Event::MouseWheel { unit, delta, .. } if in_picture => {
                    let scale = match unit {
                        MouseWheelUnit::Point => 1.0,
                        MouseWheelUnit::Line => 120.0,
                        MouseWheelUnit::Page => 360.0,
                    };
                    mouse.wheel(
                        owner,
                        (delta.x * scale).round() as i32,
                        (delta.y * scale).round() as i32,
                    );
                }
                _ => {}
            }
        }
        mouse.heartbeat();
        if !neutral {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        self.control.mouse().pause_owner(self.owner());
        self.reset_picture();
    }
}

fn local_monitor_size(ctx: &egui::Context) -> Option<(u32, u32)> {
    ctx.input(|i| {
        let monitor = i.viewport().monitor_size?;
        let scale = i.viewport().native_pixels_per_point?;
        Some((
            (monitor.x * scale).round() as u32,
            (monitor.y * scale).round() as u32,
        ))
    })
}
