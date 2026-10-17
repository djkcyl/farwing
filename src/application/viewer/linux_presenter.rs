//! Player window for X11 and Wayland.
//!
//! The Windows player composes DXVA surfaces through DirectComposition and
//! takes input from a raw-input hook. Neither exists here, so this presenter is
//! built the other way round: frames land on the CPU (software decode), the
//! wgpu shell draws them under the egui chrome, and input comes from the winit
//! events the compositor delivers to the focused window.
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{
    DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowAttributes, WindowId};

use crate::features::remote_input::MouseMode;
use crate::features::stream_control::RemoteScreen;
use crate::media::decoder::RenderSurface;
use crate::platform::graphics::VideoPlacement;
use crate::ui::chrome::{
    WindowMoveState, WindowResizeState, cancel_pointer_operation, configure_dwm_window,
    resize_regions, set_fullscreen, title_bar_height, title_bar_panel,
    update_nonmodal_window_resize, window_title_bar,
};
use crate::ui::controls::{ViewerCaptionIcon, viewer_caption_button as caption_button};
use crate::ui::gfx::{UiPresenter, UiTimingAudit, split_output};
use crate::ui::window_manager::{Event as UiEvent, Repaint as UiRepaintEvent};

use super::{
    ConnectionProgress, ConnectionProgressApp, DecodedVideoFrame, NativeViewerSession,
    PerformancePanelMode, StreamControlUi, ViewerDisplayHandle, ViewerWindowEvent,
    configure_viewer_visuals, mutex_lock, show_stream_control_window, take_next_frame,
};

pub(super) use crate::application::viewer_shortcuts::Action as ViewerShortcut;

/// Space the caption keeps for the brand logo and the device alias.
const TITLE_CAPTION_WIDTH: f32 = 220.0;

pub(crate) struct ConnectingWindowsRunConfig {
    pub alias: String,
    pub progress: std_mpsc::Receiver<ConnectionProgress>,
    pub session: std_mpsc::Receiver<ViewerWindowEvent>,
    pub display_sender: tokio::sync::oneshot::Sender<ViewerDisplayHandle>,
}

pub(super) fn run(session: NativeViewerSession) -> Result<()> {
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
            alias,
            progress: std_mpsc::channel().1,
            session: receiver,
            display_sender: tokio::sync::oneshot::channel().0,
        },
        false,
    )
}

pub(super) fn run_connecting(config: ConnectingWindowsRunConfig) -> Result<()> {
    run_player(config, true)
}

fn run_player(config: ConnectingWindowsRunConfig, needs_display: bool) -> Result<()> {
    let event_loop = EventLoop::<UiEvent>::with_user_event()
        .build()
        .context("create player event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut runner =
        ConnectingWindowsRunner::new(config, needs_display, event_loop.create_proxy(), false);
    event_loop
        .run_app(&mut runner)
        .map_err(|error| anyhow!("run connection/player event loop: {error}"))?;
    if let Some(error) = runner.fatal_error.take() {
        bail!(error);
    }
    Ok(())
}

fn ui_frame_interval(window: &Window) -> Duration {
    let millihertz = window
        .current_monitor()
        .and_then(|monitor| monitor.refresh_rate_millihertz())
        .filter(|rate| *rate != 0)
        .unwrap_or(60_000);
    Duration::from_secs_f64(1000.0 / f64::from(millihertz))
}

pub(crate) struct ConnectingWindowsRunner {
    embedded: bool,
    alias: String,
    attributes: WindowAttributes,
    progress: Option<std_mpsc::Receiver<ConnectionProgress>>,
    session: std_mpsc::Receiver<ViewerWindowEvent>,
    display_sender: Option<tokio::sync::oneshot::Sender<ViewerDisplayHandle>>,
    window: Option<Arc<Window>>,
    shell: Option<Shell>,
    stage: Stage,
    close_requested: bool,
    fatal_error: Option<String>,
    next_repaint: Option<Instant>,
    last_frame: Option<Instant>,
    ui_frame_interval: Duration,
    proxy: EventLoopProxy<UiEvent>,
    generation: u64,
    /// The audio-only view, shown in place of the connection progress.
    audio: Option<super::AudioView>,
    /// Window geometry to restore when an audio-only session gets video.
    audio_restore_geometry: Option<(
        winit::dpi::PhysicalSize<u32>,
        bool,
        Option<winit::window::Fullscreen>,
    )>,
    /// 所有播放窗口共享的屏目录与解码器池，从第一条 Playing 会话取出来。
    screens: Option<super::screens::ScreenPlayback>,
    /// 播放窗口发给 runner 的窗口级请求。
    requests: std_mpsc::Sender<WindowRequest>,
    request_rx: std_mpsc::Receiver<WindowRequest>,
    /// 把屏拖出得到的独立窗口。窗口创建与销毁都留在事件循环线程上。
    detached: Vec<DetachedWindow>,
    /// 主窗口要求关闭。还有屏窗口在播时它只是关掉自己，由其中一个接替。
    primary_close: bool,
}

enum Stage {
    Connecting(Box<ConnectionProgressApp>),
    Playing(Box<Player>),
    Finished,
}

/// egui plus the wgpu surface for the one player window.
struct Shell {
    context: egui::Context,
    input: egui_winit::State,
    presenter: UiPresenter,
    move_state: WindowMoveState,
    resize_state: WindowResizeState,
    /// The caption floats over the video in fullscreen, as on Windows.
    fullscreen_chrome: super::fullscreen_chrome::FullscreenChrome,
    timing_audit: Option<UiTimingAudit>,
}

/// The chrome state a window keeps between frames, borrowed while its egui
/// pass runs.
struct WindowChrome<'a> {
    move_state: &'a mut WindowMoveState,
    resize_state: &'a mut WindowResizeState,
    fullscreen: &'a mut super::fullscreen_chrome::FullscreenChrome,
}

/// 缩放热区和标题栏（全屏时是浮动栏），以及播放器放在栏里的屏标签与按钮。主窗口
/// 和独立窗口画的是同一套，返回点没点关闭。`tooltips` 只有主窗口画音频视图时关掉。
fn draw_window_chrome(
    ui: &mut egui::Ui,
    window: &Arc<Window>,
    title: &str,
    panel: &'static str,
    tooltips: bool,
    chrome: WindowChrome<'_>,
    mut player: Option<&mut Player>,
) -> bool {
    let WindowChrome {
        move_state,
        resize_state,
        fullscreen: floating,
    } = chrome;
    let ctx = ui.ctx().clone();
    resize_regions(ui, window, |response, direction| {
        update_nonmodal_window_resize(&ctx, window, response, direction, resize_state, None);
    });
    let fullscreen = window.fullscreen().is_some();
    floating.sync(fullscreen);
    if let Some(player) = player.as_deref_mut() {
        // 用上一帧的浮动栏矩形判断，和 Windows 的 display_transition 一样；
        // 指针在栏上时本机接管输入，按键不再发给远端。主窗口和独立窗口都走这里。
        let relative = player.input().relative_mode();
        player.chrome_hovered = floating.owns_pointer(&ctx, relative);
    }
    // Windows 的 caption 里，别名那一格是可以点开的设备切换菜单；没有可切换
    // 的设备时才是普通标签。这里跟着它走，所以标题那一格由播放器自己画。
    // 全屏那根浮动条和平时是同一根（Windows 也是），设备切换菜单在那儿一样能点。
    let switcher = player.as_ref().and_then(|player| player.device_switch());
    let caption =
        |ui: &mut egui::Ui, move_state: Option<&mut WindowMoveState>, tooltips| match switcher
            .as_ref()
        {
            Some(switcher) => crate::ui::chrome::window_title_bar_with_identity(
                ui,
                window,
                title,
                move_state,
                tooltips,
                &mut |ui: &mut egui::Ui| {
                    let response = crate::ui::controls::viewer_device_button(
                        ui,
                        title.trim_start_matches(crate::VIEWER_TITLE_PREFIX),
                    );
                    switcher.menu(&response, window.id());
                },
            ),
            None => window_title_bar(ui, window, title, move_state, tooltips),
        };
    if !fullscreen {
        return title_bar_panel(ui, panel, title_bar_height(), |ui| {
            let bar = ui.available_rect_before_wrap();
            let close = caption(ui, Some(move_state), tooltips);
            if let Some(player) = player {
                player.title_bar_controls(ui, bar, window);
            }
            close
        })
        .inner;
    }
    let Some(player) = player else {
        return false;
    };
    let relative = player.input().relative_mode();
    let menu_open = player.stream_control_ui.open;
    // 切屏、切设备期间浮动栏不收起，等待态一直看得见（Windows 传的是
    // `screen_tabs.is_pending()`，同一个判断）。
    let pending = player.is_pending();
    floating.show(&ctx, relative, menu_open, pending, |ui| {
        let bar = ui.available_rect_before_wrap();
        let close = caption(ui, None, true);
        player.title_bar_controls(ui, bar, window);
        close
    })
}

/// What a window's frame left to do after it was handed to the GPU.
struct PresentedFrame {
    /// The window asked to close (its close button, or a viewport command).
    close: bool,
    /// When egui wants the next pass.
    repaint_at: Option<Instant>,
    /// Time from the pass's start to the GPU submission.
    layout: Duration,
    presented: bool,
}

impl Shell {
    /// 取走播放器最新的一帧交给 GPU，再把攒下的错误做成弹窗。
    ///
    /// A minimised window never submits, and `write_texture` only reaches the
    /// GPU with the next submission, so uploading then would strand the
    /// staging copy inside the device -- one video frame's worth, at the
    /// video's frame rate, until an allocation finally fails. The frame is
    /// still taken, to keep the decoder's queue from growing instead.
    fn show_frame(&mut self, player: &mut Player, drawing: bool) -> Result<()> {
        if let Some(frame) = player.take_frame() {
            let upload = Instant::now();
            if drawing {
                match &frame.surface {
                    RenderSurface::CpuNv12 { data, color } => self.presenter.upload_video_nv12(
                        frame.width,
                        frame.height,
                        data,
                        color.transform(8),
                    )?,
                    RenderSurface::Dmabuf {
                        frame: dmabuf,
                        color,
                    } => self.presenter.upload_video_dmabuf(
                        frame.width,
                        frame.height,
                        dmabuf,
                        color.transform(8),
                    )?,
                    RenderSurface::CpuRgba8(pixels) => self.presenter.upload_video(
                        frame.width,
                        frame.height,
                        bytemuck::cast_slice(pixels),
                    )?,
                    // The D3D11 surface type is uninhabited off Windows.
                    RenderSurface::D3D11(surface) => match *surface {},
                }
            }
            player.publish(&frame, upload.elapsed());
            player.current = Some(frame);
        }
        // 这一帧画完之后再发错误弹窗：`notice` 自己会要求下一帧把它显示出来，
        // 而写在标题栏里会把屏标签挤没（Windows 对同样的错误也是弹窗）。
        player.publish_error(&self.context);
        Ok(())
    }

    /// 视频位置、平台输出（光标、剪贴板、输入法），然后提交这一帧；窗口最小化
    /// 时只把输出攒着。`started` 是这一轮开始的时刻，用来量排版耗时。
    fn present(
        &mut self,
        window: &Arc<Window>,
        output: egui::FullOutput,
        placement: Option<VideoPlacement>,
        drawing: bool,
        started: Instant,
    ) -> Result<PresentedFrame> {
        let placement = placement.filter(|_| self.presenter.has_video());
        self.presenter.set_video_placement(placement);
        let (ui_output, platform, viewports) = split_output(output);
        self.input.handle_platform_output(window, platform);
        let root = viewports.get(&egui::ViewportId::ROOT);
        let close = root.is_some_and(|viewport| {
            viewport
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::Close))
        });
        let repaint_at =
            root.and_then(|viewport| Instant::now().checked_add(viewport.repaint_delay));
        let layout = started.elapsed();
        let mut presented = false;
        if !drawing {
            self.presenter.defer_output(ui_output);
        } else {
            presented = self
                .presenter
                .render(&self.context, ui_output, false)?
                .presented();
            if presented && window.is_visible() == Some(false) {
                window.set_visible(true);
            }
        }
        Ok(PresentedFrame {
            close,
            repaint_at,
            layout,
            presented,
        })
    }
}

impl ConnectingWindowsRunner {
    pub(crate) fn has_focus(&self) -> bool {
        self.window
            .as_ref()
            .is_some_and(|window| window.has_focus())
            || self.detached.iter().any(|slot| slot.window.has_focus())
    }

    pub(crate) fn new(
        config: ConnectingWindowsRunConfig,
        needs_display: bool,
        proxy: EventLoopProxy<UiEvent>,
        embedded: bool,
    ) -> Self {
        let (requests, request_rx) = std_mpsc::channel();
        Self {
            attributes: WindowAttributes::default()
                .with_visible(false)
                .with_title(format!("{}{}", crate::VIEWER_TITLE_PREFIX, config.alias))
                .with_window_icon(Some(crate::ui::branding::window_icon()))
                .with_decorations(false)
                .with_inner_size(LogicalSize::new(1280.0, 760.0))
                .with_min_inner_size(LogicalSize::new(760.0, 520.0)),
            alias: config.alias.clone(),
            progress: Some(config.progress),
            session: config.session,
            display_sender: needs_display.then_some(config.display_sender),
            window: None,
            shell: None,
            stage: Stage::Connecting(Box::new(ConnectionProgressApp::new(
                config.alias,
                std_mpsc::channel().1,
            ))),
            close_requested: false,
            fatal_error: None,
            next_repaint: Some(Instant::now()),
            last_frame: None,
            ui_frame_interval: Duration::from_secs_f64(1.0 / 60.0),
            proxy,
            embedded,
            generation: 1,
            audio: None,
            audio_restore_geometry: None,
            screens: None,
            requests,
            request_rx,
            detached: Vec::new(),
            primary_close: false,
        }
    }

    pub(crate) fn closed(&self) -> bool {
        self.close_requested
    }

    pub(crate) fn error(&self) -> Option<String> {
        self.fatal_error.clone()
    }

    pub(crate) fn owns(&self, id: WindowId) -> bool {
        self.window.as_ref().is_some_and(|window| window.id() == id)
            || self.detached.iter().any(|slot| slot.window.id() == id)
    }

    pub(crate) fn focus(&self) {
        if let Some(window) = &self.window {
            window.set_minimized(false);
            window.set_visible(true);
            window.focus_window();
        }
    }

