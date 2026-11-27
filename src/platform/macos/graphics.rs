//! wgpu (Metal) presentation for the shared egui shell on macOS.
//!
//! Mirrors the Linux presenter: one surface per window, egui output deferred
//! so a busy presentation never blocks the event loop.
use anyhow::{Context, Result};
use std::sync::Arc;
use winit::dpi::PhysicalSize;
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

/// Where the video layer lands in the window, which the player decides.
pub(crate) use super::video_layer::VideoPlacement;
use crate::ui::gfx::nonzero_size;

/// What a render call did with the window, as the D3D11 presenter reports
/// it. A wgpu surface has no skipped or visibility-deferred frames: it either
/// presents or keeps the output for the next repaint.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiFrame {
    Presented,
    Pending,
}
impl UiFrame {
    pub fn ready(self) -> bool {
        self == Self::Presented
    }
    pub fn presented(self) -> bool {
        self == Self::Presented
    }
}

/// egui drawing output, separated from the platform half of `FullOutput`.
pub(crate) struct RendererOutput {
    pub(crate) textures_delta: egui::TexturesDelta,
    pub(crate) shapes: Vec<egui::epaint::ClippedShape>,
    pub(crate) pixels_per_point: f32,
}

/// Split a [`egui::FullOutput`] into the drawing part and the platform parts,
/// matching `egui_directx11::split_output` so the shell stays platform-neutral.
pub(crate) fn split_output(
    full_output: egui::FullOutput,
) -> (
    RendererOutput,
    egui::PlatformOutput,
    egui::OrderedViewportIdMap<egui::ViewportOutput>,
) {
    (
        RendererOutput {
            textures_delta: full_output.textures_delta,
            shapes: full_output.shapes,
            pixels_per_point: full_output.pixels_per_point,
        },
        full_output.platform_output,
        full_output.viewport_output,
    )
}

/// The process-wide wgpu device shared by every shell window.
#[derive(Clone)]
pub(crate) struct Graphics {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    label: String,
}

impl Graphics {
    pub(crate) fn label(&self) -> &str {
        &self.label
    }
}

/// One adapter and device for the whole process: windows open and close, but
/// re-creating a Metal device per window would stall each one on startup.
pub(crate) fn create_device(event_loop: &ActiveEventLoop, window: &Window) -> Result<Graphics> {
    static SHARED: std::sync::OnceLock<std::result::Result<Graphics, String>> =
        std::sync::OnceLock::new();
    SHARED
        .get_or_init(|| create_graphics(event_loop, window).map_err(|error| format!("{error:#}")))
        .clone()
        .map_err(anyhow::Error::msg)
}

fn create_graphics(event_loop: &ActiveEventLoop, window: &Window) -> Result<Graphics> {
    // Metal is the only backend this build links; the default instance
    // descriptor already carries sane validation flags.
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
    desc.backends = wgpu::Backends::METAL;
    let instance = wgpu::Instance::new(desc);
    let probe = instance
        .create_surface(window)
        .context("创建窗口绘制表面失败")?;
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&probe),
        ..Default::default()
    }))
    .context("没有能在这个窗口上绘制的 GPU 适配器（需要 Metal）")?;
    drop(probe);
    let info = adapter.get_info();
    tracing::info!(
        adapter = %info.name,
        backend = ?info.backend,
        device_type = ?info.device_type,
        driver = %info.driver,
        "UI GPU adapter selected"
    );
    let required_features = wgpu::Features::empty();
    let (device, queue) = pollster::block_on(
        adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("openuuyc-ui"),
            required_features,
            // The UI stays within the baseline limits; video upload does too.
            required_limits: wgpu::Limits::downlevel_defaults()
                .using_resolution(adapter.limits())
                .using_alignment(adapter.limits()),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            trace: wgpu::Trace::Off,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
        }),
    )
    .context("创建 GPU 设备失败")?;
    let label = format!("{} · {:?}", info.name, info.backend);
    Ok(Graphics {
        instance,
        adapter,
        device,
        queue,
        label,
    })
}

