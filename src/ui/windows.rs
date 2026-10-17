use super::gfx::{Retained, UiPresenter, can_hide, create_device, split_output};
use super::window_manager::{self, Event, Repaint, Request};
use super::{AppFactory, AppSession, WindowConfig};
use crate::application::viewer::presenter::ConnectingWindowsRunner;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
#[cfg(target_os = "linux")]
use winit::event::{DeviceEvent, DeviceId};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
#[cfg(windows)]
use winit::platform::windows::EventLoopBuilderExtWindows;
use winit::window::{Window, WindowId};

/// The longest a destroyed (Wayland) control center waits between UI passes:
/// the device center asks for one every 250 ms anyway, as a hidden X11 or
/// Windows window gets.
const HIDDEN_TICK: Duration = Duration::from_millis(250);
/// The shortest, so an animation request cannot spin a window-less pass.
const HIDDEN_MIN_TICK: Duration = Duration::from_millis(50);
/// 通知区域的主人（面板、状态栏）常常比程序晚起来，服务随会话启动时尤其如此，
/// 所以托盘没建出来就按这个间隔接着试，直到它出现。
const TRAY_RETRY: Duration = Duration::from_secs(2);

pub(super) fn run(config: WindowConfig, factory: AppFactory) -> Result<()> {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut builder = EventLoop::<Event>::with_user_event();
    // The Windows keyboard hook needs to see raw messages before winit does.
    #[cfg(windows)]
    builder.with_msg_hook(crate::application::viewer::desktop_input_message);
    let event_loop = builder.build().context("create desktop event loop")?;
    let _input_hook = crate::application::viewer::desktop_input_hook()?;
    let mut runner = Windows {
        main: Runner {
            config,
            factory: Some(factory),
            state: None,
            hidden: None,
            #[cfg(target_os = "linux")]
            activation: None,
            error: None,
            proxy: event_loop.create_proxy(),
            root: true,
            closed: false,
        },
        windows: HashMap::new(),
        viewers: HashMap::new(),
    };
    window_manager::install(Some(event_loop.create_proxy()));
    // Linux 上这个入口只用来开控制中心主窗口（观看窗口与通知窗口走别的 runner），
    // 所以控制通道跟着它一起建立与撤销。
    #[cfg(target_os = "linux")]
    let _control = super::control::listen()
        .inspect_err(|error| tracing::warn!(error = %format!("{error:#}"), "控制通道不可用"))
        .ok();
    #[cfg(target_os = "linux")]
    if let Err(error) = super::shutdown::install() {
        tracing::warn!(error = %format!("{error:#}"), "退出信号处理不可用");
    }
    event_loop
        .run_app(&mut runner)
        .context("run desktop event loop")?;
    window_manager::install(None);
    crate::features::clipboard::shutdown();
    if let Some(error) = runner.main.error.take() {
        bail!(error);
    }
    Ok(())
}

/// 被控角标：托盘图标右上角那个红三角跟着应用报上来的被控状态走。
fn sync_tray(tray: Option<&super::tray::Tray>, controlled: bool) {
    if let Some(tray) = tray {
        tray.set_controlled(controlled);
    }
}

/// 把迟到的托盘接回来。通知区域的主人是桌面会话里的另一个进程，比程序先起来
/// 不是它能保证的事（systemd 那边也只能等到图形会话），而托盘的有无决定窗口
/// 关闭是「收进托盘」还是「结束程序」，所以它得是能恢复的状态，不是启动时的
/// 一次判定。第一次失败由建窗口的地方带着原因记 warn，之后每次只记 debug。
fn retry_tray(tray: &mut Option<super::tray::Tray>, wanted: bool, next: &mut Instant) {
    if !wanted || tray.is_some() || Instant::now() < *next {
        return;
    }
    *next = Instant::now() + TRAY_RETRY;
    match super::tray::Tray::new() {
        Ok(new) => {
            tracing::info!("通知区域可用了，窗口关闭改为收进托盘");
            *tray = Some(new);
        }
        Err(error) => tracing::debug!(error = %format!("{error:#}"), "通知区域仍不可用"),
    }
}