    fn exit(&mut self, event_loop: &ActiveEventLoop) {
        self.close_requested = true;
        self.stage = Stage::Finished;
        self.close_detached_all();
        if !self.embedded {
            event_loop.exit();
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: &anyhow::Error) {
        self.fatal_error = Some(format!("{error:#}"));
        self.exit(event_loop);
    }

    fn create(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        if self.window.is_some() {
            return Ok(());
        }
        let window = Arc::new(
            event_loop
                .create_window(self.attributes.clone())
                .context("create player window")?,
        );
        crate::ui::branding::set_taskbar_icon(&window);
        configure_dwm_window(&window);
        let (shell, interval) =
            Self::build_shell(event_loop, &window, &self.proxy, self.generation)?;
        self.ui_frame_interval = interval;
        tracing::debug!(
            interval_ms = self.ui_frame_interval.as_secs_f64() * 1000.0,
            scale = window.scale_factor(),
            "player window created"
        );
        self.shell = Some(shell);
        // Only the D3D11 path can hand a decoder its own surfaces; a Linux
        // session always decodes into CPU frames, so the handle stays empty.
        if let Some(sender) = self.display_sender.take() {
            let _ = sender.send(ViewerDisplayHandle::default());
        }
        if let Some(receiver) = self.progress.take() {
            self.stage = Stage::Connecting(Box::new(ConnectionProgressApp::new(
                self.alias.clone(),
                receiver,
            )));
        }
        self.window = Some(window);
        Ok(())
    }

    /// 建一个窗口的 egui 上下文、输入状态、wgpu 表面与浮条状态。主窗口和独立窗口共用。
    fn build_shell(
        event_loop: &ActiveEventLoop,
        window: &Arc<Window>,
        proxy: &EventLoopProxy<UiEvent>,
        generation: u64,
    ) -> Result<(Shell, Duration)> {
        let context = egui::Context::default();
        context.set_embed_viewports(true);
        crate::ui::fonts::install(&context);
        configure_viewer_visuals(&context);
        let window_id = window.id();
        let proxy = proxy.clone();
        context.set_request_repaint_callback(move |info| {
            if info.viewport_id == egui::ViewportId::ROOT
                && let Some(when) = Instant::now().checked_add(info.delay)
            {
                let _ = proxy.send_event(UiEvent::Repaint(UiRepaintEvent {
                    window: window_id,
                    generation,
                    pass: info.current_cumulative_pass_nr,
                    when,
                }));
            }
        });
        let input = egui_winit::State::new(
            context.clone(),
            egui::ViewportId::ROOT,
            window,
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );
        let graphics = crate::ui::gfx::create_device(event_loop, window)?;
        let presenter = UiPresenter::new(window.clone(), &graphics)?;
        Ok((
            Shell {
                context,
                input,
                presenter,
                move_state: WindowMoveState::default(),
                resize_state: WindowResizeState::default(),
                fullscreen_chrome: Default::default(),
                timing_audit: None,
            },
            ui_frame_interval(window),
        ))
    }

    /// Session hand-offs arrive on a channel: play, close, or reconnect.
    fn poll_session(&mut self, event_loop: &ActiveEventLoop) {
        loop {
            let event = match self.session.try_recv() {
                Ok(event) => event,
                Err(std_mpsc::TryRecvError::Empty) => return,
                Err(std_mpsc::TryRecvError::Disconnected) => return,
            };
            match event {
                ViewerWindowEvent::Close => {
                    self.exit(event_loop);
                    return;
                }
                ViewerWindowEvent::Listening(audio) => {
                    let Some(window) = self.window.clone() else {
                        return;
                    };
                    self.audio_restore_geometry.get_or_insert_with(|| {
                        (
                            window.inner_size(),
                            window.is_maximized(),
                            window.fullscreen(),
                        )
                    });
                    configure_audio_window(&window, false);
                    window.set_visible(true);
                    self.audio = Some(audio);
                    window.request_redraw();
                }
                ViewerWindowEvent::Playing(session) => {
                    let Some(window) = self.window.clone() else {
                        return;
                    };
                    self.audio = None;
                    restore_video_window_constraints(&window);
                    if let Some((size, maximized, fullscreen)) = self.audio_restore_geometry.take()
                    {
                        let _ = window.request_inner_size(size);
                        window.set_maximized(maximized);
                        set_fullscreen(&window, fullscreen);
                    } else if window
                        .inner_size()
                        .to_logical::<f64>(window.scale_factor())
                        .width
                        < 760.0
                    {
                        let _ = window.request_inner_size(LogicalSize::new(1280.0, 760.0));
                    }
                    let mut session = *session;
                    let screens = match self.screens.clone() {
                        Some(screens) => screens,
                        None => match session.take_screen_playback() {
                            Some(screens) => screens,
                            None => {
                                self.fail(
                                    event_loop,
                                    &anyhow!("screen playback owner unavailable"),
                                );
                                return;
                            }
                        },
                    };
                    self.screens = Some(screens.clone());
                    let tabs = screens.screens().iter().map(|screen| screen.id).collect();
                    match Player::new(
                        Arc::new(session),
                        screens,
                        &window,
                        self.proxy.clone(),
                        PlayerWindow {
                            tabs,
                            requests: self.requests.clone(),
                            input_owns_connection: true,
                        },
                    ) {
                        Ok(player) => {
                            window.set_visible(true);
                            self.stage = Stage::Playing(Box::new(player));
                        }
                        Err(error) => {
                            self.fail(event_loop, &error);
                            return;
                        }
                    }
                }
                ViewerWindowEvent::Reconnect {
                    alias,
                    window: _,
                    progress,
                    display,
                } => {
                    self.close_detached_all();
                    self.screens = None;
                    self.alias = alias.clone();
                    self.generation += 1;
                    self.audio = None;
                    if let Some(window) = &self.window
                        && !window.is_resizable()
                    {
                        restore_video_window_constraints(window);
                        let _ = window.request_inner_size(LogicalSize::new(1280.0, 760.0));
                    }
                    let _ = display.send(ViewerDisplayHandle::default());
                    if let Some(shell) = self.shell.as_mut() {
                        shell.presenter.clear_video();
                    }
                    self.stage =
                        Stage::Connecting(Box::new(ConnectionProgressApp::new(alias, progress)));
                }
            }
        }
    }

    /// Pace ordinary UI repaints at the display refresh rate.
    fn schedule(&mut self, when: Instant) {
        let when = self
            .last_frame
            .map_or(when, |last| when.max(last + self.ui_frame_interval));
        self.next_repaint = Some(self.next_repaint.map_or(when, |old| old.min(when)));
    }

    /// A decoded frame is already late; waiting for the next paced tick costs
    /// it most of a refresh interval on the way to the screen.
    fn schedule_now(&mut self) {
        self.next_repaint = Some(Instant::now());
    }

    /// 独立窗口的重画下限与主窗口同一套节流。
    fn schedule_window(&mut self, id: WindowId, when: Instant) {
        if self.window.as_ref().is_some_and(|window| window.id() == id) {
            self.schedule(when);
            return;
        }
        if let Some(slot) = self.detached.iter_mut().find(|slot| slot.window.id() == id) {
            let when = slot
                .last_frame
                .map_or(when, |last| when.max(last + slot.interval));
            slot.next_repaint = Some(slot.next_repaint.map_or(when, |old| old.min(when)));
        }
    }

    /// 连接结束或重连：独立窗口全部作废，屏归还主窗口。
    fn close_detached_all(&mut self) {
        while !self.detached.is_empty() {
            self.close_detached(self.detached.len() - 1);
        }
    }

    /// 关掉主窗口：还有屏窗口在播时，把「主窗口」这个角色交给其中一个，连接
    /// 继续；一个都不剩才真正结束。Windows 那边所有观看窗口本来就是平级的，
    /// 关掉任意一个都只是合并，这里跟着它走。
    fn close_primary(&mut self, event_loop: &ActiveEventLoop) {
        let Some(index) = self
            .detached
            .iter()
            .position(|slot| matches!(slot.stage, DetachedStage::Playing(_)))
        else {
            self.exit(event_loop);
            return;
        };
        // 旧主窗口的屏交给接替者，别让它们随着窗口一起消失。
        let mut tabs = Vec::new();
        if let (Stage::Playing(player), Some(window)) = (&mut self.stage, &self.window) {
            // 它已经交出了正在直播的那块屏，析构时不能再关整个解码器池。
            player.input_owns_connection = false;
            tabs = std::mem::take(&mut player.tabs);
            player.abandon_window(window.id());
        }
        let DetachedWindow {
            stage,
            pending,
            shell,
            window,
            tabs: _,
            close: _,
            next_repaint,
            last_frame,
            interval,
        } = self.detached.remove(index);
        drop(pending);
        let DetachedStage::Playing(mut player) = stage else {
            self.exit(event_loop);
            return;
        };
        player.input_owns_connection = true;
        for id in tabs {
            if !player.tabs.contains(&id) {
                player.tabs.push(id);
            }
        }
        // 旧主窗口离场。Player 的唤醒只撤销自己那份登记。
        if let Some(old) = self.window.take() {
            old.set_visible(false);
        }
        let old_stage = std::mem::replace(&mut self.stage, Stage::Playing(player));
        let old_shell = self.shell.take();
        drop(old_stage);
        drop(old_shell);
        self.window = Some(window);
        self.shell = Some(shell);
        self.last_frame = last_frame;
        self.ui_frame_interval = interval;
        self.next_repaint = earliest(self.next_repaint, next_repaint);
        // 音频视图属于被关掉的那个窗口，多窗口时本来也不会在。
        self.audio = None;
        self.audio_restore_geometry = None;
        self.schedule(Instant::now());
        tracing::debug!("主窗口已关闭，由一个屏窗口接替");
    }

    fn close_detached(&mut self, index: usize) {
        if index >= self.detached.len() {
            return;
        }
        let mut slot = self.detached.remove(index);
        slot.close = true;
        slot.window.set_visible(false);
        slot.pending = None;
        if let DetachedStage::Playing(player) = &slot.stage {
            player.abandon_window(slot.window.id());
        }
        let tabs = std::mem::take(&mut slot.tabs);
        if let Stage::Playing(player) = &mut self.stage {
            for id in tabs {
                if !player.tabs.contains(&id) {
                    player.tabs.push(id);
                }
                // 拖出时交出去的那块屏又回来了：主窗口如果还在等新屏，直接从
                // 这路解码器接着看，而不是一直停在「正在等待画面」。
                if player.frame_paused && player.session.screen_id() == id {
                    player.resume_frames();
                }
            }
            self.schedule(Instant::now());
        }
    }

    /// 播放窗口请求把一块屏拖出成独立窗口，或关掉自己。
    fn handle_requests(&mut self, event_loop: &ActiveEventLoop) {
        while let Ok(request) = self.request_rx.try_recv() {
            let result = match request {
                WindowRequest::Detach {
                    screen_id,
                    position,
                } => self.detach_screen(event_loop, screen_id, position),
                WindowRequest::Close { window } => {
                    if self
                        .window
                        .as_ref()
                        .is_some_and(|primary| primary.id() == window)
                    {
                        self.primary_close = true;
                    } else if let Some(index) = self
                        .detached
                        .iter()
                        .position(|slot| slot.window.id() == window)
                    {
                        self.close_detached(index);
                    }
                    Ok(())
                }
            };
            if let Err(error) = result {
                tracing::warn!(%error, "无法把显示器拖出成独立窗口");
                if let Stage::Playing(player) = &mut self.stage {
                    player.report("显示器操作失败", format!("{error:#}"));
                }
            }
        }
    }

    /// 建一个新窗口，把这块屏从主窗口挪过去。窗口先画连接进度，解码器就绪后换成播放器。
    fn detach_screen(
        &mut self,
        event_loop: &ActiveEventLoop,
        screen_id: i32,
        position: Option<PhysicalPosition<i32>>,
    ) -> Result<()> {
        let screens = self.screens.clone().context("屏幕目录尚未就绪")?;
        let source = self.window.clone().context("播放窗口尚未创建")?;
        let Stage::Playing(player) = &mut self.stage else {
            bail!("连接尚未就绪");
        };
        anyhow::ensure!(player.tabs.contains(&screen_id), "显示器不在这个窗口里");
        anyhow::ensure!(player.tabs.len() > 1, "这个窗口只剩一块显示器，不能再拖出");
        anyhow::ensure!(
            !player.session.is_software(),
            "软解仅允许一个播放窗口，请使用标签切换屏幕"
        );
        anyhow::ensure!(
            !self
                .detached
                .iter()
                .any(|slot| slot.tabs.contains(&screen_id)),
            "显示器已经在独立窗口里"
        );
        let catalog = screens.screens();
        anyhow::ensure!(
            catalog.iter().any(|screen| screen.id == screen_id),
            "显示器已断开"
        );
        let label = catalog
            .iter()
            .find(|screen| screen.id == screen_id)
            .map_or_else(String::new, |screen| screen.label(&catalog));

        let attributes = WindowAttributes::default()
            .with_visible(false)
            .with_title(format!("{}{}", crate::VIEWER_TITLE_PREFIX, self.alias))
            .with_window_icon(Some(crate::ui::branding::window_icon()))
            .with_decorations(false)
            .with_inner_size(source.inner_size())
            .with_min_inner_size(LogicalSize::new(760.0, 520.0));
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .context("创建独立播放窗口")?,
        );
        crate::ui::branding::set_taskbar_icon(&window);
        configure_dwm_window(&window);
        if let Some(position) = position {
            // 抓取点大致落在新窗口的标题栏上，标题栏整条留在松手的那块显示器里。
            // Wayland 上这个请求会被忽略，位置交给合成器决定。
            let monitors: Vec<_> = event_loop
                .available_monitors()
                .map(|monitor| (monitor.position(), monitor.size()))
                .collect();
            window.set_outer_position(constrain_to_monitor(
                PhysicalPosition::new(position.x - 100, position.y - 18),
                position,
                window.outer_size(),
                crate::ui::chrome::title_bar_height_pixels(&window),
                &monitors,
            ));
        }
        let (shell, interval) =
            Self::build_shell(event_loop, &window, &self.proxy, self.generation)?;

        let (progress, progress_rx) = std_mpsc::channel();
        let _ = progress.send(ConnectionProgress::working(10, "正在打开显示器", label));
        let app = ConnectionProgressApp::new(self.alias.clone(), progress_rx);
        let repaint_window = window.id();
        let proxy = self.proxy.clone();
        let repaint = move || {
            let _ = proxy.send_event(UiEvent::Repaint(UiRepaintEvent {
                window: repaint_window,
                generation: 0,
                pass: 0,
                when: Instant::now(),
            }));
        };
        let (task, result) = screens.open(
            screen_id,
            ViewerDisplayHandle::default(),
            None,
            None,
            repaint,
        );
        self.detached.push(DetachedWindow {
            stage: DetachedStage::Opening(Box::new(app)),
            pending: Some(DetachedPending {
                screen_id,
                task,
                result,
            }),
            shell,
            window: window.clone(),
            tabs: vec![screen_id],
            close: false,
            next_repaint: Some(Instant::now()),
            last_frame: None,
            interval,
        });
        window.request_redraw();

        if let Stage::Playing(player) = &mut self.stage {
            player.tabs.retain(|id| *id != screen_id);
            // 正在打开的就是这块：交给新窗口去开，原窗口放弃这次切换，否则它接上
            // 之后两个窗口会从同一个解码队列取帧（Windows `detach_tab` 同样
            // `pending.take()`）。
            if player
                .pending
                .as_ref()
                .is_some_and(|pending| pending.id == screen_id)
            {
                player.pending = None;
            }
            if player.session.screen_id() == screen_id {
                // 这块解码器已经交给新窗口了。新屏接上之前主窗口不要再从同一个
                // 队列取帧，否则两个窗口会互相抢帧，两边都停住。
                player.frame_paused = true;
                player.current = None;
                player.video_size = None;
                player.video_rect = None;
                if let Some(next) = player.tabs.first().copied()
                    && let Err(error) = player.select(next, &source)
                {
                    player.failed_screen = Some(player.failed_screen_for(next));
                    player.report(
                        "显示器操作失败",
                        format!("切到显示器 {next} 失败：{error:#}"),
                    );
                }
                player.park_stranded();
            }
        }
        Ok(())
    }

    /// 独立窗口的解码器就绪、取帧、目录核对。
    fn poll_detached(&mut self) {
        let Some(screens) = self.screens.clone() else {
            return;
        };
        let mut close = Vec::new();
        for (index, slot) in self.detached.iter_mut().enumerate() {
            let ready = slot
                .pending
                .as_mut()
                .and_then(|pending| match pending.result.try_recv() {
                    Ok(result) => Some((pending.screen_id, result)),
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                        Some((pending.screen_id, Err(anyhow!("屏幕加载已取消"))))
                    }
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty) => None,
                });
            if let Some((id, result)) = ready {
                slot.pending = None;
                let installed = match result {
                    Ok(session)
                        if !screen_mapped(
                            &screens.screens(),
                            id,
                            session.track_index,
                            session.screen_binding(),
                        ) =>
                    {
                        tracing::debug!(screen_id = id, "打开期间屏的映射变了，放弃这个独立窗口");
                        false
                    }
                    Ok(session) => match Player::new(
                        session,
                        screens.clone(),
                        &slot.window,
                        self.proxy.clone(),
                        PlayerWindow {
                            tabs: vec![id],
                            requests: self.requests.clone(),
                            input_owns_connection: false,
                        },
                    ) {
                        Ok(player) => {
                            slot.tabs.clone_from(&player.tabs);
                            slot.stage = DetachedStage::Playing(Box::new(player));
                            slot.next_repaint = Some(Instant::now());
                            true
                        }
                        Err(error) => {
                            tracing::error!(%error, "独立窗口播放器初始化失败");
                            false
                        }
                    },
                    Err(error) => {
                        tracing::warn!(%error, "打开独立窗口的显示器失败");
                        false
                    }
                };
                if !installed {
                    slot.close = true;
                    if let Stage::Playing(player) = &mut self.stage {
                        player.report("显示器操作失败", format!("无法在独立窗口打开显示器 {id}"));
                    }
                }
            }
            if let DetachedStage::Playing(player) = &mut slot.stage {
                let mut redraw = player.poll_pending();
                player.pump_drag();
                // 独立窗口只守着自己那块屏，新屏归主窗口。
                redraw |= player.refresh_catalog(&slot.window, &[]);
                let frame = player.poll();
                redraw |= player.error.is_some();
                if frame || redraw {
                    slot.next_repaint = Some(Instant::now());
                }
                if player.connection_ended() {
                    self.close_requested = true;
                }
                slot.tabs.clone_from(&player.tabs);
                // 这块屏从对端目录里没了：窗口没有别的屏可显示，和 Windows 一样关掉。
                if player.tabs.is_empty() {
                    slot.close = true;
                }
            }
            if slot.close {
                close.push(index);
            }
        }
        for index in close.into_iter().rev() {
            self.close_detached(index);
        }
    }

    fn render_detached(&mut self, index: usize) -> Result<()> {
        let Some(slot) = self.detached.get_mut(index) else {
            return Ok(());
        };
        let window = slot.window.clone();
        slot.next_repaint = None;
        let started = Instant::now();
        slot.last_frame = Some(started);
        let mut close_requested = false;
        let mut placement = None;
        if let DetachedStage::Opening(app) = &mut slot.stage {
            app.drain();
        }
        let input = slot.shell.input.take_egui_input(&window);
        let shell = &mut slot.shell;
        let stage = &mut slot.stage;
        let title = self.alias.clone();
        let output = shell.context.run_ui(input, |ui| {
            let player = match stage {
                DetachedStage::Playing(player) => Some(&mut **player),
                DetachedStage::Opening(_) => None,
            };
            close_requested |= draw_window_chrome(
                ui,
                &window,
                &title,
                "detached-window-chrome",
                true,
                WindowChrome {
                    move_state: &mut shell.move_state,
                    resize_state: &mut shell.resize_state,
                    fullscreen: &mut shell.fullscreen_chrome,
                },
                player,
            );
            let menu_inset = shell.fullscreen_chrome.menu_inset(ui.ctx());
            match stage {
                DetachedStage::Opening(app) => app.draw(ui),
                DetachedStage::Playing(player) => placement = player.draw(ui, &window, menu_inset),
            }
            crate::ui::controls::show_notices(ui.ctx());
        });
        let drawing = window.is_minimized() != Some(true);
        if let DetachedStage::Playing(player) = stage {
            shell.show_frame(player, drawing)?;
        }
        let frame = shell.present(&window, output, placement, drawing, started)?;
        close_requested |= frame.close;
        let repaint_at = frame.repaint_at;
        if close_requested {
            slot.close = true;
        }
        if let Some(when) = repaint_at {
            let when = slot
                .last_frame
                .map_or(when, |last| when.max(last + slot.interval));
            slot.next_repaint = Some(slot.next_repaint.map_or(when, |old| old.min(when)));
        }
        Ok(())
    }

    fn detached_window_event(&mut self, index: usize, event: WindowEvent) {
        let Some(slot) = self.detached.get_mut(index) else {
            return;
        };
        let window = slot.window.clone();
        let shell = &mut slot.shell;
        let previous_events = shell.input.egui_input_mut().events.len();
        let response = shell.input.on_window_event(&window, &event);
        let consumed = response.consumed;
        if let DetachedStage::Playing(player) = &mut slot.stage {
            let forwarded = player.on_window_event(&window, &shell.context, &event, consumed);
            if matches!(
                event,
                WindowEvent::KeyboardInput { .. } | WindowEvent::MouseInput { .. }
            ) {
                player.update_shortcuts(&shell.context, &window);
            }
            if forwarded && matches!(event, WindowEvent::KeyboardInput { .. }) {
                shell
                    .input
                    .egui_input_mut()
                    .events
                    .truncate(previous_events);
            }
        }
        let mut close = false;
        let mut render = false;
        match event {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => close = true,
            WindowEvent::Focused(focused) => {
                if !focused {
                    cancel_pointer_operation(&mut shell.move_state, &mut shell.resize_state);
                }
                if let DetachedStage::Playing(player) = &mut slot.stage {
                    player.on_focus_changed(&window, focused);
                    player.update_shortcuts(&shell.context, &window);
                    if focused {
                        // 对端按聚焦窗口的那块屏送指针与输入。
                        player.screens.focus(player.session.screen_id());
                    }
                }
                crate::application::viewer_shortcuts::set_focused_window(if focused {
                    u64::from(window.id())
                } else {
                    0
                });
                slot.next_repaint = Some(Instant::now());
            }
            WindowEvent::Resized(size) => {
                if let Err(error) = shell.presenter.resize(size) {
                    tracing::error!(%error, "独立窗口的表面缩放失败");
                    close = true;
                }
                slot.next_repaint = Some(Instant::now());
            }
            WindowEvent::RedrawRequested => render = true,
            _ => {
                if response.repaint {
                    slot.next_repaint = Some(Instant::now());
                }
            }
        }
        if close {
            self.close_detached(index);
        } else if render && let Err(error) = self.render_detached(index) {
            tracing::error!(%error, "独立窗口渲染失败");
            self.close_detached(index);
        }
    }

    /// 把每个窗口要看的那几块屏合并起来交给共享的解码器池。
    fn sync_visible(&self) {
        let Some(screens) = &self.screens else {
            return;
        };
        let mut visible = match &self.stage {
            Stage::Playing(player) => player.visible_screens(),
            _ => Vec::new(),
        };
        for slot in &self.detached {
            match &slot.stage {
                DetachedStage::Playing(player) => visible.extend(player.visible_screens()),
                // 解码器还没交回来时，`open` 已经把这屏放进去了；这里保住它，
                // 免得聚合结果把它覆盖掉。
                DetachedStage::Opening(_) => visible.extend(&slot.tabs),
            }
        }
        visible.sort_unstable();
        visible.dedup();
        screens.set_visible(visible);
    }

    /// 把目录里还没分给任何窗口的屏补进主窗口。独立窗口只留它自己那一块，
    /// 和 Windows 的 `refresh_catalog` 把新屏交给 owner 窗口是同一条规则。
    fn sync_tabs(&mut self) -> bool {
        let elsewhere = self.detached_tabs();
        let Stage::Playing(player) = &mut self.stage else {
            return false;
        };
        collect_new_screens(&mut player.tabs, &player.catalog, &elsewhere)
    }

    /// 已经分给独立窗口的屏。
    fn detached_tabs(&self) -> Vec<i32> {
        self.detached
            .iter()
            .flat_map(|slot| slot.tabs.iter().copied())
            .collect()
    }

    fn render(&mut self) -> Result<()> {
        let (Some(window), Some(shell)) = (self.window.clone(), self.shell.as_mut()) else {
            return Ok(());
        };
        self.next_repaint = None;
        let started = Instant::now();
        self.last_frame = Some(started);
        let title = self.alias.clone();
        let mut close_requested = false;
        let mut placement = None;
        if let Stage::Connecting(app) = &mut self.stage {
            app.drain();
            if matches!(app.current.state, super::ConnectionProgressState::Failed) {
                self.audio = None;
            }
        }
        let input = shell.input.take_egui_input(&window);
        let stage = &mut self.stage;
        let audio = &mut self.audio;
        let output = shell.context.run_ui(input, |ui| {
            let player = match stage {
                Stage::Playing(player) => Some(&mut **player),
                _ => None,
            };
            close_requested |= draw_window_chrome(
                ui,
                &window,
                &title,
                "player-window-chrome",
                audio.is_none(),
                WindowChrome {
                    move_state: &mut shell.move_state,
                    resize_state: &mut shell.resize_state,
                    fullscreen: &mut shell.fullscreen_chrome,
                },
                player,
            );
            let menu_inset = shell.fullscreen_chrome.menu_inset(ui.ctx());
            match stage {
                Stage::Connecting(app) => match audio.as_mut() {
                    Some(audio) => close_requested |= audio.show(ui),
                    None => app.draw(ui),
                },
                Stage::Playing(player) => placement = player.draw(ui, &window, menu_inset),
                Stage::Finished => {}
            }
            crate::ui::controls::show_notices(ui.ctx());
        });
        if let Some(compact) = self
            .audio
            .as_mut()
            .and_then(|audio| audio.take_layout_change())
        {
            configure_audio_window(&window, compact);
        }
        let drawing = window.is_minimized() != Some(true);
        if let Stage::Playing(player) = &mut self.stage {
            shell.show_frame(player, drawing)?;
        }
        let frame = shell.present(&window, output, placement, drawing, started)?;
        close_requested |= frame.close;
        let repaint_at = frame.repaint_at;
        if let Some(audit) = UiTimingAudit::active(&mut shell.timing_audit, started) {
            audit.record(
                started,
                frame.layout,
                started.elapsed().saturating_sub(frame.layout),
                repaint_at.is_some_and(|when| when <= started),
                frame.presented,
            );
        }
        self.primary_close |= close_requested;
        if let Some(when) = repaint_at {
            self.schedule(when);
        }
        Ok(())
    }
}