pub(crate) struct UiPresenter {
    /// `None` 只在主窗口收进托盘期间出现：窗口要销毁，surface 得跟着放掉（它握着
    /// 窗口的一个 `Arc`），但 egui 的纹理、管线与没画完的那一帧都留在 presenter 里，
    /// 等托盘再打开的窗口接上继续用。
    surface: Option<wgpu::Surface<'static>>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    renderer: egui_wgpu::Renderer,
    video: super::video_layer::VideoLayer,
    /// Where the next frame goes; `None` leaves the video layer unpainted.
    placement: Option<super::video_layer::VideoPlacement>,
    /// The YUV to RGB rows for an NV12 frame; `None` when the frame is RGBA.
    transform: Option<[[f32; 4]; 3]>,
    pending_output: Option<RendererOutput>,
    size: PhysicalSize<u32>,
    configured: bool,
}

impl UiPresenter {
    pub(crate) fn new(window: Arc<Window>, graphics: &Graphics) -> Result<Self> {
        let size = nonzero_size(window.inner_size());
        let (surface, config) = create_surface(&window, graphics)?;
        let renderer = egui_wgpu::Renderer::new(
            &graphics.device,
            config.format,
            egui_wgpu::RendererOptions {
                msaa_samples: 1,
                depth_stencil_format: None,
                dithering: true,
                ..Default::default()
            },
        );
        let video = super::video_layer::VideoLayer::new(&graphics.device, config.format);
        Ok(Self {
            surface: Some(surface),
            device: graphics.device.clone(),
            queue: graphics.queue.clone(),
            config,
            renderer,
            video,
            placement: None,
            transform: None,
            pending_output: None,
            size,
            configured: true,
        })
    }

    /// 窗口要销毁了（主窗口收进托盘）：放掉 surface，绘制资源留下。
    pub(crate) fn detach(&mut self) {
        self.surface = None;
        self.configured = false;
    }

    /// 托盘再打开时的窗口接上：同一个设备、同一套表面格式，绘制资源原样用。
    pub(crate) fn attach(&mut self, window: Arc<Window>, graphics: &Graphics) -> Result<()> {
        let (surface, config) = create_surface(&window, graphics)?;
        anyhow::ensure!(
            config.format == self.config.format,
            "新窗口的绘制表面格式与保留的绘制资源不一致"
        );
        self.config = config;
        self.size = nonzero_size(window.inner_size());
        self.surface = Some(surface);
        self.configured = true;
        Ok(())
    }

    /// Replace the video frame drawn under the UI with packed RGBA8 pixels.
    pub(crate) fn upload_video(&mut self, width: u32, height: u32, pixels: &[u8]) -> Result<()> {
        self.transform = None;
        self.video
            .upload_rgba(&self.device, &self.queue, width, height, pixels)
    }

    /// Replace it with packed NV12, converted to RGB by the fragment shader.
    pub(crate) fn upload_video_nv12(
        &mut self,
        width: u32,
        height: u32,
        data: &[u8],
        transform: [[f32; 4]; 3],
    ) -> Result<()> {
        self.transform = Some(transform);
        self.video
            .upload_nv12(&self.device, &self.queue, width, height, data)
    }

    pub(crate) fn set_video_placement(
        &mut self,
        placement: Option<super::video_layer::VideoPlacement>,
    ) {
        self.placement = placement;
    }

    pub(crate) fn has_video(&self) -> bool {
        self.video.has_frame()
    }

    pub(crate) fn clear_video(&mut self) {
        self.video.clear();
        self.placement = None;
    }

    pub(crate) fn resize(&mut self, size: PhysicalSize<u32>) -> Result<()> {
        if size.width == 0 || size.height == 0 || size == self.size {
            return Ok(());
        }
        let Some(surface) = &self.surface else {
            return Ok(());
        };
        self.config.width = size.width;
        self.config.height = size.height;
        surface.configure(&self.device, &self.config);
        self.size = size;
        self.configured = true;
        Ok(())
    }

