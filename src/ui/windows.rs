use super::d3d11::UiPresenter;
use super::window_manager::{self, Event, Repaint, Request};
use super::{AppFactory, AppSession, WindowConfig};
use crate::application::viewer::windows_presenter::ConnectingWindowsRunner;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter1, IDXGIDevice};
use windows::core::Interface;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::platform::windows::EventLoopBuilderExtWindows;
use winit::window::{Window, WindowId};

pub(super) fn run(config: WindowConfig, factory: AppFactory) -> Result<()> {
    let mut builder = EventLoop::<Event>::with_user_event();
    builder.with_msg_hook(crate::application::viewer::desktop_input_message);
    let event_loop = builder.build().context("create desktop event loop")?;
    let _input_hook = crate::application::viewer::desktop_input_hook()?;
    let mut runner = Windows {
        main: Runner {
            config,
            factory: Some(factory),
            state: None,
            error: None,
            proxy: event_loop.create_proxy(),
            root: true,
            closed: false,
        },
        windows: HashMap::new(),
        viewers: HashMap::new(),
    };
    window_manager::install(Some(event_loop.create_proxy()));
    event_loop
        .run_app(&mut runner)
        .context("run D3D11 desktop event loop")?;
    window_manager::install(None);
    crate::features::clipboard::shutdown();
    if let Some(error) = runner.main.error.take() {
        bail!(error);
    }
    Ok(())
}

struct Runner {
    root: bool,
    closed: bool,
    config: WindowConfig,
    factory: Option<AppFactory>,
    state: Option<DesktopWindow>,
    error: Option<String>,
    proxy: EventLoopProxy<Event>,
}

struct DesktopWindow {
    // Hide before field destruction, then release business and composition
    // resources before destroying the HWND.
    app: AppSession,
    presenter: UiPresenter,
    context: egui::Context,
    input: egui_winit::State,
    viewport: egui::ViewportInfo,
    close_requested: bool,
    tray: Option<super::tray::Tray>,
    exiting: bool,
    show_after_present: bool,
    focus_after_present: bool,
    window_move: super::chrome::WindowMoveState,
    window_resize: super::chrome::WindowResizeState,
    min_inner_size: Option<egui::Vec2>,
    next_repaint: Option<Instant>,
    last_frame: Option<Instant>,
    interval: Duration,
    window: Window,
    notification: Option<crate::platform::windows::notifications::Placement>,
}

impl Drop for DesktopWindow {
    fn drop(&mut self) {
        // UiPresenter detaches the composition tree in Drop. Hide the window
        // first so its system background cannot appear during that interval.
        self.window.set_visible(false);
    }
}

impl Runner {
    fn fail(&mut self, event_loop: &ActiveEventLoop, error: anyhow::Error) {
        self.error = Some(format!("{error:#}"));
        self.state.take();
        self.closed = true;
        if self.root {
            event_loop.exit();
        }
    }

