//! Window connection lifecycle and event-loop integration.
use super::chrome::borderless_resize;
use super::player::ThreadedWindowsApp;
use super::screen_windows::ScreenWindows;
use crate::application::viewer::windows_ui::{UiPresenter, UiTimingAudit};
use crate::application::viewer::{
    ConnectionProgress, ConnectionProgressApp, NativeViewerSession, ViewerDisplayHandle,
    ViewerWindowEvent, configure_viewer_visuals, mutex_lock,
};
use crate::ui::chrome::{
    configure_dwm_window, title_bar_height, title_bar_panel, window_title_bar,
};
use crate::ui::window_manager::{Event as UiEvent, Repaint as UiRepaintEvent};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::platform::windows::EventLoopBuilderExtWindows;
use winit::platform::windows::WindowAttributesExtWindows;
use winit::window::{Window, WindowAttributes, WindowId};

pub(in crate::application::viewer) fn run(session: NativeViewerSession) -> Result<()> {
    let (sender, receiver) = std_mpsc::channel();
    let alias = session
        .title
        .strip_prefix(crate::VIEWER_TITLE_PREFIX)
        .unwrap_or(&session.title)
        .to_owned();
    sender
        .send(ViewerWindowEvent::Playing(Box::new(session)))
        .map_err(|_| anyhow!("queue existing viewer session"))?;
    run_player(
        ConnectingWindowsRunConfig {
            connection_key: format!("standalone:{alias}"),
            connection_tabs: crate::application::viewer::connection_tabs::ConnectionTabs::default(),
            window_mode: crate::application::viewer::ViewerWindowMode::Separate,
            cancel: CancellationToken::new(),
            alias,
            progress: std_mpsc::channel().1,
            session: receiver,
            display_sender: tokio::sync::oneshot::channel().0,
        },
        false,
    )
}

pub(crate) struct ConnectingWindowsRunConfig {
    pub connection_key: String,
    pub connection_tabs: crate::application::viewer::connection_tabs::ConnectionTabs,
    pub window_mode: crate::application::viewer::ViewerWindowMode,
    pub cancel: CancellationToken,
    pub alias: String,
    pub progress: std_mpsc::Receiver<ConnectionProgress>,
    pub session: std_mpsc::Receiver<ViewerWindowEvent>,
    pub display_sender: tokio::sync::oneshot::Sender<ViewerDisplayHandle>,
}

pub(in crate::application::viewer) fn run_connecting(
    config: ConnectingWindowsRunConfig,
) -> Result<()> {
    run_player(config, true)
}

pub(in crate::application::viewer) fn ui_frame_interval(window: &Window) -> Duration {
    let millihertz = window
        .current_monitor()
        .and_then(|monitor| monitor.refresh_rate_millihertz())
        .filter(|rate| *rate != 0)
        .unwrap_or(60_000);
    Duration::from_secs_f64(1000.0 / f64::from(millihertz))
}