    pub(crate) fn render(
        &mut self,
        context: &egui::Context,
        output: RendererOutput,
        transparent: bool,
    ) -> Result<UiFrame> {
        self.defer_output(output);
        let Some(mut output) = self.pending_output.take() else {
            return Ok(UiFrame::Pending);
        };
        let Some(surface) = &self.surface else {
            // 只有窗口不存在时才会走到这里，把这一帧留给下次。
            self.pending_output = Some(output);
            return Ok(UiFrame::Pending);
        };
        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                // Still drawable; the next resize reconfigures it properly.
                self.configured = false;
                frame
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                // The compositor dropped the swap chain, commonly on a monitor
                // or scale change. Reconfigure and draw on the next repaint.
                surface.configure(&self.device, &self.config);
                self.pending_output = Some(output);
                self.flush_writes();
                context.request_repaint();
                return Ok(UiFrame::Pending);
            }
            other => {
                // Timeout or occluded: keep the frame and try again later.
                tracing::debug!(?other, "窗口绘制表面暂时不可用");
                self.pending_output = Some(output);
                self.flush_writes();
                context.request_repaint();
                return Ok(UiFrame::Pending);
            }
        };
        let pixels_per_point = output.pixels_per_point;
        let jobs = context.tessellate(output.shapes, pixels_per_point);
        let descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [self.config.width, self.config.height],
            pixels_per_point,
        };
        for (id, deltas) in &output.textures_delta.set {
            for delta in deltas {
                self.renderer
                    .update_texture(&self.device, &self.queue, *id, delta);
            }
        }
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("openuuyc-ui"),
            });
        let user_buffers = self.renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &jobs,
            &descriptor,
        );
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        {
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("openuuyc-ui"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.0,
                                g: 0.0,
                                b: 0.0,
                                a: if transparent { 0.0 } else { 1.0 },
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            if let Some(placement) = self.placement {
                self.video.draw(
                    &self.queue,
                    &mut pass,
                    (self.config.width, self.config.height),
                    placement,
                    self.transform.unwrap_or_default(),
                );
            }
            self.renderer.render(&mut pass, &jobs, &descriptor);
        }
        let submission = self
            .queue
            .submit(user_buffers.into_iter().chain(Some(encoder.finish())));
        // Freeing after submit keeps a texture alive while the frame still uses it.
        for id in &output.textures_delta.free {
            self.renderer.free_texture(id);
        }
        // Every delta has been handed to the renderer; epaint asserts on drop
        // if any is still pending.
        output.textures_delta.clear();
        self.queue.present(frame);
        // Nothing else reclaims what a finished submission held. Each frame's
        // `write_texture` takes a staging buffer from the device, and neither
        // submitting nor presenting hands it back -- only maintaining the device
        // does. Without this the buffers pile up at the video bitrate, a few
        // hundred megabytes a second, until an upload fails with out of memory.
        let _ = submission;
        if let Err(error) = self.device.poll(wgpu::PollType::Poll) {
            tracing::debug!(?error, "维护 GPU 设备失败");
        }
        Ok(UiFrame::Presented)
    }

    /// Hand any queued `write_texture` data to the GPU without drawing.
    ///
    /// A write is only staged until the next submission, so a frame uploaded
    /// for a pass that then bails would otherwise hold its staging copy inside
    /// the device forever. An empty submission is enough to release it.
    fn flush_writes(&self) {
        self.queue.submit(std::iter::empty());
        if let Err(error) = self.device.poll(wgpu::PollType::Poll) {
            tracing::debug!(?error, "维护 GPU 设备失败");
        }
    }

    pub(crate) fn defer_output(&mut self, output: RendererOutput) {
        // Same merge rule as egui::FullOutput::append: only the newest shapes,
        // but all required texture/font changes.
        if let Some(pending) = &mut self.pending_output {
            pending.textures_delta.append(output.textures_delta);
            pending.shapes = output.shapes;
            pending.pixels_per_point = output.pixels_per_point;
        } else {
            self.pending_output = Some(output);
        }
    }
}