impl ApplicationHandler<UiEvent> for ConnectingWindowsRunner {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(error) = self.create(event_loop) {
            self.fail(event_loop, &error);
            return;
        }
        self.poll_session(event_loop);
        if let Err(error) = self.render() {
            self.fail(event_loop, &error);
            return;
        }
        if let Some(window) = &self.window {
            window.set_visible(true);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if !self.owns(id) {
            return;
        }
        if let Some(index) = self.detached.iter().position(|slot| slot.window.id() == id) {
            self.detached_window_event(index, event);
            return;
        }
        let (Some(window), Some(shell)) = (self.window.clone(), self.shell.as_mut()) else {
            return;
        };
        let previous_events = shell.input.egui_input_mut().events.len();
        let response = shell.input.on_window_event(&window, &event);
        let consumed = response.consumed;
        if let Stage::Playing(player) = &mut self.stage {
            let forwarded = player.on_window_event(&window, &shell.context, &event, consumed);
            if matches!(
                event,
                WindowEvent::KeyboardInput { .. } | WindowEvent::MouseInput { .. }
            ) {
                // A key or click can start or end control (the release
                // shortcut); the shortcuts follow at once, not a frame later.
                player.update_shortcuts(&shell.context, &window);
            }
            if forwarded && matches!(event, WindowEvent::KeyboardInput { .. }) {
                // A remote Tab must not also focus a local toolbar button.
                shell
                    .input
                    .egui_input_mut()
                    .events
                    .truncate(previous_events);
            }
        }
        match event {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                self.primary_close = true;
                return;
            }
            WindowEvent::Focused(focused) => {
                if !focused {
                    cancel_pointer_operation(&mut shell.move_state, &mut shell.resize_state);
                }
                if let Stage::Playing(player) = &mut self.stage {
                    player.on_focus_changed(&window, focused);
                    player.update_shortcuts(&shell.context, &window);
                    // 和独立窗口、和 Windows 的每个屏窗口一样：对端按聚焦窗口的那块屏
                    // 送指针与输入。从独立窗口点回来时不说，键鼠会落到那边的屏上。
                    // 当前屏刚交给独立窗口、新屏还没接上时，这个窗口没有自己的屏。
                    if focused && !player.frame_paused {
                        player.screens.focus(player.session.screen_id());
                    }
                }
                crate::application::viewer_shortcuts::set_focused_window(if focused {
                    u64::from(id)
                } else {
                    0
                });
                self.schedule(Instant::now());
            }
            WindowEvent::Resized(size) => {
                if let Err(error) = shell.presenter.resize(size) {
                    self.fail(event_loop, &error);
                    return;
                }
                self.schedule(Instant::now());
            }
            WindowEvent::RedrawRequested => {
                if let Err(error) = self.render() {
                    self.fail(event_loop, &error);
                    return;
                }
            }
            _ => {
                if response.repaint {
                    self.schedule(Instant::now());
                }
            }
        }
        if self.close_requested {
            self.exit(event_loop);
        }
    }

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        device: DeviceId,
        event: DeviceEvent,
    ) {
        // 原始运动事件没有窗口号，只派给有焦点的那个窗口。
        if let Some(slot) = self
            .detached
            .iter_mut()
            .find(|slot| slot.window.has_focus())
        {
            if let DetachedStage::Playing(player) = &mut slot.stage
                && player.on_device_event(device, &event)
            {
                slot.next_repaint = Some(Instant::now());
            }
            return;
        }
        // Raw motion is the only pointer source once the pointer is locked.
        if let Stage::Playing(player) = &mut self.stage
            && player.on_device_event(device, &event)
        {
            self.schedule(Instant::now());
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UiEvent) {
        let UiEvent::Repaint(event) = event else {
            return;
        };
        if !self.owns(event.window) {
            return;
        }
        self.schedule_window(event.window, event.when);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.poll_session(event_loop);
        if self.close_requested {
            self.exit(event_loop);
            return;
        }
        self.handle_requests(event_loop);
        if self.primary_close {
            self.primary_close = false;
            self.close_primary(event_loop);
            if self.close_requested {
                return;
            }
        }
        self.poll_detached();
        self.sync_visible();
        let elsewhere = self.detached_tabs();
        if let Stage::Playing(player) = &mut self.stage {
            let mut redraw = player.poll_pending();
            // 拖放那条通道读 socket 的仍然是 winit，这里只把已经读进来的事件
            // 分发给它自己的队列——拖动期间指针不在我们手上，晚一轮就少一格坐标。
            player.pump_drag();
            // 屏目录每 250 ms 对一次：切屏期间没有画面要画，但轮询交给它自己，
            // 不再把 UI 线程按在「立刻重画」上（那样会绕开帧率节拍一直占满一个核）。
            if let Some(window) = self.window.clone() {
                redraw |= player.refresh_catalog(&window, &elsewhere);
            }
            if player.connection_ended() {
                // 和 Windows 一样干净地退出：结束的原因由 controller 那边报。
                self.exit(event_loop);
                return;
            }
            // Nothing decoded: egui asks for the repaints it needs and the
            // wake bridge covers the next frame, so the loop can sleep.
            // Repainting the whole UI at the display rate instead would
            // leave an arriving frame waiting for the pass in progress.
            player.park_stranded();
            let frame = player.poll();
            // 解码器出错的弹窗也要画出来，哪怕之后再没有新帧。
            redraw |= player.error.is_some();
            if frame {
                self.schedule_now();
            } else if redraw {
                self.schedule(Instant::now());
            }
        }
        if self.sync_tabs() {
            self.schedule(Instant::now());
        }
        let now = Instant::now();
        let due: Vec<usize> = self
            .detached
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.next_repaint.is_some_and(|when| when <= now))
            .map(|(index, _)| index)
            .collect();
        for index in due {
            if self.detached.get(index).is_none() {
                continue;
            }
            if let Err(error) = self.render_detached(index) {
                tracing::error!(%error, "独立窗口渲染失败");
                if let Some(slot) = self.detached.get_mut(index) {
                    slot.close = true;
                }
            }
        }
        while let Some(index) = self.detached.iter().position(|slot| slot.close) {
            self.close_detached(index);
        }
        match self.next_repaint {
            Some(when) if when <= Instant::now() => {
                if let Err(error) = self.render() {
                    self.fail(event_loop, &error);
                    return;
                }
            }
            _ => {}
        }
        // 目录核对与独立窗口的重画都有自己的节拍，和重画一样要能把事件循环叫醒。
        let mut wake = self.next_repaint;
        if let Stage::Playing(player) = &self.stage {
            wake = earliest(wake, Some(player.next_refresh));
        }
        for slot in &self.detached {
            wake = earliest(wake, slot.next_repaint);
            if let DetachedStage::Playing(player) = &slot.stage {
                wake = earliest(wake, Some(player.next_refresh));
            }
        }
        event_loop.set_control_flow(wake.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.close_detached_all();
        self.stage = Stage::Finished;
        self.shell = None;
        self.window = None;
    }
}

/// 拖出去的一块屏：自己的窗口与 egui/wgpu 表面，共享同一个连接和解码器池。
struct DetachedWindow {
    /// 字段顺序即析构顺序：先丢播放器和进度页，再丢表面，最后才是窗口。
    stage: DetachedStage,
    pending: Option<DetachedPending>,
    shell: Shell,
    window: Arc<Window>,
    /// 这个窗口分到的屏。独立窗口只有一块；拖进新屏等于再开一个窗口。
    tabs: Vec<i32>,
    close: bool,
    next_repaint: Option<Instant>,
    last_frame: Option<Instant>,
    interval: Duration,
}

enum DetachedStage {
    /// 解码器还没就绪，先画连接进度。
    Opening(Box<ConnectionProgressApp>),
    Playing(Box<Player>),
}

/// 正在为独立窗口打开的那块屏。
struct DetachedPending {
    screen_id: i32,
    /// 让 `ScreenPlayback` 申请对端开始推这块屏；取消时 abort 掉这次等待。
    task: Option<tokio::task::AbortHandle>,
    result: tokio::sync::oneshot::Receiver<Result<Arc<NativeViewerSession>>>,
}

impl Drop for DetachedPending {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// 构造播放器时要和窗口绑在一起的一组东西。
struct PlayerWindow {
    /// 这个窗口分到的屏。
    tabs: Vec<i32>,
    /// 把屏拖出成独立窗口的请求通道。
    requests: std_mpsc::Sender<WindowRequest>,
    /// 主窗口负责整个连接的键鼠控制与解码器池；独立窗口关闭时不能动它们。
    input_owns_connection: bool,
}

/// 播放窗口向 runner 提的窗口级请求：把一块屏拖出成独立窗口、或关掉自己。
enum WindowRequest {
    Detach {
        screen_id: i32,
        /// 松手时的全局位置（X11 才有）；Wayland 交给合成器摆放。
        position: Option<PhysicalPosition<i32>>,
    },
    /// 独立窗口请求关闭自己（主窗口关闭等于结束连接，走它原来的路径）。
    Close { window: WindowId },
}

/// 正在打开的一块屏：解码器还没就绪，tab 先显示等待态。
struct PendingScreen {
    id: i32,
    /// 让 `ScreenPlayback` 申请对端开始推这块屏。丢掉这次等待就是把请求取消掉，
    /// 所以句柄留着、Drop 里 abort（和 Windows 侧一致）。
    task: Option<tokio::task::AbortHandle>,
    result: tokio::sync::oneshot::Receiver<Result<Arc<NativeViewerSession>>>,
}

impl Drop for PendingScreen {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// The playing half of the window: video, input forwarding and the menus.
struct Player {
    /// Reloads shortcut settings changed in the control center while playing.
    _shortcuts: crate::application::viewer_shortcuts::Watcher,
    /// 本窗口正在显示的那块屏的解码器。窗口「选中的」屏就是它的绑定，
    /// 切换中的那块另记在 `pending` 里（与 Windows 的 `slot.selected` 同义）。
    session: Arc<NativeViewerSession>,
    /// 连接开始时的那条会话。带着「连接结束」句柄的是它
    /// （`session/controller/viewing.rs` 的 `present_video`），切到别的屏之后也得靠它
    /// 的 shutdown 标志发现连接没了（见 [`Player::connection_ended`]）。
    initial: Arc<NativeViewerSession>,
    /// 正在显示的那块屏的解码器已经报过致命错误：弹一次窗就够了，换屏时复位。
    session_failed: bool,
    /// 这个连接上所有能看的屏，以及每块屏各自的解码器。多窗口时共用同一份。
    screens: super::screens::ScreenPlayback,
    /// 顶栏 tab 用的屏目录：对端报的屏，加上拓扑里还没落定的那几块。
    catalog: Vec<RemoteScreen>,
    /// 下次核对目录的时间。
    next_refresh: Instant,
    /// 上一次看过的拓扑序号，变了就重新跟一次对端选中的屏。
    topology_sequence: Option<i64>,
    /// 已经跟过去的那块对端选中的屏，免得同一份报告反复切。
    topology_focused: Option<i32>,
    /// 开失败的那块屏 `(id, 轨道, 类型)`，它变样之前不再重试。
    failed_screen: Option<(i32, i32, i32)>,
    /// 正在打开的那块屏。
    pending: Option<PendingScreen>,
    proxy: EventLoopProxy<UiEvent>,
    window_id: WindowId,
    /// Keeps the decoder publishing frames and nudges the event loop; held for
    /// its lifetime, which is what registers this window with the session.
    /// `None` while the window has handed its screen to a detached window and
    /// has nothing else to show (see [`Player::park_stranded`]).
    #[allow(dead_code, reason = "Owned for its registration and its Drop.")]
    wake: Option<FrameWakeBridge>,
    /// Identifies this window to `RemoteInput`, which arbitrates between windows.
    owner: u64,
    stream_control_ui: StreamControlUi,
    performance_mode: PerformancePanelMode,
    intercept_shortcuts: bool,
    current: Option<DecodedVideoFrame>,
    video_size: Option<(u32, u32, u16)>,
    /// The device's local display aspect, which stretches the video and the
    /// pointer mapping with it; `None` keeps the picture's own aspect.
    display_aspect: Option<(u32, u32)>,
    /// Whether the window was fullscreen at the last frame.
    fullscreen: bool,
    /// Content area of the last frame, used to map pointer positions.
    video_rect: Option<egui::Rect>,
    pointer_in_video: bool,
    last_position: Option<PhysicalPosition<f64>>,
    modifiers: winit::keyboard::ModifiersState,
    held_keys: Vec<u16>,
    /// Everything physically down right now, whether or not it was forwarded.
    /// Entering control waits for this to empty before the remote accepts input.
    physical_keys: Vec<u16>,
    physical_buttons: u8,
    /// Pixels per point of the last frame, for pointer mapping.
    scale: f32,
    /// 待显示的错误 `(标题, 正文)`。取走时做成可关闭的弹窗：顶栏那一格是留给屏标签
    /// 的，一条常驻的红色文字能把它挤没。
    error: Option<(&'static str, String)>,
    /// The remote cursor shape, decoded once per distinct image.
    cursor: Option<(usize, egui::TextureHandle, [u32; 2], [u32; 2])>,
    /// Whether the pointer is currently locked to the window for raw motion.
    pointer_locked: bool,
    /// The compositor only lets a focused window hold the pointer.
    focused: bool,
    /// A local menu, text field or modal temporarily owns input.
    local_input_blocked: bool,
    /// 指针上一帧是否落在全屏浮动栏上。
    chrome_hovered: bool,
    /// Sub-pixel motion carried between raw events.
    motion_remainder: [f64; 2],
    /// Report rate per physical mouse, for the high polling-rate notice.
    polling: crate::features::remote_input::polling::Monitor,
    polling_origin: Instant,
    /// Throttles the pointer diagnostics, which stay off unless
    /// `openuuyc::viewer::input` is enabled at debug.
    diagnostics_at: [Option<Instant>; 2],
    /// When the remote first said its pointer was hidden, for [`CURSOR_HIDE_GRACE`].
    hidden_since: Option<Instant>,
    /// Holds the compositor's own shortcuts back while this window has the
    /// keyboard, so the keys the remote desktop wants arrive here instead.
    shortcuts: Option<super::linux_shortcuts::Shortcuts>,
    /// Files dropped on the window, and drags carried out of it.
    drag: super::linux_drag::WindowDrag,
    /// 拖放借用的第二条 Wayland 通道（serial 与拖动落点都只有它给得了）。
    /// X11 会话里没有，拖放退回 winit + X 服务器那套。
    linked: Option<Arc<crate::platform::linux::drag_drop::linked::Linked>>,
    /// Mouse mode of the previous frame, to notice control being dropped.
    last_mouse_mode: MouseMode,
    /// 这块屏已经交给独立窗口，新屏接上之前不要再从同一个解码队列取帧。
    frame_paused: bool,
    /// 这个窗口分到的屏。主窗口默认持有全部，拖出成独立窗口的屏从它这里移过去。
    tabs: Vec<i32>,
    /// 把屏拖出成独立窗口、或关闭自己的请求交给 runner（窗口操作只能在事件循环里做）。
    requests: std_mpsc::Sender<WindowRequest>,
    /// 主窗口负责整个连接的键鼠控制与解码器池；独立窗口关闭时不能动它们。
    input_owns_connection: bool,
}

impl Player {
    fn new(
        session: Arc<NativeViewerSession>,
        screens: super::screens::ScreenPlayback,
        window: &Arc<Window>,
        proxy: EventLoopProxy<UiEvent>,
        PlayerWindow {
            tabs,
            requests,
            input_owns_connection,
        }: PlayerWindow,
    ) -> Result<Self> {
        session.ensure_running()?;
        // 屏目录与每屏的解码器由 `ScreenPlayback` 统一持有：同一个设备连接上的
        // 所有窗口共用它，才不会两块窗口各推一份同样的屏。它由 runner 持有并传进来。
        screens.register(Arc::clone(&session));
        let initial = Arc::clone(&session);
        let catalog = screens.screens();
        let owner = u64::from(window.id());
        let window_id = window.id();
        let wake = FrameWakeBridge::install(&session, proxy.clone(), window_id)?;
        // 挂在 winit 的 display 上的第二个客户端。进程里只建一次，之后的窗口
        // 只是在它那儿登记一下自己那块 surface。
        let linked = match crate::platform::linux::drag_drop::linked::Linked::attach(window) {
            Ok(linked) => {
                if let Err(error) = linked.watch(window) {
                    tracing::debug!(%error, "无法登记观看窗口的拖放通道");
                }
                Some(linked)
            }
            Err(error) => {
                tracing::debug!(%error, "没有可用的 Wayland 拖放通道");
                None
            }
        };
        let drag =
            super::linux_drag::WindowDrag::new(owner, &session.stream_control, linked.clone());
        Ok(Self {
            shortcuts: super::linux_shortcuts::Shortcuts::new(window),
            drag,
            linked,
            _shortcuts: crate::application::viewer_shortcuts::Watcher::new()?,
            session,
            initial,
            session_failed: false,
            screens,
            catalog,
            next_refresh: Instant::now(),
            topology_sequence: None,
            topology_focused: None,
            failed_screen: None,
            pending: None,
            proxy,
            window_id,
            wake: Some(wake),
            owner,
            stream_control_ui: StreamControlUi::default(),
            performance_mode: PerformancePanelMode::Compact,
            intercept_shortcuts: true,
            current: None,
            video_size: None,
            display_aspect: None,
            fullscreen: false,
            video_rect: None,
            pointer_in_video: false,
            last_position: None,
            modifiers: winit::keyboard::ModifiersState::empty(),
            held_keys: Vec::new(),
            physical_keys: Vec::new(),
            physical_buttons: 0,
            scale: window.scale_factor() as f32,
            error: None,
            cursor: None,
            pointer_locked: false,
            focused: window.has_focus(),
            local_input_blocked: false,
            chrome_hovered: false,
            motion_remainder: [0.0, 0.0],
            polling: Default::default(),
            polling_origin: Instant::now(),
            diagnostics_at: [None; 2],
            hidden_since: None,
            last_mouse_mode: MouseMode::View,
            frame_paused: false,
            tabs,
            requests,
            input_owns_connection,
        })
    }

