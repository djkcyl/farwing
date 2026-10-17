//! Render-thread ownership, commands, frame delivery and shutdown.
use super::plugin_video;
use super::renderer::D3D11Presenter;
use super::swapchain::is_device_lost;
use crate::application::viewer::{
    DecodedVideoFrame, NativeViewerSession, mutex_lock, take_next_frame,
};
use crate::diagnostics::performance::{PerformanceMonitor, RenderedFrameTiming};
use crate::media::decoder::RenderSurface;
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
};
use winit::dpi::PhysicalSize;

pub(in crate::application::viewer) enum RenderCommand {
    DisplayAspect(Option<(u32, u32)>),
    Resize(PhysicalSize<u32>),
    Occluded(bool),
    Stop,
}

pub(in crate::application::viewer) struct RenderWorker {
    pub(in crate::application::viewer) plugins: crate::plugins::Controller,
    pub(in crate::application::viewer) frame_wake: crate::application::viewer::FrameWake,
    pub(in crate::application::viewer) commands: std_mpsc::Sender<RenderCommand>,
    pub(in crate::application::viewer) wake: std::thread::Thread,
    pub(in crate::application::viewer) thread: Option<std::thread::JoinHandle<()>>,
    pub(in crate::application::viewer) current_video_size: Arc<Mutex<Option<(u32, u32, u16)>>>,
    pub(in crate::application::viewer) first_presented: Arc<AtomicBool>,
}