impl Drop for UiPresenter {
    fn drop(&mut self) {
        // A deferred frame still owns texture deltas that will never be applied.
        if let Some(mut pending) = self.pending_output.take() {
            pending.textures_delta.clear();
        }
    }
}

/// 一个新窗口的绘制表面与配置。
fn create_surface(
    window: &Arc<Window>,
    graphics: &Graphics,
) -> Result<(wgpu::Surface<'static>, wgpu::SurfaceConfiguration)> {
    let size = nonzero_size(window.inner_size());
    let surface = graphics
        .instance
        .create_surface(Arc::clone(window))
        .context("创建窗口绘制表面失败")?;
    // The device was chosen for the first window; a window on a screen that
    // GPU cannot reach would otherwise abort in configure().
    anyhow::ensure!(
        graphics.adapter.is_surface_supported(&surface),
        "所选 GPU（{}）无法在这个窗口上绘制",
        graphics.label
    );
    let capabilities = surface.get_capabilities(&graphics.adapter);
    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: surface_format(&capabilities),
        width: size.width,
        height: size.height,
        present_mode: present_mode(&capabilities),
        desired_maximum_frame_latency: 2,
        alpha_mode: alpha_mode(&capabilities),
        color_space: wgpu::SurfaceColorSpace::Auto,
        view_formats: Vec::new(),
    };
    surface.configure(&graphics.device, &config);
    Ok((surface, config))
}

/// 窗口销毁重建之间留下的绘制资源。egui 的纹理和管线都在 presenter 里，丢了就
/// 等于所有文字和图片都要重新上载一次，所以跟随应用一起留下。
pub(crate) struct Retained(UiPresenter);

impl Retained {
    pub(crate) fn take(mut presenter: UiPresenter) -> Self {
        presenter.detach();
        Self(presenter)
    }

    pub(crate) fn reuse(self, window: Arc<Window>, graphics: &Graphics) -> Result<UiPresenter> {
        let mut presenter = self.0;
        presenter.attach(window, graphics)?;
        Ok(presenter)
    }

    /// Keep a pass run without a window: its texture uploads land on the next
    /// window, as a minimized window's do.
    pub(crate) fn defer_output(&mut self, output: RendererOutput) {
        self.0.defer_output(output);
    }
}

/// Whether `set_visible(false)` takes the window off screen. AppKit orders
/// the window out, so it always can.
pub(crate) fn can_hide(_window: &Window) -> bool {
    true
}

/// egui writes gamma-space colors, so a non-sRGB 8-bit format is the match.
fn surface_format(capabilities: &wgpu::SurfaceCapabilities) -> wgpu::TextureFormat {
    const PREFERRED: [wgpu::TextureFormat; 2] = [
        wgpu::TextureFormat::Bgra8Unorm,
        wgpu::TextureFormat::Rgba8Unorm,
    ];
    PREFERRED
        .into_iter()
        .find(|format| capabilities.formats.contains(format))
        .or_else(|| capabilities.formats.first().copied())
        .unwrap_or(wgpu::TextureFormat::Bgra8Unorm)
}

/// Mailbox never blocks the event loop; Fifo is the always-present fallback.
fn present_mode(capabilities: &wgpu::SurfaceCapabilities) -> wgpu::PresentMode {
    if capabilities
        .present_modes
        .contains(&wgpu::PresentMode::Mailbox)
    {
        wgpu::PresentMode::Mailbox
    } else {
        wgpu::PresentMode::Fifo
    }
}

fn alpha_mode(capabilities: &wgpu::SurfaceCapabilities) -> wgpu::CompositeAlphaMode {
    if capabilities
        .alpha_modes
        .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
    {
        wgpu::CompositeAlphaMode::PreMultiplied
    } else {
        wgpu::CompositeAlphaMode::Auto
    }
}