    fn create(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        if self.state.is_some() {
            return Ok(());
        }
        let context = egui::Context::default();
        super::fonts::install(&context);
        context.set_embed_viewports(true);
        // WindowConfig sizes describe page content; the shared caption occupies
        // client space now, so preserve the page's requested and minimum size.
        let mut viewport_builder = self.config.viewport.clone();
        let caption_height = if self.config.notification {
            0.0
        } else {
            super::chrome::title_bar_height()
        };
        if let Some(size) = &mut viewport_builder.inner_size {
            size.y += caption_height;
        }
        if let Some(size) = &mut viewport_builder.min_inner_size {
            size.y += caption_height;
        }
        if let Some(size) = &mut viewport_builder.max_inner_size {
            size.y += caption_height;
        }
        let min_inner_size = viewport_builder.min_inner_size;
        let window = egui_winit::create_window(
            &context,
            event_loop,
            &viewport_builder.with_visible(false).with_decorations(false),
        )?;
        super::chrome::configure_dwm_window(&window);
        super::branding::set_taskbar_icon(&window);
        let notification = if self.config.notification {
            Some(crate::platform::windows::notifications::Placement::new(
                &window,
            )?)
        } else {
            None
        };
        if self.config.centered
            && let Some(monitor) = window.current_monitor()
        {
            let outer = window.outer_size();
            let size = monitor.size();
            let origin = monitor.position();
            window.set_outer_position(PhysicalPosition::new(
                origin.x + (size.width as i64 - outer.width as i64).max(0) as i32 / 2,
                origin.y + (size.height as i64 - outer.height as i64).max(0) as i32 / 2,
            ));
        }
        let (device, immediate, graphics) = create_device()?;
        let presenter = UiPresenter::new(&window, device, immediate)?;
        let input = egui_winit::State::new(
            context.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );
        let mut viewport = egui::ViewportInfo::default();
        egui_winit::update_viewport_info(&mut viewport, &context, &window, true);
        let proxy = self.proxy.clone();
        let window_id = window.id();
        context.set_request_repaint_callback(move |info| {
            if info.viewport_id == egui::ViewportId::ROOT
                && let Some(when) = Instant::now().checked_add(info.delay)
            {
                let _ = proxy.send_event(Event::Repaint(Repaint {
                    window: window_id,
                    generation: 0,
                    pass: info.current_cumulative_pass_nr,
                    when,
                }));
            }
        });
        let factory = self
            .factory
            .take()
            .context("desktop factory already consumed")?;
        let app = AppSession(factory(&context, Some(graphics)));
        let uses_tray = self.root && app.0.uses_tray();
        if uses_tray {
            crate::application::app::instance::register_window(super::d3d11::window_hwnd(
                &window,
            )?)?;
        }
        let refresh = window
            .current_monitor()
            .and_then(|monitor| monitor.refresh_rate_millihertz())
            .filter(|rate| *rate != 0)
            .unwrap_or(60_000);
        self.state = Some(DesktopWindow {
            app,
            presenter,
            context,
            input,
            viewport,
            close_requested: false,
            tray: if uses_tray {
                Some(super::tray::Tray::new()?)
            } else {
                None
            },
            exiting: false,
            show_after_present: self.config.viewport.visible.unwrap_or(true),
            focus_after_present: self.config.viewport.active.unwrap_or(true),
            window_move: Default::default(),
            window_resize: Default::default(),
            min_inner_size,
            next_repaint: Some(Instant::now()),
            last_frame: None,
            interval: Duration::from_secs_f64(1000.0 / f64::from(refresh)),
            window,
            notification,
        });
        if self.root {
            crate::application::bootstrap::ready();
        }
        let state = self.state.as_mut().expect("created desktop state");
        state.render()?;
        state.app.0.on_focus_changed(state.window.has_focus());
        self.finish_close(event_loop);
        Ok(())
    }

    fn finish_close(&mut self, event_loop: &ActiveEventLoop) {
        if self
            .state
            .as_ref()
            .is_some_and(|state| state.close_requested)
        {
            self.closed = true;
            if self.root {
                event_loop.exit();
            }
        }
    }
}

impl DesktopWindow {
    fn schedule(&mut self, when: Instant) {
        let when = self
            .last_frame
            .map_or(when, |last| when.max(last + self.interval));
        self.next_repaint = Some(self.next_repaint.map_or(when, |old| old.min(when)));
    }