struct Runner {
    root: bool,
    closed: bool,
    config: WindowConfig,
    factory: Option<AppFactory>,
    state: Option<DesktopWindow>,
    /// 主窗口收进托盘后留下的部分；这时 `state` 是 None。
    hidden: Option<Hidden>,
    /// The startup-notification token the next window is created with: a
    /// second start's, when it finds the control center destroyed in the tray.
    #[cfg(target_os = "linux")]
    activation: Option<winit::window::ActivationToken>,
    error: Option<String>,
    proxy: EventLoopProxy<Event>,
}

/// 主窗口收进托盘时留下的部分。被控端、观看会话与后台 worker 都挂在应用上，
/// 应用持有的纹理绑在 egui 上下文上，所以窗口可以销毁，这两样不能。Wayland 没有
/// 「隐藏窗口」，最后一个 `Arc<Window>` 落地就是真正的销毁。
///
/// A hidden X11 or Windows window keeps running its UI pass on the device
/// center's 250 ms request, only without presenting it. A destroyed window
/// does the same here, so nothing the application advances inside `ui()`
/// (worker events, exit handshake, notification cards and their actions,
/// update checks, dialogs queued for later) freezes on Wayland alone.
struct Hidden {
    app: AppSession,
    context: egui::Context,
    tray: Option<super::tray::Tray>,
    /// egui 的纹理和管线都在绘制资源里，丢了就等于所有文字都要重新上载一遍。
    retained: Retained,
    size: Option<PhysicalSize<u32>>,
    /// Wayland 不向客户端提供窗口位置，那里拿不到值。
    position: Option<PhysicalPosition<i32>>,
    /// The last window's viewport, in points, that window-less passes reuse.
    viewport: egui::ViewportInfo,
    screen: egui::Rect,
    /// egui time continues from the last windowed pass.
    time: f64,
    since: Instant,
    next_pass: Instant,
}

/// What a window-less pass asks of the runner.
enum HiddenPass {
    /// Not due yet, or nothing to do until then.
    Wait(Instant),
    /// The application asked for its window, for example to show an error.
    Show,
    /// The exit handshake has finished.
    Exit,
}

impl Hidden {
    /// Run the application's UI pass without a window when it is due. Its
    /// drawing is kept for the next window; a request to show the window
    /// (`Visible`, `Focus`, un-minimize, attention) recreates it.
    fn pass(&mut self) -> HiddenPass {
        let now = Instant::now();
        if now < self.next_pass {
            return HiddenPass::Wait(self.next_pass);
        }
        let mut input = egui::RawInput {
            screen_rect: Some(self.screen),
            time: Some(self.time + self.since.elapsed().as_secs_f64()),
            focused: false,
            ..Default::default()
        };
        input
            .viewports
            .insert(egui::ViewportId::ROOT, self.viewport.clone());
        let Hidden { app, context, .. } = self;
        let output = context.run_ui(input, |ui| {
            app.0.ui(ui);
            super::controls::show_notices(ui.ctx());
        });
        // Platform output (cursor, IME, clipboard, links) only follows user
        // input, which a window-less pass never has.
        let (drawing, _platform, mut viewports) = split_output(output);
        self.retained.defer_output(drawing);
        let mut delay = HIDDEN_TICK;
        let mut show = false;
        if let Some(root) = viewports.remove(&egui::ViewportId::ROOT) {
            delay = root.repaint_delay.clamp(HIDDEN_MIN_TICK, HIDDEN_TICK);
            show = wants_window(&root.commands);
        }
        if self.app.0.exit_ready() {
            return HiddenPass::Exit;
        }
        if show {
            return HiddenPass::Show;
        }
        self.next_pass = now + delay;
        HiddenPass::Wait(self.next_pass)
    }
}

/// Whether a window-less pass asked to be seen: the device center shows its
/// window this way when the exit handshake fails, and a hidden X11 or Windows
/// window would be shown by the same commands.
fn wants_window(commands: &[egui::ViewportCommand]) -> bool {
    commands.iter().any(|command| {
        matches!(
            command,
            egui::ViewportCommand::Visible(true)
                | egui::ViewportCommand::Focus
                | egui::ViewportCommand::Minimized(false)
                | egui::ViewportCommand::RequestUserAttention(_)
        )
    })
}

