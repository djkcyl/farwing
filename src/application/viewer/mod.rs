use crate::diagnostics::performance::PerformanceMonitor;
use crate::features::stream_control::StreamControlHandle;
use crate::media::VideoCodec;
use crate::media::decoder::RenderSurface;
use crate::media::video_color::RenderColor;
use crate::transport::rtc::{
    EncodedVideoFrame, FrameSenderTiming, VideoFrameSink, VideoReceiverFeedback,
};
use anyhow::{Context, Result, anyhow, bail};
use decode_pipeline::{DecodeActivity, DecoderConfig, decoder_manager};
pub(super) use hud::{PerformancePanelMode, show_performance_overlay};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use stream_menu::{StreamControlUi, show_stream_control_window};
use tokio::sync::{mpsc, oneshot};

mod audio_view;
pub(crate) mod device_switch;
mod performance_alerts;
mod performance_panel;
pub(crate) use audio_view::AudioView;
mod screens;
mod stream_menu;

mod annotation;
mod display_transition;
mod fullscreen_chrome;
mod input_gate;
mod polling_warning;

#[cfg(windows)]
mod windows_cursor;

#[cfg(not(windows))]
mod linux_drag;
#[cfg(not(windows))]
pub(crate) mod linux_keyboard;
#[cfg(not(windows))]
mod linux_keyboard_grab;
#[cfg(not(windows))]
mod linux_shortcuts;
#[cfg(windows)]
mod windows_keyboard;

#[cfg(windows)]
mod windows_drag;
#[cfg(windows)]
mod windows_mouse;

#[cfg(windows)]
pub(crate) mod windows_presenter;
#[cfg(windows)]
pub(crate) use windows_presenter as presenter;
#[cfg(not(windows))]
pub(crate) mod linux_presenter;
#[cfg(not(windows))]
pub(crate) use linux_presenter as presenter;

/// Windows installs a low-level keyboard hook so system keys reach the remote
/// desktop. On Linux each player window holds the desktop's shortcuts back
/// itself (`linux_shortcuts`), so there is nothing process-wide to install.
pub(crate) struct DesktopInputHook {
    #[cfg(windows)]
    _hook: windows_keyboard::KeyboardHook,
}
#[cfg(windows)]
pub(crate) fn desktop_input_message(message: *const std::ffi::c_void) -> bool {
    windows_keyboard::message(message) || windows_mouse::router().message(message)
}
pub(crate) fn desktop_input_hook() -> Result<DesktopInputHook> {
    #[cfg(windows)]
    {
        windows_keyboard::remove_unused_raw_keyboard()?;
        windows_keyboard::KeyboardHook::install().map(|hook| DesktopInputHook { _hook: hook })
    }
    #[cfg(not(windows))]
    Ok(DesktopInputHook {})
}

#[cfg(windows)]
mod windows_ui;

const CONNECTION_PROGRESS_STEPS: u8 = 13;

#[derive(Clone, Debug)]
pub enum ConnectionProgressState {
    Working,
    Updating(Instant),
    Ready,
    Failed,
}

#[derive(Clone, Debug)]
pub struct ConnectionProgress {
    pub step: u8,
    pub title: String,
    pub detail: String,
    pub state: ConnectionProgressState,
    pub(crate) background: Option<crate::application::wallpaper::Source>,
}

#[derive(Clone, Default)]
pub(crate) struct ViewerDisplayHandle {
    pub surface_writer: Option<crate::platform::surface::D3D11SurfaceWriter>,
}

impl ConnectionProgress {
    pub(crate) fn updating(started: Instant) -> Self {
        Self {
            state: ConnectionProgressState::Updating(started),
            ..Self::working(1, "等待被控端更新完成", "正在等待更新完成并恢复画面")
        }
    }

    pub fn working(step: u8, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            step,
            title: title.into(),
            detail: detail.into(),
            state: ConnectionProgressState::Working,
            background: None,
        }
    }

    pub fn ready(detail: impl Into<String>) -> Self {
        Self {
            step: CONNECTION_PROGRESS_STEPS,
            title: "连接完成".to_owned(),
            detail: detail.into(),
            state: ConnectionProgressState::Ready,
            background: None,
        }
    }

    pub fn failed(detail: impl Into<String>) -> Self {
        Self {
            step: 0,
            title: "无法建立连接".to_owned(),
            detail: detail.into(),
            state: ConnectionProgressState::Failed,
            background: None,
        }
    }
}