    fn render(&mut self) -> Result<()> {
        self.next_repaint = None;
        self.last_frame = Some(Instant::now());
        if let Some(placement) = &mut self.notification {
            placement.refresh(&self.window);
        }
        egui_winit::update_viewport_info(&mut self.viewport, &self.context, &self.window, false);
        let mut input = self.input.take_egui_input(&self.window);
        input
            .viewports
            .insert(egui::ViewportId::ROOT, self.viewport.clone());
        if self.close_requested {
            input
                .viewports
                .get_mut(&egui::ViewportId::ROOT)
                .expect("root viewport")
                .events
                .push(egui::ViewportEvent::Close);
        }
        self.viewport.events.clear();
        self.window_resize.min_size = self.min_inner_size.map(|size| {
            winit::dpi::LogicalSize::new(size.x, size.y).to_physical(self.window.scale_factor())
        });
        let output = self.context.run_ui(input, |ui| {
            let ctx = ui.ctx().clone();
            super::chrome::resize_regions(ui, &self.window, |response, direction| {
                super::chrome::update_nonmodal_window_resize(
                    &ctx,
                    &self.window,
                    response,
                    direction,
                    &mut self.window_resize,
                    None,
                );
            });
            if self.notification.is_none() && self.window.fullscreen().is_none() {
                let alert = self.app.0.title_bar_alert();
                super::chrome::title_bar_panel_alert(
                    ui,
                    "desktop-window-chrome",
                    super::chrome::title_bar_height(),
                    alert.is_some(),
                    |ui| {
                        if super::chrome::window_title_bar_alert(
                            ui,
                            &self.window,
                            &self.window.title(),
                            Some(&mut self.window_move),
                            true,
                            alert.as_ref(),
                        ) {
                            self.exiting = false;
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    },
                );
            }
            self.app.0.ui(ui);
            super::controls::show_notices(ui.ctx());
        });
        let (drawing, platform, mut viewports) = egui_directx11::split_output(output);
        self.input.handle_platform_output(&self.window, platform);
        if let Some(root) = viewports.remove(&egui::ViewportId::ROOT) {
            for command in &root.commands {
                match command {
                    egui::ViewportCommand::Close => self.close_requested = true,
                    egui::ViewportCommand::CancelClose => self.close_requested = false,
                    _ => {}
                }
            }
            let mut actions = Vec::new();
            egui_winit::process_viewport_commands(
                &self.context,
                &mut self.viewport,
                root.commands.into_iter().filter(|command| {
                    !matches!(
                        command,
                        egui::ViewportCommand::Close | egui::ViewportCommand::CancelClose
                    )
                }),
                &self.window,
                &mut actions,
            );
            for action in actions {
                let event = match action {
                    egui_winit::ActionRequested::Cut => Some(egui::Event::Cut),
                    egui_winit::ActionRequested::Copy => Some(egui::Event::Copy),
                    egui_winit::ActionRequested::Paste => self
                        .input
                        .clipboard_text()
                        .map(|text| text.replace("\r\n", "\n"))
                        .filter(|text| !text.is_empty())
                        .map(egui::Event::Paste),
                    // No current application screen requests framebuffer screenshots.
                    egui_winit::ActionRequested::Screenshot(_) => None,
                };
                if let Some(event) = event {
                    self.input.egui_input_mut().events.push(event);
                    self.schedule(Instant::now());
                }
            }
            if let Some(when) = Instant::now().checked_add(root.repaint_delay) {
                self.schedule(when);
            }
        }
        // Both native CloseRequested/Alt+F4 and the custom caption command
        // reach this point before any resources or business windows are closed.
        if self.close_requested && self.tray.is_some() && !self.exiting && !self.app.0.exit_ready()
        {
            self.close_requested = false;
            self.show_after_present = false;
            self.window.set_visible(false);
            self.app.0.on_focus_changed(false);
        }
        if self.close_requested && !self.app.0.on_close_requested() {
            self.close_requested = false;
            self.context.request_repaint();
        }
        if let Some(size) = self.window_resize.requested_render_size.take() {
            self.presenter.resize(size)?;
        }
        if !self.close_requested
            && (self.show_after_present || self.window.is_visible() != Some(false))
            && self.window.is_minimized() != Some(true)
        {
            let presented = self.presenter.render(&self.context, drawing, false)?;
            if presented.ready() && self.show_after_present {
                self.show_after_present = false;
                self.window.set_visible(true);
                if std::mem::take(&mut self.focus_after_present) {
                    // Respect Windows foreground permission. winit's Windows
                    // focus_window() synthesizes Alt input; activation here
                    // needs only the permission granted by our launcher.
                    if let Ok(hwnd) = super::d3d11::window_hwnd(&self.window) {
                        let activated = unsafe {
                            windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd)
                        }
                        .as_bool();
                        tracing::debug!(activated, "initial window foreground activation");
                    }
                }
            }
        } else if !self.close_requested {
            // QR/font updates must survive a minimized window and upload on restore.
            self.presenter.defer_output(drawing);
        }
        Ok(())
    }
}