pub(in crate::application::viewer) fn run_player(
    config: ConnectingWindowsRunConfig,
    needs_display: bool,
) -> Result<()> {
    let mut builder = EventLoop::<UiEvent>::with_user_event();
    let router = crate::application::viewer::windows_mouse::router().clone();
    builder.with_msg_hook(move |message| {
        crate::application::viewer::windows_keyboard::message(message) || router.message(message)
    });
    let event_loop = builder.build().context("create player event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    crate::application::viewer::windows_keyboard::remove_unused_raw_keyboard()?;
    let _keyboard_hook = crate::application::viewer::windows_keyboard::KeyboardHook::install()
        .map_err(
            |error| tracing::warn!(%error, "keyboard hook unavailable; viewing remains available"),
        )
        .ok();
    let mut runner = ConnectingWindowsRunner::new(
        config,
        needs_display,
        event_loop.create_proxy(),
        false,
        None,
    );
    event_loop
        .run_app(&mut runner)
        .map_err(|error| anyhow!("run Windows connection/player event loop: {error}"))?;
    if let Some(error) = runner.fatal_error.take() {
        bail!(error);
    }
    Ok(())
}

struct ViewerConnectionInput {
    alias: String,
    progress: Option<std_mpsc::Receiver<ConnectionProgress>>,
    session: std_mpsc::Receiver<ViewerWindowEvent>,
    display: Option<oneshot::Sender<ViewerDisplayHandle>>,
    cancel: CancellationToken,
    done: Option<oneshot::Sender<Result<()>>>,
}

pub(crate) struct ConnectingWindowsRunner {
    audio_restore_geometry: Option<(
        winit::dpi::PhysicalSize<u32>,
        bool,
        Option<winit::window::Fullscreen>,
    )>,
    pub(in crate::application::viewer) embedded: bool,
    pub(super) screens: Option<ScreenWindows>,
    pub(in crate::application::viewer) close_requested: bool,
    pub(in crate::application::viewer) attributes: WindowAttributes,
    pub(in crate::application::viewer) alias: String,
    pub(in crate::application::viewer) connection_key: String,
    pub(in crate::application::viewer) connection_tabs:
        crate::application::viewer::connection_tabs::ConnectionTabs,
    connections: HashMap<String, ViewerConnectionInput>,
    closed_connections: Vec<String>,
    pub(in crate::application::viewer) window: Option<Window>,
    pub(in crate::application::viewer) connecting: Option<WindowsConnectionApp>,
    pub(in crate::application::viewer) playing: Option<ThreadedWindowsApp>,
    pub(in crate::application::viewer) fatal_error: Option<String>,
    pub(in crate::application::viewer) next_ui_update: Instant,
    pub(in crate::application::viewer) next_repaint: Option<Instant>,
    pub(in crate::application::viewer) last_ui_frame: Option<Instant>,
    pub(in crate::application::viewer) ui_frame_interval: Duration,
    pub(in crate::application::viewer) repaint_proxy: EventLoopProxy<UiEvent>,
    pub(in crate::application::viewer) ui_generation: u64,
}

impl ApplicationHandler<UiEvent> for ConnectingWindowsRunner {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        match event_loop.create_window(self.attributes.clone()) {
            Ok(window) => {
                crate::ui::branding::set_taskbar_icon(&window);
                configure_dwm_window(&window);
                let progress = self
                    .connections
                    .get_mut(&self.connection_key)
                    .and_then(|input| input.progress.take())
                    .expect("connecting window is created only once");
                self.ui_frame_interval = ui_frame_interval(&window);
                match WindowsConnectionApp::new(
                    &window,
                    self.alias.clone(),
                    progress,
                    self.repaint_proxy.clone(),
                    self.ui_generation,
                ) {
                    Ok((mut app, display)) => {
                        if let Err(error) = app.render(&window) {
                            self.fail(
                                event_loop,
                                format!("draw first connection UI frame: {error:#}"),
                            );
                            return;
                        }
                        self.last_ui_frame = Some(Instant::now());
                        if let Some(input) = self.connections.get_mut(&self.connection_key)
                            && let Some(sender) = input.display.take()
                        {
                            let _ = sender.send(ViewerDisplayHandle {
                                surface_writer: display.surface_writer.clone(),
                            });
                        }
                        self.window = Some(window);
                        self.connecting = Some(app);
                        if let Some(window) = self.window.as_ref() {
                            window.request_redraw();
                        }
                    }
                    Err(error) => self.fail(
                        event_loop,
                        format!("initialize Windows connection UI: {error:#}"),
                    ),
                }
            }
            Err(error) => self.fail(
                event_loop,
                format!("create Windows connection/player window: {error}"),
            ),
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if let Some(screens) = self.screens.as_mut() {
            screens.window_event(window_id, &event, event_loop);
            return;
        }
        let Some(window) = self.window.as_ref() else {
            return;
        };
        if window.id() != window_id {
            return;
        }
        if event == WindowEvent::CloseRequested {
            if let Some(app) = self.playing.as_mut() {
                app.mouse.release(window);
                app.shutdown.store(true, Ordering::Release);
            }
            self.exit(event_loop);
            return;
        }
        if matches!(
            event,
            WindowEvent::Moved(_) | WindowEvent::ScaleFactorChanged { .. }
        ) {
            self.ui_frame_interval = ui_frame_interval(window);
        }
        if event == WindowEvent::RedrawRequested {
            // Input events still enter egui immediately; coalesce their draw
            // requests to the local monitor cadence, never the stream's FPS.
            if window.is_minimized().unwrap_or(false) || !window.is_visible().unwrap_or(true) {
                self.next_repaint = None;
                return;
            }
            if let Some(earliest) = self.last_ui_frame.map(|at| at + self.ui_frame_interval)
                && Instant::now() < earliest
            {
                self.next_repaint =
                    Some(self.next_repaint.map_or(earliest, |old| old.min(earliest)));
                return;
            }
            self.next_repaint = None;
            self.last_ui_frame = Some(Instant::now());
        }
        let result = if let Some(app) = self.playing.as_mut() {
            app.on_window_event(window, &event, event_loop)
        } else if let Some(app) = self.connecting.as_mut() {
            app.on_window_event(window, &event)
        } else {
            Ok(())
        };
        if let Err(error) = result {
            self.fail(
                event_loop,
                format!("Windows connection/player failed: {error:#}"),
            );
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(window) = &self.window {
            window.set_visible(false);
        }
        self.screens.take();
        let completion = self
            .fatal_error
            .clone()
            .map_or(Ok(()), |error| Err(anyhow!(error)));
        for key in self.connections.keys().cloned().collect::<Vec<_>>() {
            self.complete_connection(
                &key,
                completion
                    .as_ref()
                    .map(|_| ())
                    .map_err(|error| anyhow!("{error:#}")),
            );
        }
        self.playing.take();
        self.connecting.take();
        self.window.take();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UiEvent) {
        let UiEvent::Repaint(event) = event else {
            return;
        };
        if !self.owns(event.window) {
            return;
        }
        if let Err(error) = self.receive_session_events() {
            self.fail(event_loop, format!("update player session: {error:#}"));
            return;
        }
        if let Some(screens) = self.screens.as_mut() {
            screens.repaint(event);
            return;
        }
        // Match egui's callback contract: requests from the just
        // completed pass are valid; older requests have already been serviced.
        // A replaced room has a new Context and must reject the old callbacks.
        let context = self
            .playing
            .as_ref()
            .map(|app| &app.egui_context)
            .or_else(|| self.connecting.as_ref().map(|app| &app.egui_context));
        if event.generation == self.ui_generation
            && let Some(context) = context
        {
            let current = context.cumulative_pass_nr();
            if current == event.pass || current == event.pass.saturating_add(1) {
                let when = self
                    .last_ui_frame
                    .map_or(event.when, |at| event.when.max(at + self.ui_frame_interval));
                self.next_repaint = Some(self.next_repaint.map_or(when, |old| old.min(when)));
            }
        }
        if self.close_requested
            || self
                .playing
                .as_ref()
                .is_some_and(|app| app.close_requested || app.shutdown.load(Ordering::Acquire))
        {
            self.exit(event_loop);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(error) = self.receive_session_events() {
            self.fail(event_loop, format!("update player session: {error:#}"));
            return;
        }
        if self.close_requested {
            self.exit(event_loop);
            return;
        }
        if let Some(screens) = self.screens.as_mut() {
            if !screens.update(event_loop) {
                self.exit(event_loop);
            }
            self.finish_closed_connections();
            return;
        }
        if let Some(app) = self.playing.as_mut() {
            if app.close_requested {
                self.exit(event_loop);
                return;
            }
            if let Some(window) = &self.window {
                app.refresh_mouse(window, event_loop);
            }
            if app.shutdown.load(Ordering::Acquire) {
                if let Some(error) = mutex_lock(&app.fatal_error).clone() {
                    self.fatal_error = Some(error);
                }
                self.exit(event_loop);
                return;
            }
        } else if self.connecting.is_none() {
            return;
        }
        if self
            .connecting
            .as_ref()
            .is_some_and(|app| app.close_requested)
        {
            self.exit(event_loop);
            return;
        }
        let now = Instant::now();
        let maintenance_due = now >= self.next_ui_update;
        let repaint_due = self.next_repaint.is_some_and(|when| now >= when);
        if maintenance_due || repaint_due {
            if let Some(window) = self.window.as_ref() {
                window.request_redraw();
            }
            self.next_repaint = None;
            if maintenance_due {
                self.next_ui_update = now
                    + if self.playing.is_some() {
                        Duration::from_millis(250)
                    } else {
                        Duration::from_millis(33)
                    };
            }
        }
        let wake_at = self
            .next_repaint
            .map_or(self.next_ui_update, |when| when.min(self.next_ui_update));
        event_loop.set_control_flow(ControlFlow::WaitUntil(wake_at));
    }
}

impl ConnectingWindowsRunner {
    pub(crate) fn new(
        config: ConnectingWindowsRunConfig,
        needs_display: bool,
        proxy: EventLoopProxy<UiEvent>,
        embedded: bool,
        done: Option<oneshot::Sender<Result<()>>>,
    ) -> Self {
        let primary_key = config.connection_key.clone();
        let tabs = config.connection_tabs.clone();
        tabs.add(primary_key.clone(), config.alias.clone());
        let mut connections = HashMap::new();
        connections.insert(
            primary_key.clone(),
            ViewerConnectionInput {
                alias: config.alias.clone(),
                progress: Some(config.progress),
                session: config.session,
                display: needs_display.then_some(config.display_sender),
                cancel: config.cancel.clone(),
                done,
            },
        );
        Self {
            audio_restore_geometry: None,
            attributes: WindowAttributes::default()
                .with_drag_and_drop(false)
                .with_visible(false)
                .with_title(format!("{}{}", crate::VIEWER_TITLE_PREFIX, config.alias))
                .with_window_icon(Some(crate::ui::branding::window_icon()))
                .with_decorations(false)
                .with_inner_size(LogicalSize::new(1280.0, 760.0))
                .with_min_inner_size(LogicalSize::new(760.0, 520.0)),
            alias: config.alias,
            connection_key: primary_key,
            connection_tabs: tabs,
            connections,
            closed_connections: Vec::new(),
            window: None,
            connecting: None,
            playing: None,
            fatal_error: None,
            next_ui_update: Instant::now(),
            next_repaint: None,
            last_ui_frame: None,
            ui_frame_interval: Duration::from_secs_f64(1.0 / 60.0),
            repaint_proxy: proxy,
            embedded,
            ui_generation: 1,
            close_requested: false,
            screens: None,
        }
    }
    pub(in crate::application::viewer) fn exit(&mut self, event_loop: &ActiveEventLoop) {
        self.close_requested = true;
        if !self.embedded {
            event_loop.exit();
        }
    }
    pub(crate) fn closed(&self) -> bool {
        self.close_requested
    }
    pub(crate) fn contains_connection(&self, key: &str) -> bool {
        self.connections.contains_key(key)
    }
    pub(crate) fn error(&self) -> Option<String> {
        self.fatal_error.clone()
    }
    pub(crate) fn owns(&self, id: WindowId) -> bool {
        self.window.as_ref().is_some_and(|w| w.id() == id)
            || self.screens.as_ref().is_some_and(|s| s.owns(id))
    }
    pub(crate) fn focus(&self) {
        if let Some(window) = &self.window {
            window.set_visible(true);
            window.focus_window();
        } else if let Some(screens) = &self.screens {
            screens.focus();
        }
    }
    pub(crate) fn attach_connection(
        &mut self,
        config: ConnectingWindowsRunConfig,
        done: oneshot::Sender<Result<()>>,
    ) -> Result<()> {
        let key = config.connection_key.clone();
        if self.connections.contains_key(&key) {
            let _ = done.send(Err(anyhow!("观看连接标签已存在")));
            return Ok(());
        }
        let alias = config.alias.clone();
        let mut input = ViewerConnectionInput {
            alias,
            progress: Some(config.progress),
            session: config.session,
            display: Some(config.display_sender),
            cancel: config.cancel,
            done: Some(done),
        };
        let writer = match crate::platform::surface::D3D11SurfaceWriter::new() {
            Ok(writer) => writer,
            Err(error) => {
                input.cancel.cancel();
                if let Some(done) = input.done.take() {
                    let _ = done.send(Err(anyhow!("create connection surface writer: {error:#}")));
                }
                return Ok(());
            }
        };
        if input
            .display
            .take()
            .expect("new tab display sender")
            .send(ViewerDisplayHandle {
                surface_writer: Some(writer),
            })
            .is_err()
        {
            input.cancel.cancel();
            if let Some(done) = input.done.take() {
                let _ = done.send(Err(anyhow!("connection owner cancelled display setup")));
            }
            return Ok(());
        }
        self.connection_tabs.add(key.clone(), input.alias.clone());
        self.connections.insert(key, input);
        Ok(())
    }

    fn complete_connection(&mut self, key: &str, result: Result<()>) {
        if let Some(mut input) = self.connections.remove(key) {
            input.cancel.cancel();
            self.closed_connections.push(key.to_owned());
            if let Some(done) = input.done.take() {
                let _ = done.send(result);
            }
        }
        if self.connections.len() == 1
            && let Some((key, alias)) = self
                .connections
                .iter()
                .next()
                .map(|(key, input)| (key.clone(), input.alias.clone()))
        {
            self.connection_key = key;
            self.alias = alias;
        }
    }

    fn finish_closed_connections(&mut self) {
        let keys = self
            .screens
            .as_mut()
            .map(|screens| screens.take_closed_connections())
            .unwrap_or_default();
        for key in keys {
            self.complete_connection(&key, Ok(()));
        }
        if self.connections.is_empty() {
            self.close_requested = true;
        }
    }

    pub(in crate::application::viewer) fn receive_session_events(&mut self) -> Result<()> {
        let keys: Vec<String> = self.connections.keys().cloned().collect();
        let mut queued = Vec::new();
        let mut abandoned = Vec::new();
        for key in &keys {
            if let Some(input) = self.connections.get_mut(key) {
                if input.done.as_ref().is_some_and(|done| done.is_closed()) {
                    abandoned.push(key.clone());
                    continue;
                }
                while let Ok(event) = input.session.try_recv() {
                    queued.push((key.clone(), event));
                }
                while let Some(progress) = input
                    .progress
                    .as_ref()
                    .and_then(|receiver| receiver.try_iter().last())
                {
                    match progress.state {
                        crate::application::viewer::ConnectionProgressState::Ready => {
                            self.connection_tabs.set_ready(key);
                        }
                        crate::application::viewer::ConnectionProgressState::Failed => {
                            self.connection_tabs.set_failed(key, progress.detail);
                        }
                        _ => {
                            self.connection_tabs.set_progress(key, progress.detail);
                        }
                    }
                }
            }
        }
        for key in abandoned {
            if let Some(screens) = self.screens.as_mut() {
                if screens.remove_connection(&key) {
                    self.close_requested = true;
                }
            } else {
                self.connection_tabs.remove(&key);
            }
            self.complete_connection(&key, Ok(()));
        }
        for (key, event) in queued {
            if self.close_requested {
                continue;
            }
            match event {
                ViewerWindowEvent::Close => {
                    if let Some(screens) = self.screens.as_mut() {
                        if screens.remove_connection(&key) {
                            self.close_requested = true;
                        }
                    } else {
                        self.connection_tabs.remove(&key);
                    }
                    self.complete_connection(&key, Ok(()));
                }
                ViewerWindowEvent::Listening(audio) => {
                    if (key != self.connection_key && self.connections.len() > 1)
                        || self.screens.is_some()
                    {
                        let error = anyhow!("仅音频模式暂不支持在共享观看窗口中显示");
                        self.connection_tabs.set_failed(&key, format!("{error:#}"));
                        if let Some(screens) = self.screens.as_mut()
                            && screens.remove_connection(&key)
                        {
                            self.close_requested = true;
                        }
                        self.complete_connection(&key, Err(error));
                        continue;
                    }
                    let window = self.window.as_ref().context("音频窗口已关闭")?;
                    self.connection_tabs.set_alias(&key, self.alias.clone());
                    self.connection_tabs.select(&key);
                    self.audio_restore_geometry.get_or_insert_with(|| {
                        (
                            window.inner_size(),
                            window.is_maximized(),
                            window.fullscreen(),
                        )
                    });
                    configure_audio_window(window, false);
                    self.connecting.as_mut().context("音频界面尚未准备")?.audio = Some(audio);
                    window.request_redraw();
                }
                ViewerWindowEvent::Playing(session) => {
                    let alias = self
                        .connections
                        .get(&key)
                        .map(|input| input.alias.clone())
                        .unwrap_or_else(|| self.alias.clone());
                    self.connection_tabs.set_ready(&key);
                    if self.screens.is_none() {
                        self.connection_tabs.select(&key);
                        self.start_playing(key.clone(), *session)?;
                    } else if let Some(screens) = self.screens.as_mut()
                        && let Err(error) = screens.add_connection(key.clone(), alias, *session)
                    {
                        self.connection_tabs.set_failed(&key, format!("{error:#}"));
                        self.complete_connection(&key, Err(error));
                    }
                }
                ViewerWindowEvent::Reconnect {
                    alias,
                    window,
                    progress,
                    display,
                } => {
                    if window.is_some_and(|window| {
                        self.screens
                            .as_ref()
                            .is_some_and(|screens| screens.owns(window))
                    }) {
                        self.focus();
                    }
                    self.connection_tabs.set_alias(&key, alias.clone());
                    self.connection_tabs.set_progress(&key, "正在重新连接");
                    if let Some(input) = self.connections.get_mut(&key) {
                        input.alias = alias.clone();
                        input.progress = Some(progress);
                    }
                    if self.connections.len() == 1 && self.connections.contains_key(&key) {
                        self.connection_key = key.clone();
                        self.alias = alias.clone();
                        if let Some(mut screens) = self.screens.take() {
                            self.window = screens.take_window(window);
                        }
                        self.playing.take();
                        self.connecting.take();
                        self.ui_generation = self.ui_generation.wrapping_add(1);
                        self.next_repaint = None;
                        let window = self.window.as_ref().context("重连时观看窗口已关闭")?;
                        if !window.is_resizable() {
                            restore_video_window_constraints(window);
                            let _ = window.request_inner_size(LogicalSize::new(1280.0, 760.0));
                        }
                        let progress = self
                            .connections
                            .get_mut(&key)
                            .and_then(|input| input.progress.take())
                            .context("重连进度已关闭")?;
                        let (mut app, new_display) = WindowsConnectionApp::new(
                            window,
                            alias.clone(),
                            progress,
                            self.repaint_proxy.clone(),
                            self.ui_generation,
                        )?;
                        app.render(window)?;
                        self.connecting = Some(app);
                        display
                            .send(new_display)
                            .map_err(|_| anyhow!("connection owner cancelled window reconnect"))?;
                        window.set_title(&format!("{}{}", crate::VIEWER_TITLE_PREFIX, alias));
                        window.request_redraw();
                        continue;
                    }
                    let writer = match crate::platform::surface::D3D11SurfaceWriter::new() {
                        Ok(writer) => writer,
                        Err(error) => {
                            self.connection_tabs.set_failed(&key, format!("{error:#}"));
                            self.complete_connection(
                                &key,
                                Err(anyhow!("create reconnect surface writer: {error:#}")),
                            );
                            continue;
                        }
                    };
                    if display
                        .send(ViewerDisplayHandle {
                            surface_writer: Some(writer),
                        })
                        .is_err()
                    {
                        self.connection_tabs.set_failed(&key, "重连画面交接已取消");
                        self.complete_connection(
                            &key,
                            Err(anyhow!(
                                "connection owner cancelled reconnect display setup"
                            )),
                        );
                    }
                }
            }
        }
        self.finish_closed_connections();
        Ok(())
    }

    pub(in crate::application::viewer) fn start_playing(
        &mut self,
        connection_key: String,
        session: NativeViewerSession,
    ) -> Result<()> {
        let window = self
            .window
            .take()
            .context("connection/player window is unavailable")?;
        restore_video_window_constraints(&window);
        if let Some((size, maximized, fullscreen)) = self.audio_restore_geometry.take() {
            let _ = window.request_inner_size(size);
            window.set_maximized(maximized);
            crate::ui::chrome::set_fullscreen(&window, fullscreen);
        } else if window
            .inner_size()
            .to_logical::<f64>(window.scale_factor())
            .width
            < 760.0
        {
            let _ = window.request_inner_size(LogicalSize::new(1280.0, 760.0));
        }
        let connecting = self
            .connecting
            .take()
            .context("connection UI is unavailable")?;
        self.connection_tabs.set_ready(&connection_key);
        self.screens = Some(ScreenWindows::new(
            window,
            session,
            connecting,
            self.repaint_proxy.clone(),
            self.ui_generation,
            connection_key,
            self.connection_tabs.clone(),
        )?);
        Ok(())
    }

    pub(in crate::application::viewer) fn fail(
        &mut self,
        event_loop: &ActiveEventLoop,
        message: String,
    ) {
        tracing::error!(%message);
        self.fatal_error = Some(message);
        if let Some(app) = self.playing.as_ref() {
            app.shutdown.store(true, Ordering::Release);
        }
        self.exit(event_loop);
    }
}

pub(in crate::application::viewer) struct WindowsConnectionApp {
    audio: Option<crate::application::viewer::AudioView>,
    pub(in crate::application::viewer) progress: ConnectionProgressApp,
    pub(in crate::application::viewer) presenter: UiPresenter,
    pub(in crate::application::viewer) egui_context: egui::Context,
    pub(in crate::application::viewer) egui_winit: egui_winit::State,
    pub(in crate::application::viewer) close_requested: bool,
    pub(in crate::application::viewer) timing_audit: Option<UiTimingAudit>,
}

impl WindowsConnectionApp {
    pub(in crate::application::viewer) fn new(
        window: &Window,
        alias: String,
        receiver: std_mpsc::Receiver<ConnectionProgress>,
        repaint_proxy: EventLoopProxy<UiEvent>,
        generation: u64,
    ) -> Result<(Self, ViewerDisplayHandle)> {
        let egui_context = egui::Context::default();
        let window_id = window.id();
        egui_context.set_request_repaint_callback(move |info| {
            if info.viewport_id == egui::ViewportId::ROOT
                && let Some(when) = Instant::now().checked_add(info.delay)
            {
                let _ = repaint_proxy.send_event(UiEvent::Repaint(UiRepaintEvent {
                    window: window_id,
                    generation,
                    pass: info.current_cumulative_pass_nr,
                    when,
                }));
            }
        });
        crate::ui::fonts::install(&egui_context);
        configure_viewer_visuals(&egui_context);
        let egui_winit = egui_winit::State::new(
            egui_context.clone(),
            egui_context.viewport_id(),
            window,
            None,
            None,
            None,
        );
        let surface_writer = crate::platform::surface::D3D11SurfaceWriter::new()?;
        let (renderer_device, renderer_context) = surface_writer.create_renderer_device()?;
        let presenter = UiPresenter::new(window, renderer_device, renderer_context)?;
        Ok((
            Self {
                audio: None,
                progress: ConnectionProgressApp::new(alias, receiver),
                presenter,
                egui_context,
                egui_winit,
                close_requested: false,
                timing_audit: None,
            },
            ViewerDisplayHandle {
                surface_writer: Some(surface_writer),
            },
        ))
    }

    pub(in crate::application::viewer) fn on_window_event(
        &mut self,
        window: &Window,
        event: &WindowEvent,
    ) -> Result<()> {
        let response = self.egui_winit.on_window_event(window, event);
        if response.repaint && !matches!(event, WindowEvent::RedrawRequested) {
            window.request_redraw();
        }
        match event {
            WindowEvent::Resized(size) if size.width != 0 && size.height != 0 => {
                self.presenter.resize(*size)?;
            }
            WindowEvent::RedrawRequested => self.render(window)?,
            _ => {}
        }
        Ok(())
    }

    pub(in crate::application::viewer) fn render(&mut self, window: &Window) -> Result<()> {
        self.progress.drain();
        if matches!(
            self.progress.current.state,
            crate::application::viewer::ConnectionProgressState::Failed
        ) {
            self.audio = None;
        }
        let started = Instant::now();
        let input = self.egui_winit.take_egui_input(window);
        let mut resize = None;
        let output = self.egui_context.run_ui(input, |ui| {
            resize = borderless_resize(ui, window, None);
            title_bar_panel(ui, "connection-window-chrome", title_bar_height(), |ui| {
                self.close_requested |=
                    window_title_bar(ui, window, &self.progress.alias, None, self.audio.is_none());
            });
            if let Some(audio) = &mut self.audio {
                self.close_requested |= audio.show(ui);
            } else {
                self.progress.draw(ui);
            }
            crate::ui::controls::show_notices(ui.ctx());
        });
        if let Some(compact) = self
            .audio
            .as_mut()
            .and_then(|audio| audio.take_layout_change())
        {
            configure_audio_window(window, compact);
        }
        let (renderer_output, platform_output, viewports) = egui_directx11::split_output(output);
        let immediate = viewports
            .get(&egui::ViewportId::ROOT)
            .is_some_and(|viewport| viewport.repaint_delay.is_zero());
        self.close_requested |= viewports
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
            .presenter
            .render(&self.egui_context, renderer_output, false)?;
        if presented.ready() && window.is_visible() == Some(false) {
            window.set_visible(true);
        }
        if let Some(audit) = UiTimingAudit::active(&mut self.timing_audit, started) {
            audit.record(
                started,
                layout_elapsed,
                started.elapsed().saturating_sub(layout_elapsed),
                immediate,
                presented.presented(),
            );
        }
        if let Some(direction) = resize {
            let _ = window.drag_resize_window(direction);
        }
        Ok(())
    }
}

fn configure_audio_window(window: &Window, compact: bool) {
    crate::ui::chrome::set_fullscreen(window, None);
    window.set_maximized(false);
    window.set_resizable(false);
    window.set_enabled_buttons(
        winit::window::WindowButtons::CLOSE | winit::window::WindowButtons::MINIMIZE,
    );
    let [width, height] = if compact {
        crate::ui::theme::AUDIO_COMPACT_SIZE
    } else {
        crate::ui::theme::AUDIO_WINDOW_SIZE
    };
    let size = LogicalSize::new(width, height);
    // Clear the previous mode's limits before installing the new fixed size.
    window.set_min_inner_size(None::<LogicalSize<f32>>);
    window.set_max_inner_size(None::<LogicalSize<f32>>);
    let _ = window.request_inner_size(size);
    window.set_min_inner_size(Some(size));
    window.set_max_inner_size(Some(size));
}
fn restore_video_window_constraints(window: &Window) {
    window.set_max_inner_size(None::<LogicalSize<f32>>);
    window.set_resizable(true);
    window.set_enabled_buttons(winit::window::WindowButtons::all());
    window.set_min_inner_size(Some(LogicalSize::new(760.0, 520.0)));
}