impl ConnectionProgress {
    pub(crate) fn background(source: crate::application::wallpaper::Source) -> Self {
        Self {
            background: Some(source),
            ..Self::working(0, "", "")
        }
    }
}

fn configure_viewer_visuals(ctx: &egui::Context) {
    crate::ui::theme::configure(ctx);
}

pub(crate) fn run_connecting_viewer_window(
    alias: String,
    progress: std_mpsc::Receiver<ConnectionProgress>,
    session: std_mpsc::Receiver<ViewerWindowEvent>,
    display_sender: oneshot::Sender<ViewerDisplayHandle>,
) -> Result<()> {
    {
        presenter::run_connecting(presenter::ConnectingWindowsRunConfig {
            alias,
            progress,
            session,
            display_sender,
        })
    }
}

pub(crate) enum ViewerWindowEvent {
    Close,
    Listening(AudioView),
    Playing(Box<NativeViewerSession>),
    Reconnect {
        alias: String,
        window: Option<winit::window::WindowId>,
        progress: std_mpsc::Receiver<ConnectionProgress>,
        display: oneshot::Sender<ViewerDisplayHandle>,
    },
}

pub(super) struct ConnectionProgressApp {
    alias: String,
    receiver: std_mpsc::Receiver<ConnectionProgress>,
    details_open: bool,
    events: Vec<(Duration, ConnectionProgress)>,
    current: ConnectionProgress,
    started_at: Instant,
    background: Option<crate::application::wallpaper::Source>,
    wallpapers: crate::application::wallpaper::Wallpapers,
    update_started: Option<Instant>,
}

impl ConnectionProgressApp {
    pub(super) fn new(alias: String, receiver: std_mpsc::Receiver<ConnectionProgress>) -> Self {
        let current = ConnectionProgress::working(1, "准备连接", "正在读取本地会话和设备配置");
        Self {
            alias,
            receiver,
            details_open: false,
            events: vec![(Duration::ZERO, current.clone())],
            current,
            started_at: Instant::now(),
            background: None,
            wallpapers: Default::default(),
            update_started: None,
        }
    }

    pub(super) fn drain(&mut self) {
        while let Ok(mut progress) = self.receiver.try_recv() {
            match progress.state {
                ConnectionProgressState::Updating(started) => self.update_started = Some(started),
                ConnectionProgressState::Failed => self.update_started = None,
                _ => {}
            }
            if let Some(source) = progress.background.take() {
                if self.background.as_ref() != Some(&source) {
                    self.wallpapers.clear();
                    self.background = Some(source);
                }
                continue;
            }
            if progress.step == 0 {
                progress.step = self.current.step;
            }
            self.events
                .push((self.started_at.elapsed(), progress.clone()));
            if matches!(progress.state, ConnectionProgressState::Ready) {
                tracing::debug!("connection UI reached ready state");
            }
            self.current = progress;
        }
    }
}

mod connection_progress;

#[derive(Debug)]
struct DecodedVideoFrame {
    is_new_picture: Option<bool>,
    width: u32,
    height: u32,
    surface: RenderSurface,
    color: RenderColor,
    received_at: Instant,
    decoded_at: Instant,
    assembly_delay: Duration,
    input_queue_delay: Duration,
    decode_pipeline_delay: Duration,
    rotation: u16,
    sender_timing: FrameSenderTiming,
}

const OFFICIAL_DECODER_INFLIGHT_LIMIT: usize = 100;

// Presentation owns at most one pending decoded picture. Replacing that
// picture releases its display lease; the decoder retains its own references.
type FrameQueue = Arc<Mutex<Option<DecodedVideoFrame>>>;

/// The presentation clock must not throttle decoding of reference pictures.
fn take_next_frame(
    queue: &mut Option<DecodedVideoFrame>,
    performance: &PerformanceMonitor,
) -> Option<DecodedVideoFrame> {
    let frame = queue.take();
    performance.set_presentation_queue_frames(0);
    frame
}

#[derive(Clone, Default)]
struct FrameWake {
    visible: Arc<AtomicBool>,

    /// 同一路解码器可能同时被多个窗口持有：屏被拖出成独立窗口之后，原窗口
    /// 还没接上新屏的那一小段时间里，两边都看着同一个会话。
    render_threads: Arc<Mutex<Vec<std::thread::Thread>>>,
}

impl FrameWake {
    fn install_render_thread(&self, thread: std::thread::Thread) {
        let mut threads = mutex_lock(&self.render_threads);
        if !threads.iter().any(|known| known.id() == thread.id()) {
            threads.push(thread);
        }
        drop(threads);
        self.visible.store(true, Ordering::Release);
    }