impl ApplicationHandler<Event> for Runner {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(error) = self.create(event_loop) {
            self.fail(event_loop, error);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut().filter(|state| state.window.id() == id) else {
            return;
        };
        let response = state.input.on_window_event(&state.window, &event);
        let result = match event {
            WindowEvent::Focused(focused) => {
                if !focused {
                    super::chrome::cancel_pointer_operation(
                        &mut state.window_move,
                        &mut state.window_resize,
                    );
                }
                state.app.0.on_focus_changed(focused);
                state.schedule(Instant::now());
                Ok(())
            }
            WindowEvent::CloseRequested => {
                state.exiting = false;
                state.close_requested = true;
                state.render()
            }
            WindowEvent::Destroyed => {
                self.closed = true;
                if self.root {
                    event_loop.exit();
                }
                Ok(())
            }
            WindowEvent::Resized(size) => {
                state.schedule(Instant::now());
                if let Some(placement) = &mut state.notification {
                    placement.refresh(&state.window);
                }
                state.presenter.resize(size)
            }
            WindowEvent::Moved(_) | WindowEvent::ScaleFactorChanged { .. }
                if state.notification.is_some() =>
            {
                state.schedule(Instant::now());
                Ok(())
            }
            WindowEvent::RedrawRequested => state.render(),
            _ => {
                if response.repaint {
                    state.schedule(Instant::now());
                }
                Ok(())
            }
        };
        if let Err(error) = result {
            self.fail(event_loop, error);
            return;
        }
        self.finish_close(event_loop);
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: Event) {
        let Event::Repaint(event) = event else {
            return;
        };
        if let Some(state) = &mut self.state {
            if state.window.id() != event.window {
                return;
            }
            let current = state.context.cumulative_pass_nr();
            if current == event.pass || current == event.pass.saturating_add(1) {
                state.schedule(event.when);
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(state) = &mut self.state {
            if let Some(when) = state.next_repaint {
                if when <= Instant::now() {
                    state.next_repaint = None;
                    // WM_PAINT is not a reliable completion signal for covered
                    // or minimized windows. Run the due UI pass here so worker
                    // events and the next deadline do not depend on mouse input.
                    // render() already defers presentation for hidden/minimized
                    // windows; native RedrawRequested still handles exposure.
                    if let Err(error) = state.render() {
                        self.fail(event_loop, error);
                        return;
                    }
                    event_loop.set_control_flow(
                        state
                            .next_repaint
                            .map_or(ControlFlow::Wait, ControlFlow::WaitUntil),
                    );
                } else {
                    event_loop.set_control_flow(ControlFlow::WaitUntil(when));
                }
            } else {
                event_loop.set_control_flow(ControlFlow::Wait);
            }
        }
        self.finish_close(event_loop);
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.state.take();
    }
}

struct ManagedViewer {
    runner: ConnectingWindowsRunner,
    done: Option<tokio::sync::oneshot::Sender<Result<()>>>,
}
impl ManagedViewer {
    fn finish(&mut self, event_loop: &ActiveEventLoop) {
        self.runner.exiting(event_loop);
        if let Some(done) = self.done.take() {
            let _ = done.send(
                self.runner
                    .error()
                    .map_or(Ok(()), |error| Err(anyhow!(error))),
            );
        }
    }
}
struct Windows {
    windows: HashMap<String, Runner>,
    viewers: HashMap<String, ManagedViewer>,
    main: Runner,
}
impl Windows {
    fn retire(&mut self, event_loop: &ActiveEventLoop) {
        self.windows.retain(|_, window| !window.closed);
        self.viewers.retain(|_, viewer| {
            if viewer.runner.closed() {
                viewer.finish(event_loop);
                false
            } else {
                true
            }
        });
    }
    fn focus(&self, key: &str) {
        if let Some(window) = self.windows.get(key).and_then(|r| r.state.as_ref()) {
            if window.show_after_present {
                window.window.request_redraw();
                return;
            }
            window.window.set_minimized(false);
            window.window.set_visible(true);
            window.window.focus_window();
        }
        if let Some(viewer) = self.viewers.get(key) {
            viewer.runner.focus();
        }
    }
}
impl ApplicationHandler<Event> for Windows {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.main.resumed(event_loop);
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if self
            .main
            .state
            .as_ref()
            .is_some_and(|s| s.window.id() == id)
        {
            self.main.window_event(event_loop, id, event);
        } else if let Some(window) = self
            .windows
            .values_mut()
            .find(|w| w.state.as_ref().is_some_and(|s| s.window.id() == id))
        {
            window.window_event(event_loop, id, event);
        } else if let Some(viewer) = self.viewers.values_mut().find(|v| v.runner.owns(id)) {
            viewer.runner.window_event(event_loop, id, event);
        }
        self.retire(event_loop);
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
        match event {
            Event::Repaint(event) => {
                if self
                    .main
                    .state
                    .as_ref()
                    .is_some_and(|s| s.window.id() == event.window)
                {
                    self.main.user_event(event_loop, Event::Repaint(event));
                } else if let Some(window) = self.windows.values_mut().find(|w| {
                    w.state
                        .as_ref()
                        .is_some_and(|s| s.window.id() == event.window)
                }) {
                    window.user_event(event_loop, Event::Repaint(event));
                } else if let Some(viewer) = self
                    .viewers
                    .values_mut()
                    .find(|v| v.runner.owns(event.window))
                {
                    viewer.runner.user_event(event_loop, Event::Repaint(event));
                }
            }
            Event::Request(Request::ShowMain) => {
                if let Some(state) = &mut self.main.state {
                    state.window.set_visible(true);
                    state.window.set_minimized(false);
                    state.window.focus_window();
                    state.context.request_repaint();
                }
            }
            Event::Request(Request::Exit) => {
                if let Some(state) = &mut self.main.state {
                    state.exiting = true;
                    state.close_requested = true;
                    if let Err(error) = state.render() {
                        self.main.fail(event_loop, error);
                    }
                }
                self.main.finish_close(event_loop);
            }
            Event::Request(Request::Focus(key)) => self.focus(&key),
            Event::Request(Request::Open {
                key,
                config,
                factory,
            }) => {
                if self.windows.contains_key(&key) {
                    if !config.notification {
                        self.focus(&key);
                    }
                    return;
                }
                let mut window = Runner {
                    config,
                    factory: Some(factory),
                    state: None,
                    error: None,
                    proxy: self.main.proxy.clone(),
                    root: false,
                    closed: false,
                };
                window.resumed(event_loop);
                if !window.closed {
                    self.windows.insert(key, window);
                }
            }
            Event::Request(Request::Viewer { key, config, done }) => {
                if self.viewers.contains_key(&key) {
                    self.focus(&key);
                    let _ = done.send(Err(anyhow!("观看窗口已打开")));
                    return;
                }
                let mut viewer = ManagedViewer {
                    runner: ConnectingWindowsRunner::new(
                        config,
                        true,
                        self.main.proxy.clone(),
                        true,
                    ),
                    done: Some(done),
                };
                viewer.runner.resumed(event_loop);
                self.viewers.insert(key, viewer);
            }
        }
        self.retire(event_loop);
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let mut wake = None::<Instant>;
        self.main.about_to_wait(event_loop);
        if let ControlFlow::WaitUntil(at) = event_loop.control_flow() {
            wake = Some(at);
        }
        for window in self.windows.values_mut() {
            event_loop.set_control_flow(ControlFlow::Wait);
            window.about_to_wait(event_loop);
            if let ControlFlow::WaitUntil(at) = event_loop.control_flow() {
                wake = Some(wake.map_or(at, |old| old.min(at)));
            }
        }
        for viewer in self.viewers.values_mut() {
            event_loop.set_control_flow(ControlFlow::Wait);
            viewer.runner.about_to_wait(event_loop);
            if let ControlFlow::WaitUntil(at) = event_loop.control_flow() {
                wake = Some(wake.map_or(at, |old| old.min(at)));
            }
        }
        self.retire(event_loop);
        event_loop.set_control_flow(wake.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        window_manager::install(None);
        for viewer in self.viewers.values_mut() {
            viewer.finish(event_loop);
        }
        self.viewers.clear();
        self.windows.clear();
        self.main.exiting(event_loop);
    }
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext, String)> {
    let mut last_error = None;
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut device = None;
        let mut context = None;
        let result = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[
                    D3D_FEATURE_LEVEL_11_0,
                    D3D_FEATURE_LEVEL_10_1,
                    D3D_FEATURE_LEVEL_10_0,
                ]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        match result {
            Ok(()) => {
                let device = device.context("D3D11 did not return a GUI device")?;
                let context = context.context("D3D11 did not return a GUI context")?;
                let adapter: IDXGIAdapter1 =
                    unsafe { device.cast::<IDXGIDevice>()?.GetAdapter() }?.cast()?;
                let desc = unsafe { adapter.GetDesc1() }?;
                let length = desc
                    .Description
                    .iter()
                    .position(|value| *value == 0)
                    .unwrap_or(desc.Description.len());
                let graphics = format!(
                    "{} · D3D11",
                    String::from_utf16_lossy(&desc.Description[..length])
                );
                return Ok((device, context, graphics));
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(anyhow!("create GUI D3D11 device: {:?}", last_error))
}