/// Hides its window when dropped. It is [`DesktopWindow`]'s first field, so
/// every window disappears before the application, the presenter (which
/// detaches the composition tree) and the window itself are released, and
/// the system background cannot show through in between.
struct HideOnDrop(Arc<Window>);

impl Drop for HideOnDrop {
    fn drop(&mut self) {
        self.0.set_visible(false);
    }
}

struct DesktopWindow {
    // Hide before field destruction, then release business and composition
    // resources before destroying the HWND.
    hide_on_drop: HideOnDrop,
    app: AppSession,
    presenter: UiPresenter,
    context: egui::Context,
    input: egui_winit::State,
    viewport: egui::ViewportInfo,
    close_requested: bool,
    /// 关闭请求已经转成「收进托盘」，等这一帧结束由 [`Runner`] 拆掉窗口资源。
    /// Only where the window cannot be hidden in place (Wayland).
    hide_requested: bool,
    tray: Option<super::tray::Tray>,
    /// 托盘还没建出来时在这里接着试，见 [`retry_tray`]。
    wants_tray: bool,
    next_tray_attempt: Instant,
    exiting: bool,
    show_after_present: bool,
    /// Read only on Windows; see its use after the first present.
    #[cfg_attr(not(windows), allow(dead_code))]
    focus_after_present: bool,
    window_move: super::chrome::WindowMoveState,
    window_resize: super::chrome::WindowResizeState,
    min_inner_size: Option<egui::Vec2>,
    next_repaint: Option<Instant>,
    last_frame: Option<Instant>,
    interval: Duration,
    window: Arc<Window>,
    notification: Option<crate::platform::notifications::Placement>,
    floating: bool,
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