    /// 撤销这一个窗口的登记。最后一个窗口走了才把 `visible` 置回去，
    /// 否则别的窗口就再也收不到解码帧了。
    fn remove_render_thread(&self, thread: &std::thread::Thread) {
        let mut threads = mutex_lock(&self.render_threads);
        threads.retain(|known| known.id() != thread.id());
        if threads.is_empty() {
            self.visible.store(false, Ordering::Release);
        }
    }

    fn notify(&self) {
        for thread in mutex_lock(&self.render_threads).iter() {
            thread.unpark();
        }
    }
}

pub struct NativeViewerSession {
    // A negotiated track may be reassigned after a display topology change.
    screen_binding: std::sync::atomic::AtomicU64,
    track_index: i32,
    screens: Option<Box<screens::ScreenPlayback>>,
    title: String,
    video_sink: Option<mpsc::UnboundedSender<EncodedVideoFrame>>,
    frame_queue: FrameQueue,
    frame_wake: FrameWake,
    startup_receiver: Option<oneshot::Receiver<Result<(), String>>>,
    performance: PerformanceMonitor,
    stream_control: StreamControlHandle,
    shutdown: Arc<AtomicBool>,
    manager_thread: Option<JoinHandle<()>>,
    manager_wake: std::thread::Thread,
    fatal_error: Arc<Mutex<Option<String>>>,
    decode_activity: Box<DecodeActivity>,
}

#[derive(Clone)]
pub struct ViewerCloseHandle {
    shutdown: Arc<AtomicBool>,
    frame_wake: FrameWake,
    manager_wake: std::thread::Thread,
}

impl ViewerCloseHandle {
    pub fn close(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.manager_wake.unpark();
        self.frame_wake.notify();
    }
}

pub(crate) struct ViewerLaunchConfig {
    pub codec: VideoCodec,
    pub decoder: crate::media::selection::DecoderPreference,
    pub title: String,
    // Backend allocation hint only. The bitstream supplies actual dimensions.
    pub initial_width: u32,
    pub initial_height: u32,
    pub frame_rate: u32,
    pub receiver_feedback: mpsc::UnboundedSender<VideoReceiverFeedback>,
    pub performance: PerformanceMonitor,
    pub stream_control: StreamControlHandle,
    pub display: ViewerDisplayHandle,
}

impl NativeViewerSession {
    pub(crate) fn set_device_switch(&mut self, switcher: device_switch::DeviceSwitcher) {
        if let Some(screens) = &mut self.screens {
            screens.device_switch = Some(switcher);
        }
    }
    pub(crate) async fn launch(config: ViewerLaunchConfig) -> Result<Self> {
        let ViewerLaunchConfig {
            codec,
            decoder,
            title,
            initial_width,
            initial_height,
            frame_rate,
            receiver_feedback,
            performance,
            stream_control,
            display,
        } = config;
        let (video_sink, video_source) = mpsc::unbounded_channel();
        let frame_queue = Arc::new(Mutex::new(None));
        let frame_wake = FrameWake::default();
        let shutdown = Arc::new(AtomicBool::new(false));
        let fatal_error = Arc::new(Mutex::new(None));
        let (startup_sender, startup_receiver) = oneshot::channel();
        let software_decode = Arc::new(AtomicBool::new(!decoder.mode.hardware()));
        let decode_enabled = Arc::new(AtomicBool::new(true));
        let (decode_idle_sender, decode_idle) = tokio::sync::watch::channel(0);
        let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let manager_pause_epoch = Arc::clone(&pause_epoch);
        let manager_software = Arc::clone(&software_decode);
        let manager_enabled = Arc::clone(&decode_enabled);
        let manager_shutdown = Arc::clone(&shutdown);
        let manager_fatal_error = Arc::clone(&fatal_error);
        let manager_performance = performance.clone();
        let manager_frame_queue = Arc::clone(&frame_queue);
        let manager_frame_wake = frame_wake.clone();
        let manager_thread = std::thread::spawn(move || {
            decoder_manager(
                DecoderConfig {
                    codec,
                    width: initial_width,
                    height: initial_height,
                    frame_rate,
                    decoder,
                    software_decode: manager_software,
                    decode_enabled: manager_enabled,
                    decode_idle: decode_idle_sender,
                    pause_epoch: manager_pause_epoch,

                    surface_writer: display.surface_writer,
                },
                video_source,
                manager_frame_queue,
                manager_frame_wake,
                manager_performance,
                manager_shutdown,
                manager_fatal_error,
                receiver_feedback,
                Some(startup_sender),
            );
        });
        let manager_wake = manager_thread.thread().clone();
        // Establish the owner before awaiting initialization. Dropping this
        // future (close/account cancellation) must also stop and join its worker.
        let session = Self {
            screen_binding: std::sync::atomic::AtomicU64::new(0),
            track_index: 0,
            screens: None,
            title,
            video_sink: Some(video_sink),
            frame_queue,
            frame_wake,
            performance,
            stream_control,
            shutdown,
            manager_thread: Some(manager_thread),
            manager_wake,
            fatal_error,
            startup_receiver: Some(startup_receiver),
            decode_activity: Box::new(DecodeActivity {
                software: software_decode,
                enabled: decode_enabled,
                pause_epoch,
                idle: decode_idle,
            }),
        };
        Ok(session)
    }