    /// Holds the desktop's own shortcuts back while this window owns input for
    /// the remote, under the same gate and setting as the Windows keyboard hook.
    fn update_shortcuts(&mut self, context: &egui::Context, window: &Window) {
        if self.shortcuts.is_none() {
            return;
        }
        let input = self.session.stream_control.mouse();
        let gate = super::input_gate::InputGate {
            focused: self.focused,
            minimized: window.is_minimized() == Some(true),
            controlling: input.mode() != MouseMode::View,
            dragging: self.session.stream_control.drag_drop().interactive(),
            menu_open: self.stream_control_ui.open,
            has_video: self.video_size.is_some(),
        };
        let intercepts = super::input_gate::intercepts_shortcuts(input, self.intercept_shortcuts);
        if let Some(shortcuts) = self.shortcuts.as_mut() {
            shortcuts.update(gate, context, intercepts);
        }
    }

    /// Control that failed updates cursor and clipboard policy; control
    /// dropped by a connection loss or a stalled input channel comes back
    /// once this window is focused and nothing else holds the input.
    fn restore_control(&mut self, ctx: &egui::Context, window: &Window) {
        let control = &self.session.stream_control;
        let mode = control.mouse().mode();
        if self.last_mouse_mode != MouseMode::View
            && mode == MouseMode::View
            && control.mouse().error().is_some()
        {
            control.reconcile_mouse_failure();
        }
        self.last_mouse_mode = mode;
        if self.focused
            && window.is_minimized() != Some(true)
            && !control.drag_drop().interactive()
            // 菜单、浮动栏等本机界面占着输入时不要抢回来，否则会和
            // `update_input_gate` 的暂停每帧互相抵消。
            && !self.local_input_blocked
            && !self.stream_control_ui.open
            && !self.is_pending()
            && !ctx.any_popup_open()
            && !ctx.text_edit_focused()
            && let Err(error) = control.restore_input_control()
        {
            control.mouse().fail(error.to_string());
        }
    }

    fn control(&self) -> &crate::features::stream_control::StreamControlHandle {
        &self.session.stream_control
    }

    fn input(&self) -> &crate::features::remote_input::RemoteInput {
        self.session.stream_control.mouse()
    }

    /// 用户点了某块屏的 tab。点的是正在显示的那块，就等于取消这次切换（open 任务
    /// 随之 abort）；否则交给 [`Player::begin_screen`]。
    fn select(&mut self, screen_id: i32, window: &Arc<Window>) -> Result<()> {
        if screen_id == self.session.screen_id() {
            self.pending.take();
            return Ok(());
        }
        self.begin_screen(screen_id, window)
    }

    /// 请对端开始推这块屏，窗口仍画着原来的画面，直到 [`Player::poll_pending`] 把新
    /// 解码器接上。目录核对要重开正在显示的那块（轨道或绑定变了）时也直接走这里，
    /// 不经过 [`Player::select`] 的「点了当前屏＝取消」：Windows 的 `refresh_catalog`
    /// 先把 `slot.selected` 清掉再调 `begin_screen`，是同一个意思。
    ///
    /// 一律走 [`ScreenPlayback::open`]，和 Windows 的 `begin_screen` 一样：采集被停掉
    /// 之后要由它补发 action 8 重新开始，复用已有解码器时还得重新绑定轨道并要一个
    /// 关键帧。绕开它直接换会话，硬解下就会一直停在「等待画面」。
    fn begin_screen(&mut self, screen_id: i32, window: &Arc<Window>) -> Result<()> {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.id == screen_id)
        {
            return Ok(());
        }
        anyhow::ensure!(
            self.catalog.iter().any(|screen| screen.id == screen_id),
            "显示器已断开"
        );
        self.failed_screen = None;
        // 换屏前收尾，和 Windows 的 `begin_screen` 一样：不再等这块屏的显示设置，
        // 并把指针交回本地（切屏期间不转发输入，见 `local_ui_owns_input`）。
        self.control()
            .cancel_display_change(self.session.screen_id());
        self.release_pointer(window);
        let cancelled = self.pending.take().map(|pending| pending.id);
        let repaint = self.repaint();
        let (task, result) = self.screens.open(
            screen_id,
            ViewerDisplayHandle::default(),
            Some(self.session.screen_id()),
            cancelled,
            repaint,
        );
        self.pending = Some(PendingScreen {
            id: screen_id,
            task,
            result,
        });
        Ok(())
    }

    /// 把窗口接到某块屏的解码器上，并把旧屏的画面缓存全部作废。
    fn adopt(&mut self, screen_id: i32, session: Arc<NativeViewerSession>) -> Result<()> {
        session.ensure_running()?;
        // 对端把目标屏放到了当前这条轨道上时，拿回来的是同一个会话。这时不能再装一次
        // bridge：新 bridge 登记完之后旧的那个才 Drop，而它的 Drop 会把登记清掉，
        // 解码器随后丢掉每一帧，画面就永远停在「等待画面」。
        if self.wake.is_none() || !Arc::ptr_eq(&session, &self.session) {
            self.wake = Some(FrameWakeBridge::install(
                &session,
                self.proxy.clone(),
                self.window_id,
            )?);
        }
        self.session = session;
        self.session_failed = false;
        self.frame_paused = false;
        self.pending = None;
        self.current = None;
        self.video_size = None;
        self.video_rect = None;
        self.cursor = None;
        // 对端按「选中」的那块屏送指针与输入，换屏就得告诉它。
        self.screens.focus(screen_id);
        Ok(())
    }

    /// 窗口要关了：放下它发起、还等着回应的事，和 Windows `close_window` 一样——
    /// 设备切换的接管确认（否则它指着一个已经销毁的窗口），以及这块屏的显示设置。
    fn abandon_window(&self, window: WindowId) {
        if let Some(switcher) = self.device_switch() {
            switcher.cancel_takeover(window);
        }
        // 当前屏已经交给独立窗口时，它的显示设置归那边。
        if !self.frame_paused {
            self.control()
                .cancel_display_change(self.session.screen_id());
        }
    }

    /// 当前屏交给独立窗口之后没能接上别的屏（打开失败、对端拒绝）：窗口停在等待
    /// 画面，用户点别的标签才会再开。这期间它还登记在那块屏的唤醒上，独立窗口每来
    /// 一帧都会把它叫起来空画一遍，所以把这份登记撤掉。
    fn park_stranded(&mut self) {
        if self.frame_paused && self.pending.is_none() {
            self.wake = None;
        }
    }

    /// 交出去的那块屏又回到这个窗口（独立窗口关了）：接着从它的解码器取帧。
    fn resume_frames(&mut self) {
        self.frame_paused = false;
        if self.wake.is_none() {
            match FrameWakeBridge::install(&self.session, self.proxy.clone(), self.window_id) {
                Ok(wake) => self.wake = Some(wake),
                Err(error) => {
                    self.report("显示器操作失败", format!("无法恢复显示器画面：{error:#}"))
                }
            }
        }
    }

    /// 等正在打开的那块屏就绪。没就绪就什么都不做，`ScreenPlayback::open` 传进去的
    /// repaint 回调会把事件循环叫醒。返回这次有没有收到结果（顶栏要重画）。
    fn poll_pending(&mut self) -> bool {
        let Some(pending) = self.pending.as_mut() else {
            return false;
        };
        let ready = match pending.result.try_recv() {
            Ok(result) => Some((pending.id, result)),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                Some((pending.id, Err(anyhow!("屏幕加载已取消"))))
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => None,
        };
        let Some((id, result)) = ready else {
            return false;
        };
        self.pending = None;
        match result {
            // 打开期间映射又变了（加减虚拟屏后轨道重排、对端重新分配轨道）：拿到的这一路
            // 已经不是这块屏了。和 Windows `ScreenWindows::update` 一样作废这次结果，并让
            // 目录核对马上重跑，由它决定重开哪块。
            Ok(session)
                if !screen_mapped(
                    &self.screens.screens(),
                    id,
                    session.track_index,
                    session.screen_binding(),
                ) =>
            {
                tracing::debug!(screen_id = id, "screen mapping changed while opening");
                self.next_refresh = Instant::now();
            }
            Ok(session) => {
                if let Err(error) = self.adopt(id, session) {
                    self.report("显示器操作失败", format!("无法显示显示器 {id}：{error:#}"));
                }
            }
            Err(error) => {
                // 没切成：显示的还是原来那块（`session` 没动），记下这次失败不再重试。
                self.failed_screen = Some(self.failed_screen_for(id));
                self.report("显示器操作失败", format!("无法打开显示器 {id}：{error:#}"));
            }
        }
        true
    }

    /// 每 250 ms 对一遍目录，规则和 Windows 的 `refresh_catalog` 一样：正在看的屏从
    /// 目录里消失就换到第一块还在的；轨道或绑定和目录对不上就重开；对端报告它选中了
    /// 别的屏（刚加好的虚拟屏）就跟过去。`failed_screen` 挡住失败后的反复重试。
    ///
    /// 返回顶栏要不要重画：目录或等待中的屏变了，或者有错误要弹。Windows 每次核对完
    /// 都重画一遍；这里对端画面静止时没有新帧推着 UI 走，新加的屏的 tab、核对触发的
    /// 切屏的等待态和弹窗都得靠它排一次重画。
    ///
    /// `elsewhere` 是已经分给别的窗口的屏，主窗口不去收它们。
    fn refresh_catalog(&mut self, window: &Arc<Window>, elsewhere: &[i32]) -> bool {
        let now = Instant::now();
        if now < self.next_refresh {
            return false;
        }
        self.next_refresh = now + Duration::from_millis(250);
        let catalog = self.catalog.clone();
        let pending = self.pending.as_ref().map(|pending| pending.id);
        self.reconcile_catalog(window, elsewhere);
        self.catalog != catalog
            || self.pending.as_ref().map(|pending| pending.id) != pending
            || self.error.is_some()
    }

    fn reconcile_catalog(&mut self, window: &Arc<Window>, elsewhere: &[i32]) {
        let topology = self.screens.topology();
        if topology.sequence != self.topology_sequence {
            self.topology_sequence = topology.sequence;
            self.topology_focused = None;
        }
        let actual = self.screens.screens();
        self.catalog = actual.clone();
        for screen in &topology.transient_screens {
            if !self.catalog.iter().any(|known| known.id == screen.id) {
                self.catalog.push(screen.clone());
            }
        }
        // 新出现的屏（刚加的虚拟屏）当场收进主窗口，不等 runner 的 `sync_tabs`：
        // 对端往往在同一份快照里就报它为选中，下面要跟过去时它得已经在本窗口里。
        if self.input_owns_connection {
            collect_new_screens(&mut self.tabs, &self.catalog, elsewhere);
        }
        // 分给别的窗口的屏不在本窗口的 tabs 里；这里只丢掉已经消失的。
        self.tabs
            .retain(|id| self.catalog.iter().any(|screen| screen.id == *id));
        if self.catalog.is_empty() {
            return;
        }
        // 等着打开的那块屏已经不在对端的目录里了（比如点了它又马上在 tab 上关掉）：
        // 和 Windows 一样直接放弃这次切换，不当成失败弹窗，也不记进 `failed_screen`。
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| !actual.iter().any(|screen| screen.id == pending.id))
        {
            self.pending = None;
        }
        // 对端拓扑刚选中的那块屏（刚加好的虚拟屏），和 Windows 的 `topology_selection`
        // 一样只是换掉核对的对象，要不要真的切由 `replacement_target` 判断。
        let mut focus = None;
        if self.topology_focused != topology.selected_screen
            && topology.reported
            && !matches!(
                topology.action,
                Some(crate::features::stream_control::DisplayTopologyAction::Remove { .. })
            )
            && let Some(selected) = topology
                .selected_screen
                .filter(|id| actual.iter().any(|screen| screen.id == *id))
        {
            self.topology_focused = Some(selected);
            // 选中的是别的窗口里的屏（比如对独立窗口那块改了分辨率）：由那个窗口
            // 自己跟，这里不动，否则会退回本窗口第一块屏。
            if self.tabs.contains(&selected) {
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.id != selected)
                {
                    self.pending = None;
                }
                self.failed_screen = None;
                focus = Some(selected);
            }
        }
        if self.pending.is_some() {
            return;
        }
        let window_catalog: Vec<RemoteScreen> = self
            .catalog
            .iter()
            .filter(|screen| self.tabs.contains(&screen.id))
            .cloned()
            .collect();
        let target = replacement_target(
            &window_catalog,
            &actual,
            focus.unwrap_or_else(|| self.session.screen_id()),
            (self.session.track_index, self.session.screen_binding()),
            self.failed_screen,
        );
        if let Some(target) = target
            && let Err(error) = self.begin_screen(target, window)
        {
            self.failed_screen = Some(self.failed_screen_for(target));
            self.report(
                "显示器操作失败",
                format!("切到显示器 {target} 失败：{error:#}"),
            );
        }
    }

    /// 失败标记用的三元组，取不到目录条目时退回 `(id, -1, -1)`。
    fn failed_screen_for(&self, screen_id: i32) -> (i32, i32, i32) {
        self.catalog
            .iter()
            .find(|screen| screen.id == screen_id)
            .map_or((screen_id, -1, -1), screen_key)
    }

    /// 这个窗口现在要看哪几块屏：正在显示的那块，加上正在打开的那块。软解一次
    /// 只跟得上一条流，换屏期间就不再要原来那块（与 Windows 侧同一条规则）。
    ///
    /// 多个窗口的这份列表由 runner 合并后交给 `ScreenPlayback`（`sync_visible`）：
    /// 每个窗口各调一次 `set_visible` 会互相覆盖。
    fn visible_screens(&self) -> Vec<i32> {
        let switching = self.pending.as_ref().map(|pending| pending.id);
        let mut visible = Vec::new();
        if !(switching.is_some() && self.session.is_software()) {
            visible.push(self.session.screen_id());
        }
        visible.extend(switching);
        visible
    }

    /// 记下一条待显示的错误，下一帧由 [`Player::publish_error`] 做成弹窗。
    fn report(&mut self, title: &'static str, message: impl Into<String>) {
        self.error = Some((title, message.into()));
    }