    /// 把主窗口收进托盘：窗口放掉，应用、egui 上下文、托盘与绘制资源留下。托盘再
    /// 打开时按收起来之前的尺寸重建窗口，并把这些资源接到新窗口上。Only where
    /// the window cannot be hidden in place; see [`can_hide`].
    // Windows always hides in place, so its `Retained` is uninhabited and the
    // compiler rightly sees everything after `Retained::take` as unreachable
    // there; the destroy-and-recreate path exists for Wayland only.
    #[cfg_attr(windows, allow(unreachable_code, unused_variables, unused_mut))]
    fn hide_main(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        let size = Some(state.window.inner_size());
        // Wayland 不给窗口位置，那里只有尺寸能还原。
        let position = state.window.outer_position().ok();
        let pixels_per_point = egui_winit::pixels_per_point(&state.context, &state.window);
        let screen = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(
                state.window.inner_size().width as f32,
                state.window.inner_size().height as f32,
            ) / pixels_per_point,
        );
        let time = state.context.input(|input| input.time);
        let DesktopWindow {
            hide_on_drop,
            app,
            context,
            tray,
            presenter,
            input,
            mut viewport,
            window,
            ..
        } = state;
        // 绘制资源里装着 egui 的全部纹理，跟应用一起留下；surface 则随窗口放掉。
        let retained = Retained::take(presenter);
        drop(input);
        drop(hide_on_drop);
        drop(window);
        viewport.events.clear();
        viewport.focused = Some(false);
        tracing::info!(?size, "主窗口已收进托盘");
        self.hidden = Some(Hidden {
            app,
            context,
            tray,
            retained,
            size,
            position,
            viewport,
            screen,
            time,
            since: Instant::now(),
            next_pass: Instant::now() + HIDDEN_TICK,
        });
    }

    fn create(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        if self.state.is_some() {
            return Ok(());
        }
        // 收进托盘过的主窗口沿用原来的应用、egui 上下文与绘制资源重建，其余才新建。
        let (context, app, mut tray, retained, restore) = match self.hidden.take() {
            Some(Hidden {
                app,
                context,
                tray,
                retained,
                size,
                position,
                ..
            }) => (
                context,
                Some(app),
                tray,
                Some(retained),
                Some((size, position)),
            ),
            None => {
                let context = egui::Context::default();
                super::fonts::install(&context);
                context.set_embed_viewports(true);
                (context, None, None, None, None)
            }
        };
        // WindowConfig sizes describe page content; the shared caption occupies
        // client space now, so preserve the page's requested and minimum size.
        let mut viewport_builder = self.config.viewport.clone();
        let caption_height = if self.config.notification || self.config.floating {
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
        let viewport_builder = viewport_builder.with_visible(false).with_decorations(false);
        // A window rebuilt for a second start takes that start's token. The
        // first control center window takes the token its launcher left in
        // the environment, so it gets focus and the launch feedback ends. The
        // variables stay set: removing them now would race the threads already
        // reading the environment, and a spent token is worthless to a child.
        #[cfg(target_os = "linux")]
        let window = {
            use winit::platform::startup_notify::EventLoopExtStartupNotify;
            let token = self.activation.take().or_else(|| {
                (self.root && self.factory.is_some())
                    .then(|| event_loop.read_token_from_env())
                    .flatten()
            });
            create_window(&context, event_loop, &viewport_builder, token)?
        };
        #[cfg(not(target_os = "linux"))]
        let window = egui_winit::create_window(&context, event_loop, &viewport_builder)?;
        let window = Arc::new(window);
        super::chrome::configure_dwm_window(&window);
        super::branding::set_taskbar_icon(&window);
        let notification = if self.config.notification {
            Some(crate::platform::notifications::Placement::new(&window)?)
        } else {
            None
        };
        let (restore_size, restore_position) = restore.unwrap_or_default();
        if let Some(size) = restore_size {
            // 还原用物理像素，不经过缩放换算。
            let _ = window.request_inner_size(size);
        }
        if let Some(position) = restore_position {
            window.set_outer_position(position);
        } else if self.config.centered
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
        let graphics = create_device(event_loop, &window)?;
        let presenter = match retained {
            Some(retained) => retained.reuse(window.clone(), &graphics)?,
            None => UiPresenter::new(window.clone(), &graphics)?,
        };
        let input = egui_winit::State::new(
            context.clone(),
            egui::ViewportId::ROOT,
            &*window,
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
        let app = match app {
            Some(app) => app,
            None => {
                let factory = self
                    .factory
                    .take()
                    .context("desktop factory already consumed")?;
                AppSession(factory(&context, Some(graphics.label().to_owned())))
            }
        };
        let uses_tray = self.root && app.0.uses_tray();
        // 再点一次图标要能找到这个窗口（Windows 按 HWND 找进程）。
        if uses_tray && tray.is_none() {
            crate::application::app::instance::register_window(&window)?;
        }
        // 没有通知区域时窗口保留普通的关闭行为，而不是藏到一个够不着的地方；
        // 但托盘可能只是还没起来，所以交给 retry_tray 接着试：它起来了，关闭
        // 就又是收进托盘。托盘建不出来不该拦着窗口本身。
        let next_tray_attempt = Instant::now() + TRAY_RETRY;
        if uses_tray && tray.is_none() {
            match super::tray::Tray::new() {
                Ok(new) => tray = Some(new),
                Err(error) => tracing::warn!(
                    error = %format!("{error:#}"),
                    "通知区域不可用，先按没有托盘运行"
                ),
            }
        }
        let refresh = window
            .current_monitor()
            .and_then(|monitor| monitor.refresh_rate_millihertz())
            .filter(|rate| *rate != 0)
            .unwrap_or(60_000);
        // 主窗口可能一上来就是收进托盘的状态，角标要当场对上。
        sync_tray(tray.as_ref(), app.0.controlled());
        self.state = Some(DesktopWindow {
            hide_on_drop: HideOnDrop(window.clone()),
            app,
            presenter,
            context,
            input,
            viewport,
            close_requested: false,
            hide_requested: false,
            tray,
            wants_tray: uses_tray,
            next_tray_attempt,
            exiting: false,
            // 重建出来的窗口总是要显示的。
            show_after_present: restore.is_some() || self.config.viewport.visible.unwrap_or(true),
            focus_after_present: self.config.viewport.active.unwrap_or(true),
            window_move: Default::default(),
            window_resize: Default::default(),
            min_inner_size,
            next_repaint: Some(Instant::now()),
            last_frame: None,
            interval: Duration::from_secs_f64(1000.0 / f64::from(refresh)),
            window,
            notification,
            floating: self.config.floating,
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
            .is_some_and(|state| state.hide_requested)
        {
            self.hide_main();
        }
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
    /// Show the control center for a launch elsewhere. With the launcher's
    /// token the window is raised past focus-stealing prevention and the
    /// launch feedback ends; without one, or if that fails, it is shown as
    /// the tray's "打开" shows it.
    #[cfg(target_os = "linux")]
    fn activate(&mut self, token: &crate::platform::linux::startup_notify::LaunchToken) {
        let raised = token.for_window(&self.window).is_some_and(|value| {
            let window = &self.window;
            crate::platform::linux::startup_notify::raise(window, value, || {
                window.set_visible(true);
                window.set_minimized(false);
            })
            .inspect_err(|error| {
                tracing::warn!(error = %format!("{error:#}"), "无法用启动通知令牌激活控制中心")
            })
            .is_ok()
        });
        if !raised {
            self.window.set_visible(true);
            self.window.set_minimized(false);
            self.window.focus_window();
        }
        self.context.request_repaint();
    }

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
        // 标题栏提示与托盘角标共用这一次判定。
        let alert = self.app.0.title_bar_alert();
        let controlled = alert.is_some();
        retry_tray(&mut self.tray, self.wants_tray, &mut self.next_tray_attempt);
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
            if self.notification.is_none() && !self.floating && self.window.fullscreen().is_none() {
                super::chrome::title_bar_panel_alert(
                    ui,
                    "desktop-window-chrome",
                    super::chrome::title_bar_height(),
                    controlled,
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
        // 收进托盘后窗口不再呈现，但这一趟 UI 照跑，角标跟着走。
        sync_tray(self.tray.as_ref(), controlled);
        let (drawing, platform, mut viewports) = split_output(output);
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
        // 有托盘时，关闭按钮、Alt+F4 与 Mod+Q 的语义是收进托盘，不是结束程序；
        // 托盘里的「退出」才会走 Request::Exit 的后台退出握手。
        if self.close_requested && self.tray.is_some() && !self.exiting && !self.app.0.exit_ready()
        {
            self.close_requested = false;
            self.show_after_present = false;
            // Windows 的窗口能直接隐藏，而且必须留着：更新器和通知激活都按
            // 控制中心 HWND 找进程；X11 也能直接隐藏。Wayland 的 set_visible
            // 是空操作，只能销毁窗口和它的 surface，等托盘再打开时重建。
            if can_hide(&self.window) {
                self.window.set_visible(false);
            } else {
                self.hide_requested = true;
            }
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
                // X11 and Wayland window managers apply their own focus
                // policy to a newly mapped window.
                #[cfg(windows)]
                if std::mem::take(&mut self.focus_after_present) {
                    // Respect Windows foreground permission. winit's Windows
                    // focus_window() synthesizes Alt input; activation here
                    // needs only the permission granted by our launcher.
                    if let Ok(hwnd) = super::gfx::window_hwnd(&self.window) {
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
        } else if let Some(hidden) = &mut self.hidden {
            match hidden.pass() {
                HiddenPass::Wait(at) => event_loop.set_control_flow(ControlFlow::WaitUntil(at)),
                HiddenPass::Show => {
                    if let Err(error) = self.create(event_loop) {
                        self.fail(event_loop, error);
                        return;
                    }
                    event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now()));
                }
                HiddenPass::Exit => {
                    // 托盘的「退出」在隐藏状态下完成，不必再建一个窗口。
                    self.closed = true;
                    if self.root {
                        event_loop.exit();
                    }
                    return;
                }
            }
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
        self.finish_close(event_loop);
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.state.take();
        self.hidden.take();
    }
}

/// `egui_winit::create_window`, giving the new window the launcher's
/// startup-notification token when there is one. (egui_winit's own placement
/// on a numbered monitor only applies to fullscreen viewports, which the shell
/// windows are not.)
#[cfg(target_os = "linux")]
fn create_window(
    context: &egui::Context,
    event_loop: &ActiveEventLoop,
    builder: &egui::ViewportBuilder,
    token: Option<winit::window::ActivationToken>,
) -> Result<Window, winit::error::OsError> {
    use winit::platform::startup_notify::WindowAttributesExtStartupNotify;
    let Some(token) = token else {
        return egui_winit::create_window(context, event_loop, builder);
    };
    let attributes = egui_winit::create_winit_window_attributes(context, builder.clone())
        .with_activation_token(token);
    let window = event_loop.create_window(attributes)?;
    egui_winit::apply_viewport_builder_to_window(context, &window, builder);
    Ok(window)
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
    #[cfg(target_os = "linux")]
    fn device_event(&mut self, event_loop: &ActiveEventLoop, device: DeviceId, event: DeviceEvent) {
        // Raw motion has no WindowId. Only the focused viewer may send it;
        // broadcasting it would move the pointer in several remote sessions.
        if let Some(viewer) = self.viewers.values_mut().find(|v| v.runner.has_focus()) {
            viewer.runner.device_event(event_loop, device, event);
        }
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
                } else if self.main.hidden.is_some()
                    && let Err(error) = self.main.create(event_loop)
                {
                    self.main.fail(event_loop, error);
                }
            }
            #[cfg(target_os = "linux")]
            Event::Request(Request::ActivateMain(token)) => {
                if let Some(state) = &mut self.main.state {
                    state.activate(&token);
                } else if self.main.hidden.is_some() {
                    // Destroyed in the tray (Wayland): the rebuilt window takes
                    // the launcher's token at creation, as a first start does.
                    use winit::platform::wayland::ActiveEventLoopExtWayland;
                    let token = if event_loop.is_wayland() {
                        token.wayland
                    } else {
                        token.x11
                    };
                    self.main.activation = token.map(winit::window::ActivationToken::from_raw);
                    if let Err(error) = self.main.create(event_loop) {
                        self.main.fail(event_loop, error);
                    }
                }
            }
            Event::Request(Request::Exit) => {
                if let Some(state) = &mut self.main.state {
                    state.exiting = true;
                    state.close_requested = true;
                    if let Err(error) = state.render() {
                        self.main.fail(event_loop, error);
                    }
                } else if let Some(hidden) = &mut self.main.hidden {
                    // 已经收进托盘：不必为了退出再建一个窗口，请后台开始收尾，
                    // 退出事件循环的时机交给 about_to_wait 的窗口外 UI 轮次。
                    if hidden.app.0.on_close_requested() {
                        self.main.closed = true;
                        event_loop.exit();
                    } else {
                        hidden.next_pass = Instant::now();
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
                    // 只有控制中心主窗口会收进托盘。
                    hidden: None,
                    #[cfg(target_os = "linux")]
                    activation: None,
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
                if done.is_closed() {
                    return;
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A pass run without a window, as [`Hidden::pass`] runs it.
    fn windowless_pass(
        context: &egui::Context,
        ui: impl FnMut(&mut egui::Ui),
    ) -> Vec<egui::ViewportCommand> {
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1180.0, 760.0),
            )),
            time: Some(1.0),
            focused: false,
            ..Default::default()
        };
        input
            .viewports
            .insert(egui::ViewportId::ROOT, egui::ViewportInfo::default());
        let mut output = context.run_ui(input, ui);
        // The font atlas; a real pass keeps it for the next window.
        output.textures_delta.clear();
        output
            .viewport_output
            .remove(&egui::ViewportId::ROOT)
            .map(|root| root.commands)
            .unwrap_or_default()
    }

    #[test]
    fn a_windowless_pass_reports_requests_to_show_the_window() {
        let context = egui::Context::default();
        // The device center's failed exit handshake, raised inside `ui()`.
        let commands = windowless_pass(&context, |ui| {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Visible(true));
        });
        assert!(wants_window(&commands));
        // A command queued between passes (another thread, a callback)
        // arrives with the next one.
        context.send_viewport_cmd(egui::ViewportCommand::Focus);
        assert!(wants_window(&windowless_pass(&context, |_| {})));
        // Nothing asked: the window stays destroyed.
        assert!(!wants_window(&windowless_pass(&context, |_| {})));
        // Closing is the exit handshake's business, not a reason to show.
        let commands = windowless_pass(&context, |ui| {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Visible(false));
        });
        assert!(!wants_window(&commands));
    }
}