    /// Wait until the decoder opened against the first frame's parameter sets.
    pub(crate) async fn startup(&mut self) -> Result<()> {
        self.startup_receiver
            .take()
            .context("decoder startup already awaited")?
            .await
            .map_err(|error| anyhow!("native decoder startup ended: {error}"))?
            .map_err(|error| anyhow!("start native platform decoder: {error}"))
    }

    pub(crate) fn is_software(&self) -> bool {
        self.decode_activity.software.load(Ordering::Acquire)
    }

    pub(crate) async fn pause_software(&self) -> Result<()> {
        if !self.is_software() {
            return Ok(());
        }
        let mut idle = self.decode_activity.idle.clone();
        let epoch = self
            .decode_activity
            .pause_epoch
            .fetch_add(1, Ordering::AcqRel)
            + 1;
        self.decode_activity.enabled.store(false, Ordering::Release);
        self.manager_wake.unpark();
        // An acknowledgement of an earlier pause cannot satisfy a new pause
        // issued immediately after resume, before the worker has run again.
        idle.wait_for(|idle| *idle >= epoch)
            .await
            .context("软件解码器未能停止")?;
        Ok(())
    }

    pub(crate) fn resume_decode(&self) -> bool {
        if self.shutdown.load(Ordering::Acquire) {
            return false;
        }
        let paused = !self.decode_activity.enabled.swap(true, Ordering::AcqRel);
        if paused {
            self.manager_wake.unpark();
        }
        paused && !self.shutdown.load(Ordering::Acquire)
    }
    pub(crate) fn decode_paused(&self) -> bool {
        !self.decode_activity.enabled.load(Ordering::Acquire)
    }

    pub(crate) fn video_sink(&self) -> VideoFrameSink {
        VideoFrameSink::Unbounded {
            sender: self
                .video_sink
                .as_ref()
                .expect("native viewer video sink must exist while the session is alive")
                .clone(),
            wake: self.manager_wake.clone(),
        }
    }

    pub(crate) fn attach_screen_playback(
        &mut self,
        peer: &Arc<crate::transport::rtc::NativePeer>,
        profile: crate::media::ConnectionMediaProfile,
        alias: &str,
        track: i32,
    ) {
        self.track_index = track;
        if let Some(screen) = self
            .stream_control
            .snapshot()
            .screens
            .iter()
            .find(|screen| screen.video_track_index == track)
        {
            self.bind_screen(screen);
        }
        self.screens = Some(Box::new(screens::ScreenPlayback::new(
            peer,
            profile,
            alias,
            self.screen_id(),
        )));
    }

    pub(crate) fn screen_id(&self) -> i32 {
        self.screen_binding.load(Ordering::Acquire) as u32 as i32
    }

    pub(crate) fn screen_binding(&self) -> u64 {
        self.screen_binding.load(Ordering::Acquire)
    }

    pub(crate) fn bind_screen(
        &self,
        screen: &crate::features::stream_control::RemoteScreen,
    ) -> bool {
        let binding =
            (screen.id as u32 as u64) | ((screen.display.screen_type as u32 as u64) << 32);
        let changed = self.screen_binding.swap(binding, Ordering::AcqRel) != binding;
        if changed {
            mutex_lock(&self.frame_queue).take();
            self.manager_wake.unpark();
        }
        changed
    }

    pub fn close_handle(&self) -> ViewerCloseHandle {
        ViewerCloseHandle {
            shutdown: Arc::clone(&self.shutdown),
            frame_wake: self.frame_wake.clone(),
            manager_wake: self.manager_wake.clone(),
        }
    }