    /// 把待显示的错误做成可关闭的弹窗。顶栏不放常驻错误：那一格是给屏标签的，
    /// Windows 对同样的错误也是弹窗。
    fn publish_error(&mut self, context: &egui::Context) {
        let Some((title, message)) = self.error.take() else {
            return;
        };
        crate::ui::controls::notice(
            context,
            "player-error",
            title,
            crate::ui::controls::DialogIcon::Error,
            message,
        );
    }

    /// 让事件循环尽快再跑一轮：切屏期间要画等待态，也要及时接过新解码器。
    fn repaint(&self) -> impl FnOnce() + Send + 'static {
        let proxy = self.proxy.clone();
        let window = self.window_id;
        move || {
            let _ = proxy.send_event(UiEvent::Repaint(UiRepaintEvent {
                window,
                generation: 0,
                pass: 0,
                when: Instant::now(),
            }));
        }
    }

    /// 连接结束了：和 Windows `ScreenWindows::update` 一样，只看连接开始时那条会话的
    /// shutdown 标志。连接结束时 controller 关的是它（`present_video` 交出去的
    /// close 句柄），切到别的屏之后也一样；它的解码器在后台出错不算连接结束。
    fn connection_ended(&self) -> bool {
        self.initial
            .shutdown
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// True when a decoded frame is waiting to go up.
    ///
    /// 正在显示的那块屏的解码器停了（硬解初始化失败、回退失败等写进 `fatal_error`
    /// 的错误），窗口不再有新帧：和 Windows 播放器每帧查 `fatal_error` 一样弹出
    /// 「显示器操作失败」，但不关窗口，用户还能切到别的屏。
    fn poll(&mut self) -> bool {
        self.confirm_neutral();
        if self.frame_paused {
            // 这条会话已经交给独立窗口了，它的错误由那边报告。
            return false;
        }
        let failure = if self.session_failed {
            None
        } else {
            mutex_lock(&self.session.fatal_error).clone()
        };
        if let Some(error) = failure {
            self.session_failed = true;
            tracing::error!(
                screen_id = self.session.screen_id(),
                %error,
                "displayed screen decoder stopped"
            );
            self.report("显示器操作失败", error);
        }
        mutex_lock(&self.session.frame_queue).is_some()
    }

    /// Take the newest decoded frame; the decoder already replaced any the
    /// UI cadence skipped.
    fn take_frame(&mut self) -> Option<DecodedVideoFrame> {
        if self.frame_paused {
            return None;
        }
        take_next_frame(
            &mut mutex_lock(&self.session.frame_queue),
            &self.session.performance,
        )
    }

    fn publish(&mut self, frame: &DecodedVideoFrame, transfer: Duration) {
        self.video_size = Some((frame.width, frame.height, frame.rotation));
        self.session.performance.record_rendered_frame(
            crate::diagnostics::performance::RenderedFrameTiming {
                is_new_picture: frame.is_new_picture,
                width: frame.width,
                height: frame.height,
                decoded_at: frame.decoded_at,
                local: frame.received_at.elapsed(),
                assembly: frame.assembly_delay,
                input_queue: frame.input_queue_delay,
                decode_pipeline: frame.decode_pipeline_delay,
                surface_transfer: transfer,
                present_wait: Duration::ZERO,
                render_queue: frame.decoded_at.elapsed().saturating_sub(transfer),
                sender_capture_at: frame.sender_timing.capture_at,
                sender_capture: frame.sender_timing.capture_delay,
                sender_encode: frame.sender_timing.encode_delay,
                sender_pacer: frame.sender_timing.pacer_delay,
                sender_total: frame.sender_timing.sending_delay,
                transport: frame.sender_timing.transport_delay,
            },
        );
    }

    /// Rotation swaps the aspect the video should be fitted to, and the
    /// device's local display aspect replaces it.
    fn display_size(&self) -> Option<(u32, u32)> {
        let (width, height, rotation) = self.video_size?;
        let size = if matches!(rotation % 360, 90 | 270) {
            (height, width)
        } else {
            (width, height)
        };
        Some(self.display_aspect.unwrap_or(size))
    }

    /// `menu_inset` moves the stream menu under the floating fullscreen bar.
    /// 请 runner 撤掉这个窗口。窗口创建与销毁都只能由事件循环做。
    fn request_close(&self, window: &Arc<Window>) {
        let _ = self.requests.send(WindowRequest::Close {
            window: window.id(),
        });
        let _ = self.proxy.send_event(UiEvent::Repaint(UiRepaintEvent {
            window: window.id(),
            generation: 0,
            pass: 0,
            when: Instant::now(),
        }));
    }

    fn draw(
        &mut self,
        ui: &mut egui::Ui,
        window: &Arc<Window>,
        menu_inset: f32,
    ) -> Option<VideoPlacement> {
        // The title bar is a panel above this ui; max_rect would include it and
        // push the first rows of the picture underneath it.
        let content = ui.available_rect_before_wrap();
        self.video_rect = None;
        let fullscreen = window.fullscreen().is_some();
        if self.fullscreen != fullscreen {
            // Entering or leaving fullscreen hands input back and closes the
            // stream menu, as the Windows fullscreen shortcut does.
            self.fullscreen = fullscreen;
            self.stream_control_ui.open = false;
            if !self.control().drag_drop().handoff_pending() {
                self.release_input(window);
            }
        }
        let display_aspect = self
            .session
            .stream_control
            .display_aspect(self.session.track_index);
        if self.display_aspect != display_aspect {
            // The same point now maps elsewhere on the remote, as on Windows.
            if !self.control().drag_drop().handoff_pending() {
                self.release_input(window);
            }
            self.display_aspect = display_aspect;
        }
        let scale = ui.ctx().pixels_per_point();
        self.scale = scale;
        let placement = self.display_size().map(|(width, height)| {
            let available = content.size();
            let video_aspect = (width as f32).max(1.0) / (height as f32).max(1.0);
            let area_aspect = available.x / available.y.max(1.0);
            let size = if video_aspect > area_aspect {
                egui::vec2(available.x, available.x / video_aspect)
            } else {
                egui::vec2(available.y * video_aspect, available.y)
            };
            let rect = egui::Rect::from_center_size(content.center(), size);
            self.video_rect = Some(rect);
            VideoPlacement {
                x: rect.min.x * scale,
                y: rect.min.y * scale,
                width: rect.width() * scale,
                height: rect.height() * scale,
                rotation: self.video_size.map_or(0, |size| size.2),
            }
        });
        if placement.is_none() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new("正在等待画面…")
                        .color(crate::ui::theme::MUTED)
                        .size(crate::ui::theme::BODY),
                );
            });
        }
        // These choices are saved per device and shared by its viewer windows.
        let preferences = self.session.stream_control.device_preferences();
        self.performance_mode = preferences.performance_mode;
        self.intercept_shortcuts = preferences.intercept_shortcuts;
        let mut view = super::stream_menu::LocalViewSettings {
            performance_mode: self.performance_mode,
            performance_always_visible: preferences.performance_always_visible,
            intercept_shortcuts: self.intercept_shortcuts,
            send_ctrl_alt_del: false,
        };
        let screen_id = self.session.screen_id();
        // "Follow local" asks for this monitor's size, as on Windows.
        let local_size = window
            .current_monitor()
            .map(|monitor| (monitor.size().width, monitor.size().height));
        show_stream_control_window(
            ui.ctx(),
            &self.session.stream_control,
            &mut self.stream_control_ui,
            &mut view,
            screen_id,
            local_size,
            egui::vec2(-114.0 - menu_inset, 52.0),
        );
        self.set_performance_mode(view.performance_mode);
        self.session
            .stream_control
            .set_performance_always_visible(view.performance_always_visible);
        if self.intercept_shortcuts != view.intercept_shortcuts {
            self.intercept_shortcuts = view.intercept_shortcuts;
            self.session
                .stream_control
                .set_intercept_shortcuts(view.intercept_shortcuts);
        }
        if view.send_ctrl_alt_del {
            let _ = self
                .session
                .stream_control
                .mouse()
                .send_ctrl_alt_del(self.owner);
        }
        super::show_performance_overlay(
            ui.ctx(),
            &self.session.performance,
            &self.session.stream_control.audio(),
            self.performance_mode,
            view.performance_always_visible,
            self.video_rect
                .unwrap_or_else(|| ui.available_rect_before_wrap()),
            "linux-player",
        );
        if self.focused && !self.stream_control_ui.open {
            super::polling_warning::show(
                ui.ctx(),
                self.session.stream_control.mouse(),
                ui.available_rect_before_wrap(),
            );
        }
        let available = !self.stream_control_ui.open && window.is_minimized() != Some(true);
        let map = self.point_mapper(ui.ctx());
        self.drag
            .tick(window, ui.ctx(), available, |position| map(position, false));
        self.drag.show(ui.ctx());
        self.update_input_gate(ui.ctx(), window);
        self.restore_control(ui.ctx(), window);
        self.update_pointer_lock(window);
        self.update_shortcuts(ui.ctx(), window);
        self.draw_remote_cursor(ui, window);
        placement
    }

    /// Paint the remote pointer over the video and hide the local one.
    ///
    /// In absolute mode the local pointer already stands for the remote one, so
    /// the shape is drawn where the local pointer is rather than at the position
    /// the remote sampled with it.
    fn draw_remote_cursor(&mut self, ui: &mut egui::Ui, window: &Arc<Window>) {
        if self.pointer_locked {
            // The remote draws its own pointer into the video in this mode.
            self.cursor_note("locked");
            return;
        }
        let controlling = self.session.stream_control.mouse().mode() != MouseMode::View;
        // An ordinary Windows desktop flaps this flag: it hides the pointer for
        // a keystroke and shows it on the next mouse move, several times a
        // second while someone types. Taking each report at face value makes
        // the pointer blink between the remote shape and the local arrow, so a
        // hide only counts once it has lasted.
        let hidden = match (
            self.session.stream_control.remote_cursor_hidden(),
            self.hidden_since,
        ) {
            (false, _) => {
                self.hidden_since = None;
                false
            }
            (true, Some(since)) => since.elapsed() >= CURSOR_HIDE_GRACE,
            (true, None) => {
                self.hidden_since = Some(Instant::now());
                false
            }
        };
        let pointer = ui.ctx().pointer_latest_pos();
        let Some((rect, position)) = self.video_rect.zip(pointer) else {
            window.set_cursor_visible(true);
            self.cursor_note("no rect or pointer");
            return;
        };
        // Over a panel, menu or the caption the local pointer is the one that
        // matters, and the remote shape would be painted underneath them.
        if !controlling || !self.hit_video_at(ui.ctx(), position) {
            window.set_cursor_visible(true);
            self.cursor_note("not over video");
            return;
        }
        // Exactly one pointer belongs over the picture. The host draws its own
        // into the frames while it is capturing the cursor, so a second one
        // painted here would double it. While it is not, this client owns the
        // job, and hiding the local pointer before there is a shape to put in
        // its place just leaves the picture with nothing to aim with -- which
        // is what a remote Windows desktop causes every time it hides its
        // pointer for someone typing.
        if self.session.stream_control.remote_cursor_captured() {
            window.set_cursor_visible(false);
            self.cursor_note("host draws it");
            return;
        }
        if hidden {
            window.set_cursor_visible(true);
            self.cursor = None;
            self.cursor_note("remote hid it");
            return;
        }
        // A hide that has not lasted long enough leaves no shape behind: the
        // report that carried it also cleared the one before it. The texture
        // decoded earlier carries the pointer across that gap.
        if let Some(cursor) = self.session.stream_control.remote_cursor() {
            let identity = Arc::as_ptr(&cursor.image) as usize;
            if self.cursor.as_ref().is_none_or(|(id, ..)| *id != identity) {
                match decode_cursor(ui.ctx(), &cursor.image) {
                    Some(entry) => self.cursor = Some((identity, entry.0, entry.1, entry.2)),
                    None => {
                        window.set_cursor_visible(true);
                        self.cursor = None;
                        self.cursor_note("undecodable");
                        return;
                    }
                }
            }
        }
        let Some((_, texture, size, hotspot)) = &self.cursor else {
            window.set_cursor_visible(true);
            self.cursor_note("no texture");
            return;
        };
        window.set_cursor_visible(false);
        // The cursor is authored in remote pixels; scale it with the video.
        let scale = self
            .session
            .stream_control
            .mouse_screen(self.session.track_index)
            .map_or(1.0, |(_, remote_width, _)| {
                rect.width() / (remote_width.max(1) as f32)
            });
        let size = egui::vec2(size[0] as f32 * scale, size[1] as f32 * scale);
        let origin = position - egui::vec2(hotspot[0] as f32 * scale, hotspot[1] as f32 * scale);
        // Painted straight into a foreground layer rather than through an
        // `Area`: an area restarts its fade-in whenever it skips a pass, and
        // this one skips every pass the pointer spends on a panel or the
        // caption, which left the shape at a fraction of its opacity. A bare
        // layer also registers no `AreaState`, so it cannot shadow the video in
        // `hit_video_at`.
        ui.ctx()
            .layer_painter(egui::LayerId::new(
                egui::Order::Tooltip,
                egui::Id::new("remote-cursor"),
            ))
            .image(
                texture.id(),
                egui::Rect::from_min_size(origin, size),
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        self.cursor_note("painted");
    }

    /// The player commands, placed in the window caption between the title and
    /// the window buttons. The Windows player has the same row, in the same
    /// order, with the same icons — 鼠标、麦克风、1:1、批注、插件、画质，最右是
    /// 窗口按钮。Linux 上还没实现的那几个画成禁用而不是不画：一眼就能看出缺什么。
    pub(super) fn title_bar_controls(
        &mut self,
        ui: &mut egui::Ui,
        bar: egui::Rect,
        window: &Arc<Window>,
    ) {
        // Leave the logo and alias on the left and the window buttons on the right.
        let left = bar.left() + TITLE_CAPTION_WIDTH;
        let right = bar.right() - crate::ui::theme::WINDOW_CONTROLS_WIDTH - 4.0;
        if right - left < 120.0 {
            return;
        }
        let rect = egui::Rect::from_min_max(
            egui::pos2(left, bar.top()),
            egui::pos2(
                right,
                bar.top() + crate::ui::theme::WINDOW_TITLE_CONTENT_HEIGHT,
            ),
        );
        let mut row = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        row.set_clip_rect(rect);
        row.spacing_mut().item_spacing.x = 4.0;

        // 画质与串流设置。Windows 的 Quality 也在最右。
        if caption_button(
            &mut row,
            ViewerCaptionIcon::Quality,
            self.stream_control_ui.open,
            "画质与串流设置",
        )
        .clicked()
        {
            self.stream_control_ui.open = !self.stream_control_ui.open;
        }

        // 这三样是 Windows 播放器里才有的东西：插件面板和批注整套界面都在那边，
        // 1:1 也还没做（Windows 那边只是往右下 request_inner_size，不挪窗口，
        // Wayland 上也做得到）。
        for (icon, hint) in [
            (ViewerCaptionIcon::Plugins, "Linux 主控还没有插件面板"),
            (ViewerCaptionIcon::Annotation, "Linux 主控还没有批注"),
            (ViewerCaptionIcon::OneToOne, "Linux 主控还没有 1:1 实际像素"),
        ] {
            row.add_enabled_ui(false, |ui| caption_button(ui, icon, false, ""))
                .inner
                .on_disabled_hover_text(hint);
        }

        // 这一排从右往左排，先加的落在右边，所以代码顺序和屏幕上的左右顺序相反：
        // 剪贴板写在麦克风前头，显示出来才在麦克风右边，和 Windows 一样。剪贴板按
        // 设备记住开没开，和串流设置里那个开关是同一份设置。
        let control = self.session.stream_control.snapshot();
        let clip = self.session.stream_control.clipboard().snapshot();
        let clip_hint = if !clip.enabled {
            "同步剪贴板：把本机剪贴板发送到远端，并把远端剪贴板取回本机".to_owned()
        } else if let Some(error) = clip.error.as_deref() {
            error.to_owned()
        } else if clip.active {
            "剪贴板同步中，点击关闭".to_owned()
        } else {
            "剪贴板同步已开启，等待通道就绪，点击关闭".to_owned()
        };
        if row
            .add_enabled_ui(
                clip.enabled || (control.ready && control.mouse_mode != MouseMode::View),
                |ui| caption_button(ui, ViewerCaptionIcon::Clipboard, clip.enabled, &clip_hint),
            )
            .inner
            .on_disabled_hover_text("请先连接并开启键鼠控制")
            .clicked()
        {
            crate::ui::controls::clear_notice(row.ctx(), "clipboard-request-error");
            match self
                .session
                .stream_control
                .set_clipboard_enabled(!clip.enabled)
            {
                Ok(()) => crate::ui::controls::clear_notice(row.ctx(), "clipboard-error"),
                Err(error) => crate::ui::controls::notice(
                    row.ctx(),
                    "clipboard-request-error",
                    "剪贴板同步",
                    crate::ui::controls::DialogIcon::Error,
                    error.to_string(),
                ),
            }
        }

        // 麦克风：和 Windows 同一套判定，用的是同一份共享状态。
        let mic = self.session.stream_control.microphone().snapshot();
        let mic_hint = if mic.pending {
            "麦克风：等待远端确认"
        } else if let Some(error) = mic.error.as_deref() {
            error
        } else if mic.capturing {
            "正在将麦克风发送到远端，点击关闭"
        } else if mic.enabled {
            "麦克风已开启，等待远端应用使用，点击关闭"
        } else {
            "将所选麦克风发送到远端（设备可在高级设置中切换）"
        };
        if row
            .add_enabled_ui(
                mic.enabled
                    || (self.session.stream_control.microphone_available()
                        && control.ready
                        && control.mouse_mode != MouseMode::View),
                |ui| caption_button(ui, ViewerCaptionIcon::Microphone, mic.enabled, mic_hint),
            )
            .inner
            .on_disabled_hover_text("请先连接支持麦克风的 Windows 设备并开启键鼠控制")
            .clicked()
        {
            crate::ui::controls::clear_notice(row.ctx(), "microphone-request-error");
            match self
                .session
                .stream_control
                .set_microphone_enabled(!mic.enabled)
            {
                Ok(()) => crate::ui::controls::clear_notice(row.ctx(), "microphone-error"),
                Err(error) => crate::ui::controls::notice(
                    row.ctx(),
                    "microphone-request-error",
                    "麦克风",
                    crate::ui::controls::DialogIcon::Error,
                    error.to_string(),
                ),
            }
        }

        // 键鼠控制。等键鼠松开、或输入通道卡住重连时都算「开着」，点一下就取消。
        let recovering = self.input().transport_recovering();
        let controlling = self.input().mode() != MouseMode::View
            || self.input().waiting_for_neutral()
            || recovering;
        let mouse_hint = if recovering {
            "键鼠控制正在自动恢复；点击取消".to_owned()
        } else if self.input().waiting_for_neutral() {
            format!(
                "等待松开全部键鼠；退出控制快捷键：{}",
                crate::application::viewer_shortcuts::label(ViewerShortcut::ReleaseMouse)
            )
        } else if controlling {
            format!(
                "退出控制（{}）",
                crate::application::viewer_shortcuts::label(ViewerShortcut::ReleaseMouse)
            )
        } else if self.input().keyboard_supported() {
            "开启键鼠控制".to_owned()
        } else {
            "开启鼠标控制（此平台暂未适配键盘）".to_owned()
        };
        // 桌面不让锁定指针时（见 `update_pointer_lock`）只在这里说一声，不弹窗挡输入。
        let mouse_hint = if self.input().relative_available() {
            mouse_hint
        } else {
            format!("{mouse_hint}\n当前桌面不允许锁定指针，已改用绝对坐标")
        };
        if row
            .add_enabled_ui(controlling || control.ready, |ui| {
                caption_button(ui, ViewerCaptionIcon::Mouse, controlling, &mouse_hint)
            })
            .inner
            .clicked()
        {
            if !controlling {
                crate::ui::controls::clear_notice(row.ctx(), "remote-input-error");
            }
            self.set_control(!controlling);
        }

        // 这里没有全屏按钮，和 Windows 一样：Linux 本来就有全屏的浮动操作栏
        // （`fullscreen_chrome.show`），里面画的还是这根条子，窗口按钮在全屏时变成
        // 「退出全屏」；进入全屏走快捷键。两边都不多一个入口。
        //
        // 这里也没有常驻的错误文字：屏标签要用这排按钮左边的全部宽度，错误一律走
        // 可关闭的弹窗（见 `publish_error`）。
        let free = egui::Rect::from_min_max(
            rect.min,
            egui::pos2((row.min_rect().left() - 6.0).max(rect.min.x), rect.max.y),
        );
        self.title_bar_screens(ui, free, bar, window);
    }

    /// 这台播放器能切到哪些设备。连接时由 controller 装进来，和 Windows 那份是同一个。
    fn device_switch(&self) -> Option<super::device_switch::DeviceSwitcher> {
        self.screens.device_switch.clone()
    }

    /// 顶栏中间的屏标签与码流信息：每块远端屏一个 tab（点一下切过去，右键是这块
    /// 屏的显示器设置，虚拟屏可以关掉），右边一格写当前画面的「分辨率 · 帧率 ·
    /// 编码」。Windows 的 caption 是同一套内容，只是那边的按钮是图标。
    ///
    /// `bar` 是整条标题栏（全屏时是浮动栏），拖动标签松手还在它里面就不算拖出。
    fn title_bar_screens(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        bar: egui::Rect,
        window: &Arc<Window>,
    ) {
        // 与 Windows 同一个宽度，够放 "3840×1620 · 35 FPS · H.265"。
        const METRICS_WIDTH: f32 = 184.0;
        const MINIMUM: f32 = 60.0;
        if rect.width() < MINIMUM {
            return;
        }
        let metrics_rect = egui::Rect::from_min_max(
            egui::pos2((rect.max.x - METRICS_WIDTH).max(rect.min.x), rect.min.y),
            rect.max,
        );
        let tabs_rect = egui::Rect::from_min_max(
            rect.min,
            egui::pos2((metrics_rect.min.x - 6.0).max(rect.min.x), rect.max.y),
        );
        let stats = self.session.performance.snapshot();
        // 和 Windows 的 caption 同一套判断：解码器的字符串里带 264/265 时以它为准
        // （那才是实际在解的东西，比如 "H.265 (VAAPI)"），否则退回编解码器名。
        let codec_info = if stats.video_format.contains("264") || stats.video_format.contains("265")
        {
            &stats.video_format
        } else {
            &stats.video_codec
        };
        let codec = if codec_info.contains("AV1") {
            "AV1"
        } else if codec_info.contains("265") || codec_info.contains("HEVC") {
            "H.265"
        } else if codec_info.contains("264") || codec_info.contains("AVC") {
            "H.264"
        } else {
            "—"
        };
        let parameters = stats.decoded_resolution.map_or_else(
            || "等待画面".to_owned(),
            |(width, height)| {
                format!(
                    "{width}×{height} · {:.0} FPS · {codec}",
                    stats.actual_fps.max(1.0)
                )
            },
        );
        let mut metrics = ui.new_child(egui::UiBuilder::new().max_rect(metrics_rect).layout(
            egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
        ));
        metrics.set_clip_rect(metrics_rect);
        metrics
            .add(
                egui::Label::new(
                    egui::RichText::new(parameters)
                        .size(crate::ui::theme::TINY)
                        .color(crate::ui::theme::MUTED),
                )
                .truncate(),
            )
            .on_hover_text(&stats.video_format);
        if tabs_rect.width() < 40.0 {
            return;
        }
        let catalog = self.catalog.clone();
        let tab_ids = self.tabs.clone();
        let control = self.session.stream_control.clone();
        let snapshot = control.snapshot();
        let selected = self.session.screen_id();
        let pending = self.pending.as_ref().map(|pending| pending.id);
        let software = self.session.is_software();
        // 拖出成独立窗口：软解一次只跟得上一条流，和 Windows 一样禁止；
        // 只剩一块屏时拖出去会让原窗口空掉，也不允许。
        let detachable = !software && tab_ids.len() > 1;
        let detach_hint = if software {
            "软解仅允许一个播放窗口，请使用标签切换屏幕"
        } else {
            "这个窗口只剩一块显示器，不能再拖出"
        };
        let requests = self.requests.clone();
        let proxy = self.proxy.clone();
        let mut open = None;
        let mut message = None;
        let mut tabs = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(tabs_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        tabs.set_clip_rect(tabs_rect);
        egui::ScrollArea::horizontal()
            .id_salt("player-screen-tabs")
            .auto_shrink([false, false])
            .scroll_source(
                egui::scroll_area::ScrollSource::SCROLL_BAR
                    | egui::scroll_area::ScrollSource::MOUSE_WHEEL,
            )
            .show(&mut tabs, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 3.0;
                    for id in tab_ids.iter().copied() {
                        let Some(screen) = catalog.iter().find(|screen| screen.id == id) else {
                            continue;
                        };
                        // 只有 UU 虚拟屏能这样关掉，物理屏要留着，否则等于拔掉
                        // 对端的显示器。类型按对端报的屏来看（和 Windows 一样），
                        // 拓扑里还没落定的瞬态屏不给关。
                        let closable = snapshot.ready
                            && snapshot.dpi_settings_supported
                            && snapshot.screens.len() > 1
                            && (!snapshot.topology.pending || snapshot.topology.dismissed)
                            && snapshot
                                .screens
                                .iter()
                                .any(|real| real.id == id && real.display.screen_type == 1);
                        let name = screen.label(&catalog);
                        let (response, close_clicked) = ui
                            .push_id(("player-screen-tab", id), |ui| {
                                crate::ui::controls::screen_tab(
                                    ui,
                                    &name,
                                    selected == id,
                                    pending == Some(id) || control.display_change_pending(id),
                                    !software,
                                    closable,
                                )
                            })
                            .inner;
                        let response = response.on_hover_text(&name);
                        if close_clicked {
                            super::stream_menu::topology_menu::request(
                                ui.ctx(),
                                &control,
                                id,
                                crate::features::stream_control::DisplayTopologyAction::Remove {
                                    screen_id: id,
                                },
                            );
                        } else if response.clicked() {
                            open = Some(id);
                        } else if response.drag_stopped()
                            && detachable
                            && released_off_bar(
                                ui.ctx().input(|input| input.pointer.latest_pos()),
                                bar,
                            )
                        {
                            // 把标签拖出标题栏：新窗口落在松手的位置（Wayland 交给合成器）。
                            request_detach(&requests, &proxy, window, ui.ctx(), id);
                        }
                        response.context_menu(|ui| {
                            super::stream_menu::menu_style(ui);
                            ui.set_width(crate::ui::theme::CONTEXT_MENU_WIDTH);
                            if let Some(error) = super::stream_menu::tab_display_menu(
                                ui,
                                &control,
                                id,
                                window
                                    .current_monitor()
                                    .map(|monitor| (monitor.size().width, monitor.size().height)),
                            ) {
                                message = Some(error);
                            }
                            ui.separator();
                            let row = crate::ui::controls::menu_row(
                                ui,
                                "在独立窗口打开",
                                "",
                                None,
                                detachable,
                                false,
                            )
                            .on_disabled_hover_text(detach_hint);
                            if row.clicked() {
                                request_detach(&requests, &proxy, window, ui.ctx(), id);
                                ui.close();
                            }
                        });
                    }
                    if snapshot.topology_support.create_visible
                        && crate::ui::controls::screen_add(
                            ui,
                            snapshot.ready
                                && snapshot.topology_support.create
                                && (!snapshot.topology.pending || snapshot.topology.dismissed),
                        )
                        .on_hover_text("添加虚拟屏")
                        .on_disabled_hover_text(
                            snapshot
                                .topology_support
                                .create_unavailable
                                .unwrap_or("正在处理显示器操作"),
                        )
                        .clicked()
                    {
                        let (width, height, _) =
                            super::stream_menu::topology_menu::local_parameters(
                                ui.ctx(),
                                &control,
                                window
                                    .current_monitor()
                                    .map(|monitor| (monitor.size().width, monitor.size().height)),
                            );
                        super::stream_menu::topology_menu::request(
                            ui.ctx(),
                            &control,
                            selected,
                            crate::features::stream_control::DisplayTopologyAction::Create {
                                width,
                                height,
                            },
                        );
                    }
                });
            });
        if let Some(error) = message {
            self.report("显示器操作失败", error);
        }
        if let Some(id) = open
            && let Err(error) = self.select(id, window)
        {
            self.failed_screen = Some(self.failed_screen_for(id));
            self.report("显示器操作失败", format!("切到显示器 {id} 失败：{error:#}"));
        }
    }

    /// 开关键鼠控制，和 Windows 顶栏的键鼠按钮同一套：打开时用这台设备记住的鼠标模式
    /// （串流设置里选的），失败记进共享的键鼠状态，由每帧调用的
    /// `show_stream_control_window` 弹出「控制已停止」。
    fn set_control(&mut self, enabled: bool) {
        let mode = if enabled {
            self.control().snapshot().mouse_preference
        } else {
            MouseMode::View
        };
        if let Err(error) = self.control().set_mouse_mode(mode) {
            self.input().fail(error.to_string());
        }
        if !enabled {
            self.release_keys();
        }
    }

    fn release_pointer(&mut self, window: &Arc<Window>) {
        if self.pointer_locked {
            let _ = window.set_cursor_grab(winit::window::CursorGrabMode::None);
            window.set_cursor_visible(true);
            self.pointer_locked = false;
        }
    }

    /// Raw pointer motion, used while the remote drives its own cursor.
    fn on_device_event(&mut self, device: DeviceId, event: &DeviceEvent) -> bool {
        if let DeviceEvent::Removed = event {
            self.polling.interrupt();
            self.polling.remove(device_key(device));
            return false;
        }
        let DeviceEvent::MouseMotion { delta } = event else {
            return false;
        };
        if !self.focused || self.local_input_blocked {
            return false;
        }
        let input = self.input();
        if input.mode() == MouseMode::View || input.waiting_for_neutral() {
            return false;
        }
        let relative = input.relative_mode();
        // Like the Windows viewer, rate every physical report while in
        // control, relative or not, per device.
        if self.focused && (delta.0 != 0.0 || delta.1 != 0.0) {
            // winit gives no event timestamp; arrival time stands in for the
            // message time Windows reports.
            let message_ms = self.polling_origin.elapsed().as_millis() as u32;
            if let Some(hz) = self
                .polling
                .observe(device_key(device), message_ms, Instant::now())
            {
                self.input().warn_high_polling(hz);
            }
        }
        if !relative {
            return false;
        }
        // Whole pixels go out now; the fraction waits for the next event so slow
        // movement is not rounded away.
        let x = delta.0 + self.motion_remainder[0];
        let y = delta.1 + self.motion_remainder[1];
        self.motion_remainder = [x.fract(), y.fract()];
        let (x, y) = (x.trunc(), y.trunc());
        if x == 0.0 && y == 0.0 {
            return false;
        }
        self.input().relative(self.owner, x as i32, y as i32);
        true
    }

    /// Keep the pointer inside the window while the remote owns it, so the
    /// local cursor cannot wander onto another window mid-game.
    fn update_pointer_lock(&mut self, window: &Arc<Window>) {
        let input = self.input();
        let wanted = self.focused
            && !self.local_input_blocked
            && input.relative_mode()
            && input.mode() != MouseMode::View;
        if wanted == self.pointer_locked {
            return;
        }
        if wanted {
            // X11 only confines; Wayland and Windows can lock in place.
            let locked = window
                .set_cursor_grab(winit::window::CursorGrabMode::Locked)
                .or_else(|_| window.set_cursor_grab(winit::window::CursorGrabMode::Confined));
            match locked {
                Ok(()) => {
                    window.set_cursor_visible(false);
                    self.pointer_locked = true;
                }
                Err(error) => {
                    tracing::warn!(%error, "无法锁定指针，改用绝对坐标");
                    // Without the pointer the deltas would drift; the session
                    // falls back to absolute positioning on its next poll.
                    // Only a notice, not a question: a modal would hold input
                    // back until dismissed, so the caption's mouse button says
                    // it in its tooltip instead.
                    self.input().set_relative_available(false);
                }
            }
        } else {
            let _ = window.set_cursor_grab(winit::window::CursorGrabMode::None);
            window.set_cursor_visible(true);
            self.pointer_locked = false;
            self.motion_remainder = [0.0, 0.0];
        }
    }

    /// The remote refuses input until every key and button held when control
    /// was granted has been released; nothing else reports that on Linux.
    fn confirm_neutral(&self) {
        let input = self.input();
        if !input.waiting_for_neutral() {
            return;
        }
        if self.physical_keys.is_empty() && self.physical_buttons == 0 {
            input.confirm_neutral(input.activation_generation());
        }
    }

    fn release_keys(&mut self) {
        for key in std::mem::take(&mut self.held_keys) {
            self.input().key(self.owner, key, false, None);
        }
    }

    /// 正在切屏或切设备，和 Windows `ScreenTabBar::is_pending` 同一个判断。
    fn is_pending(&self) -> bool {
        self.pending.is_some()
            || self
                .device_switch()
                .is_some_and(|switcher| switcher.is_switching())
    }

    fn local_ui_owns_input(&self, context: &egui::Context) -> bool {
        // 切屏与设备切换期间不转发输入：两边都在换轨道，键鼠很容易落到别处。
        self.stream_control_ui.open
            || self.is_pending()
            || context.any_popup_open()
            || context.text_edit_focused()
            || context.memory(|memory| memory.top_modal_layer().is_some())
            // 全屏时指针落在浮动栏上：鼠标在操作栏上，按键不该落到远端。
            || self.chrome_hovered
    }

    fn update_input_gate(&mut self, context: &egui::Context, window: &Arc<Window>) {
        let blocked = self.local_ui_owns_input(context);
        if blocked && !self.local_input_blocked {
            self.release_keys();
            self.input().pause_owner(self.owner);
            self.release_pointer(window);
            self.motion_remainder = [0.0, 0.0];
        }
        self.local_input_blocked = blocked;
    }

    fn on_focus_changed(&mut self, window: &Arc<Window>, focused: bool) {
        self.focused = focused;
        if !focused && self.control().drag_drop().handoff_pending() {
            // A drag being handed out of the window keeps its button state;
            // the handoff releases remote input itself.
            return;
        }
        if !focused {
            // Reports resume from a fresh window after focus returns.
            self.polling.interrupt();
            // A key released while another window has focus never reaches us.
            self.release_input(window);
        }
    }

    /// Hand back everything held for the remote; control resumes on the next
    /// click into the video.
    fn release_input(&mut self, window: &Arc<Window>) {
        self.release_pointer(window);
        self.release_keys();
        self.physical_keys.clear();
        self.physical_buttons = 0;
        self.modifiers = winit::keyboard::ModifiersState::empty();
        self.motion_remainder = [0.0, 0.0];
        self.input().pause_owner(self.owner);
        self.control().drag_drop().cancel_gesture(true);
    }

    /// Returns true when the event was consumed by remote input.
    fn on_window_event(
        &mut self,
        window: &Arc<Window>,
        context: &egui::Context,
        event: &WindowEvent,
        consumed_by_ui: bool,
    ) -> bool {
        self.update_input_gate(context, window);
        if self.drag.event(event) {
            return false;
        }
        // While a file drag owns the pointer, nothing reaches the remote; Escape
        // calls the drag off.
        let dragging = self.control().drag_drop().interactive();
        if dragging
            && let WindowEvent::KeyboardInput { event: key, .. } = event
            && key.state == ElementState::Pressed
            && key.physical_key
                == winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::Escape)
        {
            self.control().drag_drop().cancel_gesture(false);
            return true;
        }
        match event {
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
                false
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                // Focus changes can synthesize presses for keys already held
                // outside this window. Track them for neutral detection only.
                let down = event.state == ElementState::Pressed;
                if let Some(key) =
                    crate::application::viewer_shortcuts::physical_key(event.physical_key)
                {
                    if down {
                        if !self.physical_keys.contains(&key) {
                            self.physical_keys.push(key);
                        }
                    } else {
                        self.physical_keys.retain(|held| *held != key);
                    }
                    self.confirm_neutral();
                }
                let Some(key) =
                    crate::application::viewer_shortcuts::physical_key(event.physical_key)
                else {
                    return false;
                };
                if !down {
                    self.held_keys.retain(|held| *held != key);
                    return forward_key(self.input(), self.owner, key, false, true, consumed_by_ui);
                }
                if *is_synthetic || !self.focused || self.local_input_blocked || dragging {
                    return false;
                }
                if event.repeat {
                    // Keep the compositor's delay/rate. A repeat can only extend
                    // an accepted remote press, never execute a local shortcut
                    // again or acquire a key held before control was enabled.
                    return forward_repeat_key(
                        self.input(),
                        self.owner,
                        key,
                        false,
                        consumed_by_ui,
                    );
                }
                if let Some(action) = viewer_shortcut(self.modifiers, event.physical_key) {
                    self.run_shortcut(action, window);
                    return true;
                }
                let accepted =
                    forward_key(self.input(), self.owner, key, true, false, consumed_by_ui);
                if accepted {
                    if !self.held_keys.contains(&key) {
                        self.held_keys.push(key);
                    }
                }
                accepted
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.last_position = Some(*position);
                self.pointer_in_video = self.hit_video(context, *position);
                if dragging || self.reverse_drag(window, context, *position) {
                    return false;
                }
                self.send_position(*position);
                false
            }
            WindowEvent::CursorLeft { .. } => {
                self.pointer_in_video = false;
                self.last_position = None;
                false
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(bit) = mouse_button(*button).map(button_bit) {
                    if *state == ElementState::Pressed {
                        self.physical_buttons |= bit;
                    } else {
                        self.physical_buttons &= !bit;
                    }
                    self.confirm_neutral();
                }
                let pressed = *state == ElementState::Pressed;
                let Some(code) = mouse_button(*button) else {
                    return false;
                };
                if !pressed {
                    // The input queue checks owner/held state. Never let an
                    // overlay swallow a release belonging to the remote.
                    self.input().button(self.owner, code, false);
                    return true;
                }
                // egui answers `consumed_by_ui` from the pointer position of
                // its last pass, so a click right after the pointer left the
                // caption, a menu or a notice was kept for the UI and never
                // reached the remote. Where the pointer is now, against the
                // layers the user is looking at, decides instead; a widget
                // already dragging keeps the pointer.
                self.pointer_in_video = self.pointer_on_video(context);
                let ui_dragging = context.dragged_id().is_some();
                tracing::debug!(target: "openuuyc::viewer::input",
                    ?button, pressed,
                    consumed_by_ui, ui_dragging, in_video = self.pointer_in_video,
                    mode = ?self.input().mode(),
                    relative = self.input().relative_mode(),
                    neutral_wait = self.input().waiting_for_neutral(),
                    "mouse button");
                if dragging
                    || self.local_input_blocked
                    || !self.focused
                    || self.input().mode() == MouseMode::View
                {
                    return false;
                }
                if !self.pointer_in_video || ui_dragging {
                    return false;
                }
                self.input().button(self.owner, code, true);
                true
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.pointer_in_video = self.pointer_on_video(context);
                if dragging
                    || self.local_input_blocked
                    || !self.focused
                    || !self.pointer_in_video
                    || context.dragged_id().is_some()
                {
                    return false;
                }
                let (horizontal, amount) = match delta {
                    // One notch is 120 units on the wire, as Windows reports it.
                    MouseScrollDelta::LineDelta(x, y) => {
                        if x.abs() > y.abs() {
                            (true, *x * 120.0)
                        } else {
                            (false, *y * 120.0)
                        }
                    }
                    MouseScrollDelta::PixelDelta(position) => {
                        if position.x.abs() > position.y.abs() {
                            (true, position.x as f32 * 2.0)
                        } else {
                            (false, position.y as f32 * 2.0)
                        }
                    }
                };
                let amount = amount.round() as i32;
                if amount == 0 {
                    return false;
                }
                self.input().wheel(self.owner, amount, horizontal);
                true
            }
            _ => false,
        }
    }

    fn run_shortcut(&mut self, action: ViewerShortcut, window: &Arc<Window>) {
        match action {
            ViewerShortcut::ReleaseMouse => self.set_control(false),
            ViewerShortcut::Fullscreen => toggle_fullscreen(window),
            ViewerShortcut::Close => {
                self.release_keys();
                // 和窗口按钮一样只关这个窗口；连接还有别的窗口在播就留着。
                self.request_close(window);
            }
            ViewerShortcut::Performance => {
                self.set_performance_mode(self.performance_mode.next());
            }
        }
    }

    /// Shows the chosen panel and saves it for this device, as the Windows
    /// player does.
    fn set_performance_mode(&mut self, mode: PerformancePanelMode) {
        if self.performance_mode != mode {
            self.performance_mode = mode;
            self.session.stream_control.set_performance_mode(mode);
        }
    }

    /// True at most once every half second, so a diagnostic on a per-frame path
    /// does not flood the log. `slot` keeps the callers independent.
    fn diagnose(&mut self, slot: usize) -> bool {
        if !tracing::enabled!(target: "openuuyc::viewer::input", tracing::Level::DEBUG) {
            return false;
        }
        let now = Instant::now();
        if self.diagnostics_at[slot]
            .is_some_and(|last| now.duration_since(last) < Duration::from_millis(500))
        {
            return false;
        }
        self.diagnostics_at[slot] = Some(now);
        true
    }

    /// Why the remote pointer is or is not on screen. Every path out of
    /// `draw_remote_cursor` names itself here: the two ways this goes wrong --
    /// no pointer at all, and a pointer that does not aim where it looks -- are
    /// indistinguishable from a screenshot.
    fn cursor_note(&mut self, outcome: &str) {
        if !self.diagnose(0) {
            return;
        }
        tracing::debug!(target: "openuuyc::viewer::input",
            outcome,
            locked = self.pointer_locked,
            relative = self.input().relative_mode(),
            relative_available = self.input().relative_available(),
            mode = ?self.input().mode(),
            hidden = self.session.stream_control.remote_cursor_hidden(),
            hidden_for_ms = self.hidden_since.map(|since| since.elapsed().as_millis()),
            captured = self.session.stream_control.remote_cursor_captured(),
            shape = self.session.stream_control.remote_cursor().is_some(),
            in_video = self.pointer_in_video,
            "remote cursor");
    }

    /// Whether the pointer, where winit last reported it, is on the picture.
    fn pointer_on_video(&self, context: &egui::Context) -> bool {
        self.last_position
            .is_some_and(|position| self.hit_video(context, position))
    }

    fn hit_video(&self, context: &egui::Context, position: PhysicalPosition<f64>) -> bool {
        let scale = f64::from(context.pixels_per_point());
        self.hit_video_at(
            context,
            egui::pos2((position.x / scale) as f32, (position.y / scale) as f32),
        )
    }

    /// True when this point lands on the picture and not on any UI layer above it.
    fn hit_video_at(&self, context: &egui::Context, point: egui::Pos2) -> bool {
        self.video_rect.is_some_and(|rect| rect.contains(point))
            && context
                .layer_id_at(point)
                .is_none_or(|layer| layer.order == egui::Order::Background)
    }

    /// Maps a window position onto the remote screen the way pointer input
    /// does; `dragging` keeps a point off the video at the nearest edge
    /// instead of giving none.
    fn point_mapper(
        &self,
        context: &egui::Context,
    ) -> impl Fn(PhysicalPosition<f64>, bool) -> Option<crate::protocol::drag_drop::Point> + use<>
    {
        let context = context.clone();
        let rect = self.video_rect;
        let screen = self.control().mouse_screen(self.session.track_index);
        move |position, dragging| {
            let rect = rect?;
            let (screen, remote_width, remote_height) = screen?;
            let scale = f64::from(context.pixels_per_point().max(0.1));
            let logical = egui::pos2((position.x / scale) as f32, (position.y / scale) as f32);
            if !dragging
                && !(rect.contains(logical)
                    && context
                        .layer_id_at(logical)
                        .is_none_or(|layer| layer.order == egui::Order::Background))
            {
                return None;
            }
            let width = f64::from(rect.width());
            let height = f64::from(rect.height());
            if width <= 0.0 || height <= 0.0 {
                return None;
            }
            let local_x = f64::from(logical.x - rect.min.x);
            let local_y = f64::from(logical.y - rect.min.y);
            Some(crate::protocol::drag_drop::Point {
                screen,
                x: (local_x.clamp(0.0, width - 1.0) / width)
                    .min(1.0 - 1.0 / f64::from(remote_width.max(1))),
                y: (local_y.clamp(0.0, height - 1.0) / height)
                    .min(1.0 - 1.0 / f64::from(remote_height.max(1))),
            })
        }
    }

    /// A remote drag carried out of the window becomes a file drag here, if
    /// the peer can hand one over.
    fn reverse_drag(
        &self,
        window: &Arc<Window>,
        context: &egui::Context,
        position: PhysicalPosition<f64>,
    ) -> bool {
        let input = self.input();
        if input.relative_mode() || !input.owner_holds_left(self.owner) {
            return false;
        }
        let size = window.inner_size();
        if position.x >= 0.0
            && position.y >= 0.0
            && position.x < f64::from(size.width)
            && position.y < f64::from(size.height)
        {
            return false;
        }
        let drag = self.control().drag_drop();
        if !drag.available() {
            return false;
        }
        let Some(point) = self.point_mapper(context)(position, true) else {
            return false;
        };
        drag.probe(self.owner, point, input.clone()).is_ok()
    }

    fn send_position(&mut self, position: PhysicalPosition<f64>) {
        if !self.focused || self.local_input_blocked {
            return;
        }
        let input = self.input();
        if input.mode() == MouseMode::View || input.waiting_for_neutral() {
            return;
        }
        let dragging = input.owner_holds_buttons(self.owner);
        if !dragging && !self.pointer_in_video {
            return;
        }
        let Some(rect) = self.video_rect else {
            return;
        };
        let Some((screen, remote_width, remote_height)) =
            self.control().mouse_screen(self.session.track_index)
        else {
            return;
        };
        let local_x = (position.x / f64::from(self.scale_factor())) - f64::from(rect.min.x);
        let local_y = (position.y / f64::from(self.scale_factor())) - f64::from(rect.min.y);
        let width = f64::from(rect.width());
        let height = f64::from(rect.height());
        if width <= 0.0 || height <= 0.0 {
            return;
        }
        // The host multiplies by the screen size and rounds, so the last
        // physical pixel must stay inside the screen.
        let x = (local_x.clamp(0.0, width - 1.0) / width)
            .min(1.0 - 1.0 / f64::from(remote_width.max(1)));
        let y = (local_y.clamp(0.0, height - 1.0) / height)
            .min(1.0 - 1.0 / f64::from(remote_height.max(1)));
        input.absolute(self.owner, screen, x, y);
    }

    fn scale_factor(&self) -> f32 {
        self.scale.max(0.1)
    }
}