impl RenderWorker {
    pub(in crate::application::viewer) fn spawn(
        hwnd: isize,
        mut size: PhysicalSize<u32>,
        session: &NativeViewerSession,
        context: &egui::Context,
        cpu_device: Arc<Mutex<Option<(ID3D11Device, ID3D11DeviceContext)>>>,
    ) -> Result<Self> {
        let started = Instant::now();
        let screen_id = session.screen_id();
        let stream_control = session.stream_control.clone();
        let initial_display_aspect = stream_control.display_aspect(session.track_index);
        let plugins = crate::plugins::Controller::new(context.clone());
        let plugin_state = plugins.shared.clone();
        let frame_queue = Arc::clone(&session.frame_queue);
        let performance = session.performance.clone();
        let worker_shutdown = Arc::clone(&session.shutdown);
        let (commands, command_receiver) = std_mpsc::channel();
        let current_video_size = Arc::new(Mutex::new(None));
        let worker_video_size = Arc::clone(&current_video_size);
        let first_presented = Arc::new(AtomicBool::new(false));
        let worker_first_presented = Arc::clone(&first_presented);
        let first_frame_repaint = context.clone();
        let thread = std::thread::Builder::new()
            .name("Video Render".to_owned())
            .spawn(move || {
                if let Err(error) =
                    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) }
                {
                    tracing::warn!(%error, "set video render thread priority");
                }
                let mut first_present = true;
                let mut presenter = None::<D3D11Presenter>;
                let mut current_frame = None::<DecodedVideoFrame>;
                let mut redraw = false;
                let mut occluded = false;
                let mut display_aspect = initial_display_aspect;
                'render: loop {
                    while let Ok(command) = command_receiver.try_recv() {
                        match command {
                            RenderCommand::DisplayAspect(value) => {
                                display_aspect = value;
                                redraw = true;
                            }
                            RenderCommand::Resize(new_size) => {
                                size = new_size;
                                redraw = true;
                            }
                            RenderCommand::Occluded(value) => {
                                occluded = value;
                                redraw = !value;
                                performance.pause_presentation();
                            }
                            RenderCommand::Stop => break 'render,
                        }
                    }
                    if worker_shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    if occluded || size.width == 0 || size.height == 0 {
                        current_frame = None;
                        if let Some(p) = presenter.as_mut() {
                            if let Some(effects) = p.effects.as_mut() {
                                effects.clear_history();
                            }
                            p.captures = plugin_video::Captures::default();
                        }
                        let dropped = {
                            let mut queue = mutex_lock(&frame_queue);
                            let count = usize::from(queue.take().is_some());
                            performance.set_presentation_queue_frames(0);
                            count
                        };
                        for _ in 0..dropped {
                            performance.record_dropped_present_frame();
                        }
                        // Continue draining on decoded-frame notifications while minimized.
                        std::thread::park();
                        continue;
                    }
                    let chain_changed = presenter.as_ref().is_some_and(|p| {
                        p.effect_revision != plugin_state.revision.load(Ordering::Acquire)
                    });
                    redraw |= chain_changed;
                    if let Some(p) = presenter.as_mut() {
                        match p.resize(size).and_then(|()| p.ready()) {
                            Ok(true) => {},
                            Ok(false) => {
                                // Leave the newest-frame mailbox replaceable while
                                // waiting, and service Stop/resize on every turn.
                                continue;
                            }
                            Err(error) => {
                                tracing::warn!(%error, "video presentation readiness failed");
                                presenter.take();
                                current_frame = None;
                            }
                        }
                    }
                    let replacement = take_next_frame(&mut mutex_lock(&frame_queue), &performance);
                    let is_new_submission = replacement.is_some();
                    if (redraw || is_new_submission)
                        && let Some(frame) = replacement.as_ref().or(current_frame.as_ref())
                    {
                        // Shared handles cross devices on one GPU, not arbitrary GPUs.
                        // Follow the decoder adapter; adopt non-shared resources directly.
                        // Release the old swap chain before constructing its replacement.
                        let changed = presenter.as_ref().is_some_and(|p| {
                            matches!(&frame.surface, RenderSurface::D3D11(surface)
                            if !p.can_render_surface(surface))
                        });
                        if changed {
                            presenter.take();
                        }
                        if presenter.is_none() {
                            match D3D11Presenter::for_frame(hwnd, size, frame, &cpu_device) {
                                Ok(p) => {
                                    presenter = Some(p);
                                    if let Some(frame) = replacement {
                                        let mut queue = mutex_lock(&frame_queue);
                                        if queue.is_none() {
                                            *queue = Some(frame);
                                            performance.set_presentation_queue_frames(1);
                                        } else {
                                            performance.record_dropped_present_frame();
                                        }
                                    }
                                    redraw = true;
                                    // First creation also crosses the readiness gate
                                    // before choosing the latest available picture.
                                    continue;
                                }
                                Err(error) => {
                                    tracing::warn!(%error, "initialize native video presentation pipeline");
                                    if is_new_submission { performance.record_dropped_present_frame(); }
                                    current_frame = None;
                                    std::thread::park_timeout(Duration::from_millis(10));
                                    continue;
                                }
                            }
                        }
                        let result = (|| {
                            let p = presenter.as_mut().expect("presenter initialized");
                            p.plugin_state = Some(plugin_state.clone());
                            if p.effect_revision != plugin_state.revision.load(Ordering::Acquire) {
                                let fps = performance.snapshot().receive_fps;
                                p.nominal_fps = if fps >= 15.0 { fps } else { 60.0 };
                            }
                            p.video_renderer.set_display_aspect(display_aspect);
                            p.analysis_at = frame.received_at;
                            p.analysis_new = is_new_submission || chain_changed;
                            p.resize(size).map_err(VideoRenderError::prepare)?;
                            render_thread_frame(p, frame, &performance, is_new_submission)
                        })();
                        match result {
                            Ok(()) => {
                                if is_new_submission && presenter.as_ref().is_some_and(|p| p.drew) {
                                    stream_control
                                        .topology_frame_presented(screen_id, frame.received_at);
                                }
                                if first_present && presenter.as_ref().is_some_and(|p| p.drew) {
                                    first_present = false;
                                    worker_first_presented.store(true, Ordering::Release);
                                    first_frame_repaint.request_repaint();
                                    tracing::debug!(
                                        screen_id,
                                        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
                                        "screen renderer first frame presented"
                                    );
                                }
                                if let Some(frame) = replacement {
                                    *mutex_lock(&worker_video_size) =
                                        Some((frame.width, frame.height, frame.rotation));
                                    current_frame = Some(frame);
                                }
                            }
                            Err(VideoRenderError {
                                error,
                                release_resources,
                            }) => {
                                tracing::warn!(error = %format!("{error:#}"), release_resources,
                                "D3D11 video frame was not presented");
                                if is_new_submission {
                                    performance.record_dropped_present_frame();
                                }
                                if release_resources {
                                    // Official errors release the output/allocator, then return.
                                    // The next frame reinitializes; no same-frame retry or session abort.
                                    presenter.take();
                                    current_frame = None;
                                }
                            }
                        }
                    }
                    redraw = false;
                    if mutex_lock(&frame_queue).is_none() {
                        if let Some(due) = presenter
                            .as_ref()
                            .and_then(|p| p.effects.as_ref())
                            .and_then(|e| e.next_due())
                        {
                            std::thread::park_timeout(
                                due.saturating_duration_since(Instant::now()),
                            );
                            redraw = true;
                        } else {
                            std::thread::park();
                        }
                    }
                }
                // GPU resources and retained samples go away before the child HWND.
                drop(presenter);
                drop(current_frame);
                if let Some((_, context)) = mutex_lock(&cpu_device).as_ref() {
                    // Release bindings held by the retained context before another track uses it.
                    unsafe {
                        context.ClearState();
                    }
                }
                mutex_lock(&frame_queue).take();
                performance.set_presentation_queue_frames(0);
            })
            .context("create Video Render thread")?;
        let wake = thread.thread().clone();
        *mutex_lock(&plugins.shared.wake) = Some(wake.clone());
        session.frame_wake.install_render_thread(wake.clone());
        wake.unpark();
        Ok(Self {
            plugins,
            frame_wake: session.frame_wake.clone(),
            commands,
            wake,
            thread: Some(thread),
            current_video_size,
            first_presented,
        })
    }

    pub(in crate::application::viewer) fn send(&self, command: RenderCommand) {
        if self.commands.send(command).is_ok() {
            self.wake.unpark();
        }
    }
}