    pub(crate) fn take_screen_playback(&mut self) -> Option<screens::ScreenPlayback> {
        self.screens.take().map(|owner| *owner)
    }

    pub fn ensure_running(&self) -> Result<()> {
        if let Some(error) = mutex_lock(&self.fatal_error).clone() {
            bail!("native viewer decoder stopped: {error}");
        }
        if self
            .manager_thread
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            bail!("native viewer decoder stopped unexpectedly");
        }
        Ok(())
    }

    pub fn run(self) -> Result<()> {
        presenter::run(self)
    }
}

impl Drop for NativeViewerSession {
    fn drop(&mut self) {
        let stats = self.performance.snapshot();
        tracing::info!(
            rendered_frames = stats.total_rendered_frames,
            key_frames_decoded = stats.total_key_frames_decoded,
            received_fps = format_args!("{:.1}", stats.receive_fps),
            decoded_fps = format_args!("{:.1}", stats.decode_fps),
            rendered_fps = format_args!("{:.1}", stats.render_fps),
            actual_fps = format_args!("{:.1}", stats.actual_fps),
            actual_frames = stats.total_actual_rendered_frames,
            marked_frames = stats.total_marked_rendered_frames,
            packet_loss_percent = format_args!("{:.2}", stats.packet_loss_percent),
            low_latency_playout = stats.low_latency_playout,
            local_average_ms = format_args!("{:.1}", stats.local_frame_delay_average_ms),
            local_p95_ms = format_args!("{:.1}", stats.local_frame_delay_p95_ms),
            local_max_ms = format_args!("{:.1}", stats.local_frame_delay_max_ms),
            target_playout_delay_ms = format_args!("{:.1}", stats.target_playout_delay_ms),
            jitter_playout_delay_ms = format_args!("{:.1}", stats.jitter_playout_delay_ms),
            source_interval_p95_ms = format_args!("{:.1}", stats.source_cadence.p95_ms),
            receive_interval_p95_ms = format_args!("{:.1}", stats.receive_cadence.p95_ms),
            decode_interval_p95_ms = format_args!("{:.1}", stats.decode_cadence.p95_ms),
            render_interval_p95_ms = format_args!("{:.1}", stats.render_cadence.p95_ms),
            render_interval_max_ms = format_args!("{:.1}", stats.render_cadence.max_ms),
            render_queue_delay_ms = format_args!("{:.1}", stats.render_queue_delay_ms),
            presentation_queue_frames = stats.presentation_queue_frames,
            presentation_queue_peak_frames = stats.presentation_queue_peak_frames,
            frame_buffer_peak_frames = stats.frame_buffer_peak_frames,
            rtx_packets_received = stats.rtx_packets_received,
            rtx_packets_accepted = stats.rtx_packets_accepted,
            fec_packets_received = stats.fec_packets_received,
            fec_packets_recovered = stats.fec_packets_recovered,
            outstanding_nacks = stats.outstanding_nacks,
            predecode_dropped_frames = stats.predecode_dropped_frames,
            ingress_queue_peak_packets = stats.ingress_queue_peak_packets,
            small_jank_count = stats.small_jank_count,
            jank_count = stats.jank_count,
            big_jank_count = stats.big_jank_count,
            dropped_present_frames = stats.dropped_present_frames,
            "native viewer session performance summary"
        );
        self.shutdown.store(true, Ordering::Release);
        self.video_sink.take();
        self.manager_wake.unpark();
        self.frame_wake.notify();
        // Return queued GPU samples before joining: the closing window will
        // no longer consume them. Never hold the queue mutex across join.
        mutex_lock(&self.frame_queue).take();
        if let Some(thread) = self.manager_thread.take() {
            let started = Instant::now();
            if thread.join().is_err() {
                tracing::error!("native decoder worker panicked during its owned lifetime");
            }
            tracing::debug!(
                elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
                "native decoder worker joined"
            );
        }
        // A poll already in progress when shutdown was set can publish its last
        // completed output. The joined worker cannot add any further surfaces.
        mutex_lock(&self.frame_queue).take();
    }
}

const COMPACT_HUD_WIDTH: f32 = 120.0;
const COMPACT_METER_WIDTH: f32 = 37.0;
const COMPACT_COLUMN_GAP: f32 = 8.0;

fn mutex_lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

mod decode_pipeline;
mod hud;