/// egui-winit always consumes Tab for local focus navigation, even when no
/// local editor owns input. Releases always reach the owner-aware queue.
fn forward_key(
    input: &crate::features::remote_input::RemoteInput,
    owner: u64,
    key: u16,
    down: bool,
    local_ui: bool,
    consumed_by_ui: bool,
) -> bool {
    if down && (local_ui || (consumed_by_ui && key != 9)) {
        return false;
    }
    input.key(owner, key, down, None)
}

fn forward_repeat_key(
    input: &crate::features::remote_input::RemoteInput,
    owner: u64,
    key: u16,
    local_ui: bool,
    consumed_by_ui: bool,
) -> bool {
    // A repeat only extends a press this window already sent: it never
    // acquires a key held before control began or revives a retired one.
    input.owner_holds_key(owner, key)
        && forward_key(input, owner, key, true, local_ui, consumed_by_ui)
}

#[cfg(test)]
mod input_tests {
    use super::*;
    use crate::features::remote_input::{InputEvent, InputWork, RemoteInput};
    use crate::protocol::keyboard::KeyboardKey;

    const OWNER: u64 = 42;

    fn ready_input() -> RemoteInput {
        let input = RemoteInput::default();
        input.set_keyboard_platform(1);
        input.set_ready(true);
        input.enable(MouseMode::Local, false).unwrap();
        input.confirm_neutral(input.activation_generation());
        input
    }