pub(in crate::application::viewer) struct VideoRenderError {
    pub(in crate::application::viewer) error: anyhow::Error,
    pub(in crate::application::viewer) release_resources: bool,
}

impl VideoRenderError {
    pub(in crate::application::viewer) fn prepare(error: anyhow::Error) -> Self {
        Self {
            error,
            release_resources: true,
        }
    }
}

impl RenderWorker {
    pub(in crate::application::viewer) fn stop(&mut self) {
        if self.thread.is_none() {
            return;
        }
        self.frame_wake.remove_render_thread(&self.wake);
        let _ = self.commands.send(RenderCommand::Stop);
        self.wake.unpark();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for RenderWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(in crate::application::viewer) fn render_thread_frame(
    presenter: &mut D3D11Presenter,
    frame: &DecodedVideoFrame,
    performance: &PerformanceMonitor,
    is_new_submission: bool,
) -> std::result::Result<(), VideoRenderError> {
    presenter.submission_wait = Duration::ZERO;
    let wait = Duration::ZERO;
    let started = Instant::now();
    presenter.drew = false;
    presenter.effect_output = None;
    presenter.effect_generated = false;
    presenter.effect_input = is_new_submission.then_some(plugin_video::Metadata {
        received_at: frame.received_at,
        timing: RenderedFrameTiming {
            is_new_picture: frame.is_new_picture,
            width: frame.width,
            height: frame.height,
            decoded_at: frame.decoded_at,
            local: Duration::ZERO,
            assembly: frame.assembly_delay,
            input_queue: frame.input_queue_delay,
            decode_pipeline: frame.decode_pipeline_delay,
            surface_transfer: Duration::ZERO,
            present_wait: wait,
            render_queue: frame.decoded_at.elapsed(),
            sender_capture_at: frame.sender_timing.capture_at,
            sender_capture: frame.sender_timing.capture_delay,
            sender_encode: frame.sender_timing.encode_delay,
            sender_pacer: frame.sender_timing.pacer_delay,
            sender_total: frame.sender_timing.sending_delay,
            transport: frame.sender_timing.transport_delay,
        },
    });
    let result = if !presenter.analysis_new && presenter.effects.is_some() {
        presenter.draw_effect_output(frame.color, true)
    } else {
        match &frame.surface {
            RenderSurface::D3D11(surface) => presenter.draw_video(
                surface,
                frame.width,
                frame.height,
                frame.rotation,
                frame.color,
            ),
            RenderSurface::CpuRgba8(pixels) => {
                presenter.draw_cpu_video(pixels, frame.width, frame.height, frame.rotation)
            }
        }
    };
    result.map_err(VideoRenderError::prepare)?;
    if !presenter.drew {
        presenter.release_active_input_sync();
        return Ok(());
    }
    let wait = presenter.submission_wait;
    let transfer = started.elapsed().saturating_sub(wait);
    presenter.present().map_err(|error| VideoRenderError {
        release_resources: is_device_lost(&error),
        error,
    })?;
    if presenter.effect_generated {
        if let Some(state) = &presenter.plugin_state {
            state.generated.fetch_add(1, Ordering::Relaxed);
        }
    } else if let Some(mut metadata) = presenter.effect_output {
        metadata.timing.local = metadata.received_at.elapsed();
        metadata.timing.surface_transfer = transfer;
        metadata.timing.present_wait = wait;
        metadata.timing.render_queue =
            started.saturating_duration_since(metadata.timing.decoded_at);
        performance.record_rendered_frame(metadata.timing);
    }
    Ok(())
}