    async fn submitted(input: &RemoteInput) -> InputEvent {
        let InputWork::Event(queued) = tokio::time::timeout(Duration::from_secs(1), input.next())
            .await
            .expect("expected an input event")
        else {
            panic!("unexpected recovery");
        };
        let event = queued.event.clone();
        input.complete(&queued, Ok(()));
        event
    }

    #[tokio::test]
    async fn remote_shift_tab_survives_egui_focus_navigation() {
        let input = ready_input();
        for (key, consumed) in [(16, false), (9, true)] {
            assert!(forward_key(&input, OWNER, key, true, false, consumed));
            assert!(
                matches!(submitted(&input).await, InputEvent::Key { key: k, down: true, .. } if k == KeyboardKey::Windows(key))
            );
        }
        for key in [9, 16] {
            assert!(forward_key(&input, OWNER, key, false, true, true));
            assert!(
                matches!(submitted(&input).await, InputEvent::Key { key: k, down: false, .. } if k == KeyboardKey::Windows(key))
            );
            assert!(!input.owner_holds_key(OWNER, key));
        }
    }

    #[tokio::test]
    async fn editor_blocks_new_keys_without_swallowing_remote_release() {
        let input = ready_input();
        assert!(forward_key(&input, OWNER, 65, true, false, false));
        submitted(&input).await;
        assert!(!forward_key(&input, OWNER, 9, true, true, true));
        assert!(!forward_key(&input, OWNER, 66, true, true, false));
        assert!(!forward_key(&input, OWNER + 1, 65, false, true, true));
        assert!(input.owner_holds_key(OWNER, 65));
        assert!(forward_key(&input, OWNER, 65, false, true, true));
        assert!(matches!(
            submitted(&input).await,
            InputEvent::Key {
                key: KeyboardKey::Windows(65),
                down: false,
                ..
            }
        ));
        assert!(!input.owner_holds_key(OWNER, 65));
    }

    #[tokio::test]
    async fn local_ui_takeover_releases_submitted_key_and_button() {
        let input = ready_input();
        assert!(forward_key(&input, OWNER, 65, true, false, false));
        submitted(&input).await;
        input.button(OWNER, 1, true);
        submitted(&input).await;
        input.pause_owner(OWNER);
        assert!(matches!(
            submitted(&input).await,
            InputEvent::Key {
                key: KeyboardKey::Windows(65),
                down: false,
                ..
            }
        ));
        assert!(matches!(
            submitted(&input).await,
            InputEvent::Button {
                button: 1,
                down: false
            }
        ));
        assert!(!input.owner_holds_key(OWNER, 65));
        assert!(!input.owner_holds_buttons(OWNER));
    }

    #[tokio::test]
    async fn held_letters_backspace_and_arrows_send_repeated_presses_then_release() {
        let input = ready_input();
        for key in [65, 8, 37, 9] {
            let consumed = key == 9;
            assert!(forward_key(&input, OWNER, key, true, false, consumed));
            for _ in 0..3 {
                assert!(forward_repeat_key(&input, OWNER, key, false, consumed));
            }
            assert!(forward_key(&input, OWNER, key, false, false, consumed));
            for down in [true, true, true, true, false] {
                let event = submitted(&input).await;
                let wire: serde_json::Value = serde_json::from_slice(&event.encode()).unwrap();
                assert_eq!(wire["key"], key);
                assert_eq!(
                    wire["action"],
                    if down { "kbd_press" } else { "kbd_release" }
                );
            }
            assert!(!input.owner_holds_key(OWNER, key));
            assert!(!forward_repeat_key(&input, OWNER, key, false, consumed));
        }
    }

    #[tokio::test]
    async fn repeat_cannot_acquire_or_revive_a_retired_key() {
        let input = ready_input();
        assert!(!forward_repeat_key(&input, OWNER, 65, false, false));
        assert!(forward_key(&input, OWNER, 65, true, false, false));
        submitted(&input).await;
        assert!(!forward_repeat_key(&input, OWNER + 1, 65, false, false));
        assert!(!forward_repeat_key(&input, OWNER, 65, true, false));
        assert!(!forward_repeat_key(&input, OWNER, 65, false, true));
        assert!(input.owner_holds_key(OWNER, 65));
        input.pause_owner(OWNER);
        assert!(matches!(
            submitted(&input).await,
            InputEvent::Key {
                key: KeyboardKey::Windows(65),
                down: false,
                ..
            }
        ));
        assert!(!forward_repeat_key(&input, OWNER, 65, false, false));
        assert!(forward_key(&input, OWNER, 65, true, false, false));
        submitted(&input).await;
        input.disable();
        assert!(matches!(
            submitted(&input).await,
            InputEvent::Key {
                key: KeyboardKey::Windows(65),
                down: false,
                ..
            }
        ));
        input.enable(MouseMode::Local, false).unwrap();
        input.confirm_neutral(input.activation_generation());
        assert!(!forward_repeat_key(&input, OWNER, 65, false, false));
        // A fresh physical press is required after control is restored.
        assert!(forward_key(&input, OWNER, 65, true, false, false));
        assert!(forward_repeat_key(&input, OWNER, 65, false, false));
        assert!(forward_key(&input, OWNER, 65, false, false, false));
        for down in [true, true, false] {
            assert!(
                matches!(submitted(&input).await, InputEvent::Key { key: KeyboardKey::Windows(65), down: d, .. } if d == down)
            );
        }
    }
}

impl Player {
    /// 把拖放通道上已经读到的事件分发给它自己的队列。
    pub(super) fn pump_drag(&self) {
        if let Some(linked) = &self.linked {
            linked.pump();
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Some(linked) = &self.linked {
            linked.unwatch(self.owner);
        }
        // 顺序和 Windows 的 `ScreenWindows::drop` 一致：先丢掉正在打开的那块屏
        // （abort 掉 open 任务），再关掉每条解码会话、取消这个窗口的任务域，别的屏的
        // 解码线程因此马上收到停止信号，而不是等到最后一个 `ScreenPlayback` 克隆
        // 释放时在 tokio worker 上 join。
        self.pending.take();
        self.release_keys();
        if self.input_owns_connection {
            self.input().disable();
            self.screens.close_decoders();
        } else {
            // 独立窗口只交回自己持有的输入；别的窗口还在用这个连接。
            self.input().pause_owner(self.owner);
        }
    }
}

/// 本窗口该换到哪块屏，`None` 表示不用动。规则和 Windows `refresh_catalog` 里的
/// `replacements` 一样：
/// - `focus`（正在显示的那块，或对端拓扑刚选中的那块）还只在拓扑的瞬态清单里：
///   对端正在改，等一下再说；
/// - 它的轨道和绑定都和正在显示的会话 `(track, binding)` 对得上：就是现在这一路，不动；
/// - 对不上（包括 `focus` 就是正在显示的那块、但对端把它挪到了别的轨道）：重开它；
/// - 它整个从目录里消失了：退回目录里第一块真在的屏；
/// - 选出来的那块刚失败过、而且没变样（`failed`）：不再重试。
fn replacement_target(
    catalog: &[RemoteScreen],
    actual: &[RemoteScreen],
    focus: i32,
    (track, binding): (i32, u64),
    failed: Option<(i32, i32, i32)>,
) -> Option<i32> {
    let selected = catalog.iter().find(|screen| screen.id == focus);
    if selected.is_some_and(|screen| !actual.iter().any(|real| real.id == screen.id)) {
        return None;
    }
    let target = match selected {
        Some(screen) if track == screen.video_track_index && binding == screen_binding(screen) => {
            return None;
        }
        Some(screen) => screen.id,
        // 目录按窗口分过之后，`focus`（比如对端拓扑选中的屏）可能属于别的窗口。
        // 这时退回本窗口的第一块屏，但如果那就是正在显示的这一路，就不动它。
        None => {
            let screen = catalog
                .iter()
                .find(|screen| actual.iter().any(|real| real.id == screen.id))?;
            if track == screen.video_track_index && binding == screen_binding(screen) {
                return None;
            }
            screen.id
        }
    };
    if actual
        .iter()
        .any(|screen| screen.id == target && failed == Some(screen_key(screen)))
    {
        return None;
    }
    Some(target)
}

/// 把目录里没分给任何窗口的屏补进 `tabs`，返回有没有补。`elsewhere` 是别的窗口
/// 已经分到的屏，和 Windows 的 `refresh_catalog` 把新屏交给 owner 窗口同一条规则。
fn collect_new_screens(tabs: &mut Vec<i32>, catalog: &[RemoteScreen], elsewhere: &[i32]) -> bool {
    let before = tabs.len();
    for screen in catalog {
        if !tabs.contains(&screen.id) && !elsewhere.contains(&screen.id) {
            tabs.push(screen.id);
        }
    }
    tabs.len() != before
}

/// 刚打开的这一路 `(track, binding)` 还是不是目录里这块屏的，和 Windows
/// `ScreenWindows::update` 接上新会话前的核对一样。
fn screen_mapped(actual: &[RemoteScreen], screen_id: i32, track: i32, binding: u64) -> bool {
    actual.iter().any(|screen| {
        screen.id == screen_id
            && screen.video_track_index == track
            && screen_binding(screen) == binding
    })
}

/// 请 runner 把这块屏挪到独立窗口。请求带唤醒，免得事件循环闲着等到下次输入。
fn request_detach(
    requests: &std_mpsc::Sender<WindowRequest>,
    proxy: &EventLoopProxy<UiEvent>,
    window: &Window,
    context: &egui::Context,
    screen_id: i32,
) {
    let _ = requests.send(WindowRequest::Detach {
        screen_id,
        position: global_pointer(window, context),
    });
    let _ = proxy.send_event(UiEvent::Repaint(UiRepaintEvent {
        window: window.id(),
        generation: 0,
        pass: 0,
        when: Instant::now(),
    }));
}

/// 标签拖动松手的地方是否已经出了标题栏。egui 把手抖一下、按住不放都算拖动；
/// 和 Windows `detach_tab` 一样，松手还在原窗口标题栏里就什么都不做。指针在拖动中
/// 离开了窗口时 egui 不再有它的位置（`None`），那一定已经在栏外了。
fn released_off_bar(release: Option<egui::Pos2>, bar: egui::Rect) -> bool {
    release.is_none_or(|point| !bar.contains(point))
}

/// 新窗口的左上角夹进松手点所在（不在任何一块上就取最近）的显示器，和 Windows
/// `constrain_drag_position` 一样以指针的目标显示器为准。Windows 只管上沿（任务栏
/// 在工作区里扣掉了）；这里拿不到工作区，就让整条标题栏留在显示器里，免得在屏幕
/// 边缘松手后窗口抓不回来。
fn constrain_to_monitor(
    position: PhysicalPosition<i32>,
    cursor: PhysicalPosition<i32>,
    size: winit::dpi::PhysicalSize<u32>,
    title: u32,
    monitors: &[(PhysicalPosition<i32>, winit::dpi::PhysicalSize<u32>)],
) -> PhysicalPosition<i32> {
    let distance = |(origin, extent): &(PhysicalPosition<i32>, winit::dpi::PhysicalSize<u32>)| {
        let right = origin.x + extent.width as i32 - 1;
        let bottom = origin.y + extent.height as i32 - 1;
        let dx = (origin.x - cursor.x).max(cursor.x - right).max(0);
        let dy = (origin.y - cursor.y).max(cursor.y - bottom).max(0);
        i64::from(dx) * i64::from(dx) + i64::from(dy) * i64::from(dy)
    };
    let Some((origin, extent)) = monitors.iter().min_by_key(|monitor| distance(monitor)) else {
        return position;
    };
    let width = extent.width.min(i32::MAX as u32) as i32;
    let height = extent.height.min(i32::MAX as u32) as i32;
    let max_x = origin.x + (width - size.width.min(width as u32) as i32);
    let max_y = origin.y + (height - title.min(height as u32) as i32);
    PhysicalPosition::new(
        position.x.clamp(origin.x, max_x),
        position.y.clamp(origin.y, max_y),
    )
}

/// 指针此刻的全局物理位置。Wayland 不给窗口位置，返回 `None`，新窗口由合成器摆放。
fn global_pointer(window: &Window, context: &egui::Context) -> Option<PhysicalPosition<i32>> {
    let local = context.input(|input| input.pointer.latest_pos())?;
    let origin = window.outer_position().ok()?;
    let scale = window.scale_factor();
    Some(PhysicalPosition::new(
        origin.x + (f64::from(local.x) * scale) as i32,
        origin.y + (f64::from(local.y) * scale) as i32,
    ))
}

/// 两个唤醒时刻里更早的那个，任一为空就取另一个。
fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        _ => a.or(b),
    }
}

/// 一块屏在会话里的绑定值，和 `NativeViewerSession::bind_screen` 写进去的是同一套。
fn screen_binding(screen: &RemoteScreen) -> u64 {
    (screen.id as u32 as u64) | ((screen.display.screen_type as u32 as u64) << 32)
}

/// 失败标记的三元组：屏的 id、轨道、类型。任一项变了就值得再试一次。
fn screen_key(screen: &RemoteScreen) -> (i32, i32, i32) {
    (
        screen.id,
        screen.video_track_index,
        screen.display.screen_type,
    )
}

/// The decoder drops every decoded frame unless a render thread is registered
/// with the session, which is how the Windows player signals that its window is
/// showing video. Here the frames are drawn by the UI thread, so this thread
/// holds that registration and turns each decoded frame into a repaint request.
struct FrameWakeBridge {
    wake: super::FrameWake,
    running: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FrameWakeBridge {
    fn install(
        session: &NativeViewerSession,
        proxy: EventLoopProxy<UiEvent>,
        window: WindowId,
    ) -> Result<Self> {
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let worker_running = Arc::clone(&running);
        let thread = std::thread::Builder::new()
            .name("Video wake".to_owned())
            .spawn(move || {
                while worker_running.load(std::sync::atomic::Ordering::Acquire) {
                    std::thread::park();
                    if !worker_running.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let _ = proxy.send_event(UiEvent::Repaint(UiRepaintEvent {
                        window,
                        generation: 0,
                        pass: 0,
                        when: Instant::now(),
                    }));
                }
            })
            .context("create video wake thread")?;
        let handle = thread.thread().clone();
        session.frame_wake.install_render_thread(handle.clone());
        handle.unpark();
        Ok(Self {
            wake: session.frame_wake.clone(),
            running,
            thread: Some(thread),
        })
    }
}

impl Drop for FrameWakeBridge {
    fn drop(&mut self) {
        // 只撤销这一份登记：这一路解码器可能已经交给独立窗口了，不能连别人的
        // 唤醒一起清掉。
        if let Some(thread) = self.thread.take() {
            self.wake.remove_render_thread(thread.thread());
            self.running
                .store(false, std::sync::atomic::Ordering::Release);
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

fn toggle_fullscreen(window: &Arc<Window>) {
    if window.fullscreen().is_some() {
        set_fullscreen(window, None);
    } else {
        set_fullscreen(window, Some(winit::window::Fullscreen::Borderless(None)));
    }
}

/// The wire codes `RemoteInput` accepts, which are not the 1..=6 numbering the
/// plugin hotkeys use.
fn mouse_button(button: MouseButton) -> Option<u32> {
    Some(match button {
        MouseButton::Left => 1,
        MouseButton::Right => 2,
        MouseButton::Middle => 16,
        MouseButton::Back => 32,
        MouseButton::Forward => 64,
        MouseButton::Other(_) => return None,
    })
}

/// One bit per wire code, for tracking what is physically held.
fn button_bit(code: u32) -> u8 {
    match code {
        1 => 1,
        2 => 2,
        16 => 4,
        32 => 8,
        64 => 16,
        _ => 0,
    }
}

fn viewer_shortcut(
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

/// How long the remote has to keep saying its pointer is hidden before this
/// client believes it. Long enough to swallow the flap a desktop produces while
/// someone types, short enough that a real hide still feels immediate.
const CURSOR_HIDE_GRACE: Duration = Duration::from_millis(600);

/// Decode the remote cursor PNG into a texture, with its size and hotspot.
fn decode_cursor(
    ctx: &egui::Context,
    image: &crate::features::remote_cursor::CursorImage,
) -> Option<(egui::TextureHandle, [u32; 2], [u32; 2])> {
    if image.png.is_empty() {
        return None;
    }
    let decoded = image::load_from_memory_with_format(&image.png, image::ImageFormat::Png)
        .map_err(|error| tracing::debug!(%error, "远端光标图像无法解码"))
        .ok()?
        .to_rgba8();
    let size = [decoded.width() as usize, decoded.height() as usize];
    let texture = ctx.load_texture(
        "remote-cursor",
        egui::ColorImage::from_rgba_unmultiplied(size, decoded.as_raw()),
        egui::TextureOptions::LINEAR,
    );
    Some((texture, [decoded.width(), decoded.height()], image.hotspot))
}

/// Audio-only sessions use a fixed-size window, as on Windows
/// (`windows_presenter/connection.rs`).
fn configure_audio_window(window: &Window, compact: bool) {
    set_fullscreen(window, None);
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

/// A nonzero key for a winit input device, for the per-device rate monitor.
fn device_key(device: DeviceId) -> usize {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    device.hash(&mut hasher);
    (hasher.finish() as usize).max(1)
}

#[cfg(test)]
mod screen_tests {
    use super::*;
    use crate::features::stream_control::{RemoteDisplayInfo, RemoteDisplayMode};

    fn screen(id: i32, track: i32, screen_type: i32) -> RemoteScreen {
        let mode = RemoteDisplayMode {
            width: 1920,
            height: 1080,
            pixel_width: 1920,
            pixel_height: 1080,
        };
        RemoteScreen {
            id,
            name: format!("DISPLAY{id}"),
            primary: id == 0,
            video_track_index: track,
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            display: RemoteDisplayInfo {
                preferred: None,
                current: mode,
                initial: None,
                modes: vec![mode],
                current_dpi: 96,
                recommended_dpi: 96,
                dpis: vec![96],
                screen_type,
                builtin_screen_type: screen_type,
                dpr: 1.0,
                resolution_type: 0,
                left: 0,
                top: 0,
            },
        }
    }

    fn playing(screen: &RemoteScreen) -> (i32, u64) {
        (screen.video_track_index, screen_binding(screen))
    }

    #[test]
    fn matching_track_and_binding_needs_nothing() {
        let actual = vec![screen(0, 0, 0), screen(1, 1, 0)];
        assert_eq!(
            replacement_target(&actual, &actual, 1, playing(&actual[1]), None),
            None
        );
    }

    /// The case review round 4 found unreachable: the displayed screen moved to
    /// another track. The target is the displayed screen itself, so the caller
    /// must open it directly instead of treating it as "clicked the current tab".
    #[test]
    fn displayed_screen_on_a_new_track_is_reopened() {
        let before = screen(1, 1, 0);
        let actual = vec![screen(0, 0, 0), screen(1, 2, 0)];
        assert_eq!(
            replacement_target(&actual, &actual, 1, playing(&before), None),
            Some(1)
        );
        // Same track, but the peer now reports it as a virtual screen.
        let actual = vec![screen(0, 0, 0), screen(1, 1, 1)];
        assert_eq!(
            replacement_target(&actual, &actual, 1, playing(&before), None),
            Some(1)
        );
    }

    #[test]
    fn a_failed_screen_is_not_retried_until_it_changes() {
        let before = screen(1, 1, 0);
        let actual = vec![screen(0, 0, 0), screen(1, 2, 0)];
        let failed = Some(screen_key(&actual[1]));
        assert_eq!(
            replacement_target(&actual, &actual, 1, playing(&before), failed),
            None
        );
        let moved = vec![screen(0, 0, 0), screen(1, 3, 0)];
        assert_eq!(
            replacement_target(&moved, &moved, 1, playing(&before), failed),
            Some(1)
        );
    }

    #[test]
    fn vanished_screen_falls_back_to_the_first_real_one() {
        let gone = screen(2, 2, 1);
        let actual = vec![screen(0, 0, 0), screen(1, 1, 0)];
        assert_eq!(
            replacement_target(&actual, &actual, 2, playing(&gone), None),
            Some(0)
        );
        // Only the topology's transient list still has it: wait for the peer.
        let mut catalog = actual.clone();
        catalog.push(gone.clone());
        assert_eq!(
            replacement_target(&catalog, &actual, 2, playing(&gone), None),
            None
        );
    }

    #[test]
    fn topology_focus_switches_to_the_reported_screen() {
        let shown = screen(0, 0, 0);
        let actual = vec![shown.clone(), screen(5, 1, 1)];
        assert_eq!(
            replacement_target(&actual, &actual, 5, playing(&shown), None),
            Some(5)
        );
        assert_eq!(
            replacement_target(&actual, &actual, 0, playing(&shown), None),
            None
        );
    }

    /// A virtual screen added and selected in the same snapshot: the main
    /// window collects it before following the selection, instead of
    /// falling back to its first screen.
    #[test]
    fn a_new_selected_screen_is_collected_before_it_is_followed() {
        let actual = vec![screen(0, 0, 0), screen(1, 1, 0), screen(5, 2, 1)];
        let mut tabs = vec![0, 1];
        assert!(collect_new_screens(&mut tabs, &actual, &[]));
        assert_eq!(tabs, vec![0, 1, 5]);
        assert!(!collect_new_screens(&mut tabs, &actual, &[]));
        let window: Vec<RemoteScreen> = actual
            .iter()
            .filter(|screen| tabs.contains(&screen.id))
            .cloned()
            .collect();
        assert_eq!(
            replacement_target(&window, &actual, 5, playing(&actual[1]), None),
            Some(5)
        );
    }

    /// A screen another window holds is neither collected nor followed: the
    /// main window keeps what it shows.
    #[test]
    fn a_screen_in_another_window_stays_there() {
        let actual = vec![screen(0, 0, 0), screen(1, 1, 0), screen(2, 2, 0)];
        let mut tabs = vec![0, 1];
        assert!(!collect_new_screens(&mut tabs, &actual, &[2]));
        assert_eq!(tabs, vec![0, 1]);
        let window: Vec<RemoteScreen> = actual[..2].to_vec();
        // The selection is not in this window, so the check stays on the
        // displayed screen.
        assert_eq!(
            replacement_target(&window, &actual, 1, playing(&actual[1]), None),
            None
        );
    }

    #[test]
    fn a_tab_released_on_the_bar_is_not_detached() {
        let bar = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1280.0, 36.0));
        // A shaky click or a long press ends on the bar.
        assert!(!released_off_bar(Some(egui::pos2(300.0, 20.0)), bar));
        assert!(released_off_bar(Some(egui::pos2(300.0, 200.0)), bar));
        assert!(released_off_bar(Some(egui::pos2(-40.0, 20.0)), bar));
        // The pointer left the window during the drag.
        assert!(released_off_bar(None, bar));
    }

    #[test]
    fn a_detached_window_is_kept_on_the_drop_monitor() {
        use winit::dpi::PhysicalSize;
        let monitors = [
            (PhysicalPosition::new(0, 0), PhysicalSize::new(1920, 1080)),
            (
                PhysicalPosition::new(1920, -200),
                PhysicalSize::new(1280, 1024),
            ),
        ];
        let size = PhysicalSize::new(1280, 760);
        let place = |x: i32, y: i32| {
            constrain_to_monitor(
                PhysicalPosition::new(x - 100, y - 18),
                PhysicalPosition::new(x, y),
                size,
                36,
                &monitors,
            )
        };
        // Released inside the first monitor: kept where it was dropped.
        assert_eq!(place(400, 300), PhysicalPosition::new(300, 282));
        // At the top-left corner the title bar would leave the screen.
        assert_eq!(place(20, 5), PhysicalPosition::new(0, 0));
        // At the right edge the window stays within the monitor.
        assert_eq!(place(1900, 500), PhysicalPosition::new(640, 482));
        // The second monitor sits higher; its own top edge applies.
        assert_eq!(place(2000, -190), PhysicalPosition::new(1920, -200));
        // Dropped below every monitor: the nearest one takes it.
        assert_eq!(place(400, 1500), PhysicalPosition::new(300, 1044));
        // No monitor information: unchanged.
        assert_eq!(
            constrain_to_monitor(
                PhysicalPosition::new(-50, -50),
                PhysicalPosition::new(50, -32),
                size,
                36,
                &[],
            ),
            PhysicalPosition::new(-50, -50)
        );
    }

    #[test]
    fn opened_session_must_still_match_the_catalog() {
        let opened = screen(1, 1, 0);
        let (track, binding) = playing(&opened);
        assert!(screen_mapped(
            &[screen(0, 0, 0), opened.clone()],
            1,
            track,
            binding
        ));
        // Tracks were renumbered while the open was in flight.
        assert!(!screen_mapped(
            &[screen(0, 1, 0), screen(1, 0, 0)],
            1,
            track,
            binding
        ));
        // The screen is gone.
        assert!(!screen_mapped(&[screen(0, 0, 0)], 1, track, binding));
        // Same track, different screen type.
        assert!(!screen_mapped(&[screen(1, 1, 1)], 1, track, binding));
    }
}
