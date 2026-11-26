//! Video presentation resources, output mode and device recovery.
use super::plugin_video;
use super::shader::{OFFICIAL_SHARED_TEXTURE_CACHE_SIZE, VideoShaderRenderer, VideoTextureView};
use super::swapchain::{
    create_swap_chain, is_device_lost, swap_chain_output_size, validate_visible_geometry,
};
use crate::application::viewer::{DecodedVideoFrame, mutex_lock};
use crate::media::decoder::RenderSurface;
use crate::media::video_color::RenderColor;
use crate::platform::graphics::create_backbuffer;
use crate::ui::gfx::nonzero_size;
use anyhow::{Context, Result, bail};
use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
use windows::Win32::System::Threading::WaitForSingleObjectEx;
use windows::core::Interface;
use winit::dpi::PhysicalSize;

pub(super) struct D3D11Presenter {
    ready: bool,
    pub(super) hwnd: HWND,
    pub(super) hdr_output: bool,
    pub(super) input_hdr: bool,
    pub(super) hdr_output_unavailable: bool,
    pub(super) output_monitor: isize,
    pub(super) output_monitor_hdr: bool,
    pub(super) output_hdr_checked: Option<Instant>,
    pub(super) captures: plugin_video::Captures,
    pub(super) plugin_state: Option<Arc<crate::plugins::ChainShared>>,
    pub(super) effects: Option<plugin_video::Engine>,
    pub(super) effect_revision: u64,
    pub(super) effects_cache_revision: u64,
    pub(super) nominal_fps: f64,
    pub(super) effect_input: Option<plugin_video::Metadata>,
    pub(super) effect_output: Option<plugin_video::Metadata>,
    pub(super) effect_generated: bool,
    pub(super) drew: bool,
    pub(super) submission_wait: Duration,
    pub(super) analysis_at: Instant,
    pub(super) analysis_new: bool,
    pub(super) device: ID3D11Device,
    adapter: u64,
    pub(super) context: ID3D11DeviceContext,
    pub(super) swap_chain: IDXGISwapChain1,
    pub(super) frame_latency_waitable: isize,
    pub(super) render_target: Option<ID3D11RenderTargetView>,
    pub(super) backbuffer: Option<ID3D11Texture2D>,
    pub(super) video_renderer: VideoShaderRenderer,
    pub(super) cpu_upload: Option<CpuUploadTexture>,
    pub(super) shared_textures: VecDeque<ImportedSharedTexture>,
    pub(super) active_input_sync: Option<IDXGIKeyedMutex>,
    pub(super) allow_tearing: bool,
    pub(super) swap_chain_flags: DXGI_SWAP_CHAIN_FLAG,
    pub(super) buffer_count: u32,
    pub(super) size: PhysicalSize<u32>,
    pub(super) content_top: u32,
    pub(super) display_audit: Option<DisplayAudit>,
}

// Temporary, explicitly enabled diagnostic. Does not change presentation policy
// or write to disk on the render path. DXGI samples are not necessarily updated
// on every call (notably with hardware flip queues); preserve raw counters/errors.
pub(super) struct DisplayAudit {
    pub(super) path: std::path::PathBuf,
    pub(super) origin: Instant,
    pub(super) samples: Vec<(f64, u32, i32, DXGI_FRAME_STATISTICS, f64)>,
}

impl DisplayAudit {
    pub(super) fn from_environment() -> Option<Self> {
        std::env::var_os("OPENUUYC_DISPLAY_AUDIT").map(|path| Self {
            path: path.into(),
            origin: Instant::now(),
            // Covers the full ~3-minute mixed 30/60/90/120 FPS matrix,
            // including its final high-bitrate 120 FPS segment.
            samples: Vec::with_capacity(32_768),
        })
    }

    pub(super) fn sample(&mut self, swap_chain: &IDXGISwapChain1) {
        if self.samples.len() == self.samples.capacity() {
            return;
        }
        let started = Instant::now();
        let elapsed_ms = self.origin.elapsed().as_secs_f64() * 1_000.0;
        let submitted = unsafe { swap_chain.GetLastPresentCount() }.unwrap_or(0);
        let mut statistics = DXGI_FRAME_STATISTICS::default();
        let status = unsafe { swap_chain.GetFrameStatistics(&mut statistics) }
            .err()
            .map_or(0, |error| error.code().0);
        self.samples.push((
            elapsed_ms,
            submitted,
            status,
            statistics,
            started.elapsed().as_secs_f64() * 1_000_000.0,
        ));
    }

    pub(super) fn save(&self) -> std::io::Result<()> {
        use std::io::Write;
        // Never overwrite an earlier observation, including after device recovery.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)?;
        let mut output = std::io::BufWriter::new(file);
        writeln!(
            output,
            "elapsed_ms,submitted,status,present_count,present_refresh,sync_refresh,sync_qpc,query_us"
        )?;
        for (elapsed, submitted, status, stats, query_us) in &self.samples {
            writeln!(
                output,
                "{elapsed:.6},{submitted},{status},{},{},{},{},{query_us:.3}",
                stats.PresentCount,
                stats.PresentRefreshCount,
                stats.SyncRefreshCount,
                stats.SyncQPCTime
            )?;
        }
        output.flush()
    }
}

pub(super) struct CpuUploadTexture {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) texture: ID3D11Texture2D,
}

pub(super) struct ImportedSharedTexture {
    pub(super) handle: isize,
    pub(super) texture: ID3D11Texture2D,
    pub(super) sync: IDXGIKeyedMutex,
}

pub(super) const OFFICIAL_TEXTURE_SYNC_TIMEOUT_MS: u32 = 100;

impl D3D11Presenter {
    pub(super) fn can_render_surface(
        &self,
        surface: &crate::platform::surface::D3D11Surface,
    ) -> bool {
        if surface.shared_handle().is_some() {
            self.adapter == surface.adapter_id()
        } else {
            self.accepts_surface(surface)
        }
    }

    pub(super) fn accepts_surface(&self, surface: &crate::platform::surface::D3D11Surface) -> bool {
        surface.belongs_to_device(&self.device)
    }

    pub(super) fn for_frame(
        hwnd: isize,
        size: PhysicalSize<u32>,
        frame: &DecodedVideoFrame,
        cpu_device: &Mutex<Option<(ID3D11Device, ID3D11DeviceContext)>>,
    ) -> Result<Self> {
        let (device, context) = match &frame.surface {
            RenderSurface::D3D11(surface) if surface.shared_handle().is_none() => {
                (surface.device().clone(), surface.context().clone())
            }
            RenderSurface::D3D11(surface) => surface.create_renderer_device()?,
            RenderSurface::CpuRgba8(_) => {
                let mut cached = mutex_lock(cpu_device);
                if cached
                    .as_ref()
                    .is_some_and(|(device, _)| unsafe { device.GetDeviceRemovedReason().is_err() })
                {
                    cached.take();
                }
                if cached.is_none() {
                    *cached = Some(
                        crate::platform::surface::D3D11SurfaceWriter::new()?
                            .create_renderer_device()?,
                    );
                }
                cached.as_ref().expect("CPU presentation device").clone()
            }
        };
        Self::from_device_for_window(HWND(hwnd as *mut std::ffi::c_void), size, device, context)
    }

    pub(super) fn from_device_for_window(
        hwnd: HWND,
        size: PhysicalSize<u32>,
        device: ID3D11Device,
        context: ID3D11DeviceContext,
    ) -> Result<Self> {
        let id = unsafe { device.cast::<IDXGIDevice>()?.GetAdapter()?.GetDesc()? }.AdapterLuid;
        let adapter = (u64::from(id.HighPart as u32) << 32) | u64::from(id.LowPart);
        let swap_chain_creation = create_swap_chain(&device, hwnd, size)?;
        let swap_chain = swap_chain_creation.swap_chain;
        let allow_tearing = swap_chain_creation.allow_tearing;
        let (backbuffer, render_target) = create_backbuffer(&device, &swap_chain)?;
        let frame_latency_waitable = if swap_chain_creation.waitable {
            let swap_chain2 = swap_chain
                .cast::<IDXGISwapChain2>()
                .context("query low-latency DXGI swap chain")?;
            if let Err(error) = unsafe { swap_chain2.SetMaximumFrameLatency(1) } {
                tracing::warn!(%error, "failed to set swap-chain maximum frame latency to 1");
            }
            let waitable = unsafe { swap_chain2.GetFrameLatencyWaitableObject() };
            if waitable.is_invalid() {
                tracing::warn!("DXGI did not return a frame-latency waitable object");
            }
            waitable
        } else {
            HANDLE::default()
        };
        let video_renderer = VideoShaderRenderer::new(&device)?;
        Ok(Self {
            hwnd,
            hdr_output: false,
            input_hdr: false,
            hdr_output_unavailable: false,
            output_monitor: 0,
            output_monitor_hdr: false,
            output_hdr_checked: None,
            device,
            adapter,
            context,
            swap_chain,
            ready: false,
            frame_latency_waitable: frame_latency_waitable.0 as isize,
            render_target: Some(render_target),
            backbuffer: Some(backbuffer),
            video_renderer,
            cpu_upload: None,
            shared_textures: VecDeque::with_capacity(OFFICIAL_SHARED_TEXTURE_CACHE_SIZE),
            active_input_sync: None,
            allow_tearing,
            swap_chain_flags: swap_chain_creation.flags,
            buffer_count: swap_chain_creation.buffer_count,
            size,
            content_top: 0,
            display_audit: DisplayAudit::from_environment(),
            captures: plugin_video::Captures::default(),
            plugin_state: None,
            effects: None,
            effect_revision: 0,
            effects_cache_revision: 0,
            nominal_fps: 60.0,
            effect_input: None,
            effect_output: None,
            effect_generated: false,
            drew: false,
            submission_wait: Duration::ZERO,
            analysis_at: Instant::now(),
            analysis_new: false,
        })
    }

    pub(super) fn ready(&mut self) -> Result<bool> {
        if !self.ready && self.frame_latency_waitable != 0 {
            let wait = unsafe {
                WaitForSingleObjectEx(
                    HANDLE(self.frame_latency_waitable as *mut std::ffi::c_void),
                    10,
                    false,
                )
            };
            match wait {
                WAIT_OBJECT_0 => {}
                WAIT_TIMEOUT => return Ok(false),
                _ => {
                    return Err(windows::core::Error::from_thread())
                        .context("poll video frame readiness");
                }
            }
        }
        self.ready = true;
        Ok(true)
    }

    pub(super) fn begin_frame(&self) -> Result<std::time::Duration> {
        anyhow::ensure!(
            self.ready,
            "video frame submitted before swap chain readiness"
        );
        let target = self
            .render_target
            .as_ref()
            .context("D3D11 render target is unavailable")?;
        unsafe {
            self.context
                .ClearRenderTargetView(target, &[0.0, 0.0, 0.0, 1.0]);
        }
        Ok(Duration::ZERO)
    }

    pub(super) fn reset_video_resources(&mut self) {
        self.release_active_input_sync();
        self.video_renderer.reset_input_cache();
        self.cpu_upload = None;
        self.shared_textures.clear();
        unsafe {
            self.context.ClearState();
            self.context.Flush();
        }
    }

    pub(super) fn draw_video(
        &mut self,
        surface: &crate::platform::surface::D3D11Surface,
        width: u32,
        height: u32,
        rotation: u16,
        color: RenderColor,
    ) -> Result<()> {
        let (coded_width, coded_height) = surface.coded_size();
        let (visible_x, visible_y) = surface.visible_origin();
        validate_visible_geometry(
            visible_x,
            visible_y,
            width,
            height,
            coded_width,
            coded_height,
        )
        .with_context(|| format!("decoded D3D11 texture format is {:?}", surface.format()))?;
        let (texture, array_slice) = self.acquire_surface_texture(surface)?;
        let result = self.draw_texture(
            &texture,
            VideoTextureView {
                array_slice,
                color,
                input_format: surface.format(),
                visible_x,
                visible_y,
                coded_width,
                coded_height,
                width,
                height,
                rotation,
            },
        );
        if result.is_err() {
            self.release_active_input_sync();
        }
        result
    }

    pub(super) fn acquire_surface_texture(
        &mut self,
        surface: &crate::platform::surface::D3D11Surface,
    ) -> Result<(ID3D11Texture2D, u32)> {
        self.release_active_input_sync();
        if self.accepts_surface(surface) {
            self.active_input_sync = crate::platform::surface::acquire_owned_texture_sync(
                surface.texture(),
                OFFICIAL_TEXTURE_SYNC_TIMEOUT_MS,
            )?;
            return Ok((surface.texture().clone(), surface.subresource()));
        }
        let handle = surface
            .shared_handle()
            .context("decoder returned a non-shared texture for an isolated renderer device")?;
        let imported = if let Some(index) = self
            .shared_textures
            .iter()
            .position(|entry| entry.handle == handle)
        {
            self.shared_textures
                .remove(index)
                .expect("shared texture cache index was checked")
        } else {
            let mut texture = None;
            unsafe {
                self.device.OpenSharedResource::<ID3D11Texture2D>(
                    HANDLE(handle as *mut std::ffi::c_void),
                    &raw mut texture,
                )
            }
            .context("open decoder shared texture on isolated renderer device")?;
            let texture = texture.context("D3D11 did not return an imported shared texture")?;
            let sync: IDXGIKeyedMutex = texture
                .cast()
                .context("query decoder shared texture keyed mutex")?;
            tracing::debug!(
                shared_handle = handle,
                "imported decoder shared texture into the isolated renderer device"
            );
            ImportedSharedTexture {
                handle,
                texture,
                sync,
            }
        };
        crate::platform::surface::acquire_texture_sync(
            &imported.sync,
            OFFICIAL_TEXTURE_SYNC_TIMEOUT_MS,
        )
        .context("acquire decoder shared texture for rendering")?;
        let texture = imported.texture.clone();
        self.active_input_sync = Some(imported.sync.clone());
        self.shared_textures.push_back(imported);
        while self.shared_textures.len() > OFFICIAL_SHARED_TEXTURE_CACHE_SIZE {
            self.shared_textures.pop_front();
        }
        Ok((texture, surface.subresource()))
    }

    pub(super) fn release_active_input_sync(&mut self) {
        if let Some(sync) = self.active_input_sync.take()
            && let Err(error) = unsafe { sync.ReleaseSync(0) }
        {
            tracing::warn!(%error, "failed to release decoder shared texture sync");
        }
    }

    pub(super) fn draw_cpu_video(
        &mut self,
        pixels: &[crate::media::decoder::Rgba8],
        width: u32,
        height: u32,
        rotation: u16,
    ) -> Result<()> {
        let expected = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .context("CPU decoded dimensions overflow")?;
        if pixels.len() < expected {
            bail!("CPU decoded surface is shorter than its dimensions");
        }
        let recreate = self
            .cpu_upload
            .as_ref()
            .is_none_or(|upload| upload.width != width || upload.height != height);
        if recreate {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            let mut texture = None;
            unsafe {
                self.device
                    .CreateTexture2D(&desc, None, Some(&raw mut texture))
            }
            .context("create D3D11 CPU fallback upload texture")?;
            self.cpu_upload = Some(CpuUploadTexture {
                width,
                height,
                texture: texture.context("D3D11 did not return a CPU upload texture")?,
            });
        }
        let upload = self
            .cpu_upload
            .as_ref()
            .context("CPU upload texture was not initialized")?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context.Map(
                &upload.texture,
                0,
                D3D11_MAP_WRITE_DISCARD,
                0,
                Some(&raw mut mapped),
            )
        }
        .context("map D3D11 CPU fallback upload texture")?;
        let source = bytemuck::cast_slice::<crate::media::decoder::Rgba8, u8>(&pixels[..expected]);
        let source_pitch = usize::try_from(width).expect("u32 fits usize") * 4;
        let destination_pitch = mapped.RowPitch as usize;
        for row in 0..usize::try_from(height).expect("u32 fits usize") {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    source.as_ptr().add(row * source_pitch),
                    (mapped.pData as *mut u8).add(row * destination_pitch),
                    source_pitch,
                );
            }
        }
        unsafe { self.context.Unmap(&upload.texture, 0) };
        let texture = upload.texture.clone();
        self.draw_texture(
            &texture,
            VideoTextureView {
                array_slice: 0,
                color: RenderColor::default(), // Already converted RGBA; no YUV transform.
                input_format: DXGI_FORMAT_R8G8B8A8_UNORM,
                visible_x: 0,
                visible_y: 0,
                coded_width: width,
                coded_height: height,
                width,
                height,
                rotation,
            },
        )
    }

    pub(super) fn draw_texture(
        &mut self,
        texture: &ID3D11Texture2D,
        view: VideoTextureView,
    ) -> Result<()> {
        let hdr = view.color.hdr_peak_nits.is_some();
        self.ensure_output_color(hdr)?;
        if self.input_hdr != hdr {
            // Effect/history surfaces carry different transfer functions across
            // SDR/HDR. Never present an old gamma frame as linear HDR, or vice versa.
            self.effects = None;
            self.effect_revision = u64::MAX;
            self.effects_cache_revision = 0;
            self.captures = plugin_video::Captures::default();
            self.input_hdr = hdr;
        }
        let state = self.plugin_state.clone();
        if let Some(state) = &state {
            let graph = state.graph.try_lock().ok().map(|g| g.clone());
            if let Some(graph) = graph
                && graph.revision != self.effect_revision
            {
                self.effect_revision = graph.revision;
                let candidate = if graph.nodes.is_empty() {
                    Ok(None)
                } else {
                    plugin_video::Engine::new(&self.device, &graph, self.nominal_fps).map(Some)
                };
                match candidate {
                    Ok(engine) => {
                        self.effects = engine;
                        self.effects_cache_revision = 0;
                        self.video_renderer.reset_input_cache();
                        state.video_failed.store(0, Ordering::Release);
                        state
                            .applied_revision
                            .store(graph.revision, Ordering::Release);
                    }
                    Err(error) => {
                        *mutex_lock(&state.error) = Some(format!("GPU节点准备失败：{error}"));
                        state
                            .failed_revision
                            .store(graph.revision, Ordering::Release);
                    }
                }
            }
        }
        let taps = state
            .as_ref()
            .and_then(|s| {
                s.taps.try_lock().ok().map(|t| {
                    t.iter()
                        .filter(|t| {
                            t.revision == s.applied_revision.load(Ordering::Acquire)
                                && (t.source == 0
                                    || s.video_failed.load(Ordering::Acquire) != t.revision)
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default();
        if self.analysis_new {
            self.captures.feed(
                0,
                &self.device,
                &self.context,
                texture,
                view,
                &taps,
                self.analysis_at,
                true,
            );
        }
        if let Some(engine) = self.effects.as_mut() {
            let result = if self.analysis_new {
                engine.process(
                    &self.device,
                    &self.context,
                    texture,
                    view,
                    self.analysis_at,
                    self.effect_input,
                    &mut self.captures,
                    &taps,
                )
            } else {
                Ok(())
            };
            if let Err(error) = result {
                if let Some(state) = &state {
                    state.fail(format!("视频链已停止：{error}"));
                }
                self.effects = None;
                self.video_renderer.reset_input_cache();
            } else {
                return self.draw_effect_output(view.color, !self.analysis_new);
            }
        }
        self.submission_wait = self.begin_frame()?;
        self.video_renderer.draw(
            &self.device,
            &self.context,
            self.render_target
                .as_ref()
                .context("D3D11 render target is unavailable")?,
            texture,
            view,
            self.size,
            self.content_top,
        )?;
        self.drew = true;
        self.effect_output = self.effect_input;
        Ok(())
    }
    pub(super) fn draw_effect_output(&mut self, color: RenderColor, redraw: bool) -> Result<()> {
        let Some(engine) = self.effects.as_mut() else {
            return Ok(());
        };
        let Some(state) = &self.plugin_state else {
            return Ok(());
        };
        if engine.cache_revision != self.effects_cache_revision {
            self.video_renderer.reset_input_cache();
            self.effects_cache_revision = engine.cache_revision;
        }
        let due = engine.take_due(&state.skipped);
        let fresh = due.is_some();
        let picture = due.or_else(|| if redraw { engine.last() } else { None });
        if let Some(picture) = picture {
            self.submission_wait = self.begin_frame()?;
            self.video_renderer.draw(
                &self.device,
                &self.context,
                self.render_target
                    .as_ref()
                    .context("effect presentation target")?,
                &picture.target.texture,
                plugin_video::view(&picture.target, color),
                self.size,
                self.content_top,
            )?;
            self.drew = true;
            if fresh {
                self.effect_output = picture.metadata;
                self.effect_generated = picture.generated;
            }
        }
        Ok(())
    }

    pub(super) fn present(&mut self) -> Result<()> {
        // All source reads (including plugin taps) have been submitted. The
        // keyed mutex orders those reads; Present only consumes the backbuffer.
        self.release_active_input_sync();
        let flags = if self.allow_tearing {
            DXGI_PRESENT_ALLOW_TEARING
        } else {
            DXGI_PRESENT(0)
        };
        let started = Instant::now();
        let status = unsafe { self.swap_chain.Present(0, flags) };
        self.ready = false;
        let elapsed = started.elapsed();
        if elapsed >= Duration::from_millis(50) || status != windows::core::HRESULT(0) {
            tracing::warn!(
                elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                hresult = status.0,
                "D3D11 video Present completed with delay or nonzero status"
            );
        }
        let result = status.ok().context("present D3D11 video swap chain");
        self.release_active_input_sync();
        if let Some(audit) = &mut self.display_audit {
            audit.sample(&self.swap_chain);
        }
        result
    }

    pub(super) fn resize(&mut self, size: PhysicalSize<u32>) -> Result<()> {
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        let size = nonzero_size(size);
        if size == self.size {
            return Ok(());
        }
        self.resize_backbuffer(
            size,
            if self.hdr_output {
                DXGI_FORMAT_R16G16B16A16_FLOAT
            } else {
                DXGI_FORMAT_B8G8R8A8_UNORM
            },
        )?;
        if self.hdr_output {
            unsafe {
                self.swap_chain
                    .cast::<IDXGISwapChain3>()?
                    .SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709)
            }
            .context("preserve scRGB color space after resizing")?;
        }
        Ok(())
    }

    pub(super) fn ensure_output_color(&mut self, hdr_source: bool) -> Result<()> {
        if hdr_source {
            let monitor =
                unsafe { MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST) }.0 as isize;
            if monitor != self.output_monitor
                || self
                    .output_hdr_checked
                    .is_none_or(|checked| checked.elapsed() >= Duration::from_secs(1))
            {
                // Capability polling is independent of decoding; also refresh
                // immediately when the window moves to a different monitor.
                let enabled = crate::platform::display_hdr::monitor_is_hdr(monitor);
                if monitor != self.output_monitor || enabled != self.output_monitor_hdr {
                    self.hdr_output_unavailable = false;
                }
                self.output_monitor = monitor;
                self.output_monitor_hdr = enabled;
                self.output_hdr_checked = Some(Instant::now());
            }
        }
        let requested = hdr_source && self.output_monitor_hdr && !self.hdr_output_unavailable;
        if requested == self.hdr_output {
            return Ok(());
        }
        let chain = self.swap_chain.cast::<IDXGISwapChain3>().ok();
        if requested {
            if chain.is_none() {
                self.hdr_output_unavailable = true;
                return Ok(());
            }
            let result = self
                .resize_backbuffer(self.size, DXGI_FORMAT_R16G16B16A16_FLOAT)
                .and_then(|_| {
                    let chain = chain.as_ref().unwrap();
                    let flags = unsafe {
                        chain.CheckColorSpaceSupport(DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709)
                    }?;
                    anyhow::ensure!(
                        flags & DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT.0 as u32 != 0,
                        "swap chain does not support scRGB presentation"
                    );
                    unsafe { chain.SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709) }
                        .context("select scRGB HDR color space")
                });
            if let Err(error) = result {
                if is_device_lost(&error) {
                    return Err(error);
                }
                self.hdr_output_unavailable = true;
                self.resize_backbuffer(self.size, DXGI_FORMAT_B8G8R8A8_UNORM)?;
                if let Some(chain) = &chain {
                    unsafe { chain.SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709) }?;
                }
                tracing::warn!(%error,"HDR swap chain unavailable; using SDR tone mapping");
                return Ok(());
            }
        } else {
            self.resize_backbuffer(self.size, DXGI_FORMAT_B8G8R8A8_UNORM)?;
            if let Some(chain) = &chain {
                unsafe { chain.SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709) }
                    .context("restore SDR color space")?;
            }
        }
        self.hdr_output = requested;
        tracing::info!(
            hdr_output = self.hdr_output,
            hdr_source,
            "updated native display color output"
        );
        Ok(())
    }

    pub(super) fn resize_backbuffer(
        &mut self,
        size: PhysicalSize<u32>,
        format: DXGI_FORMAT,
    ) -> Result<()> {
        self.video_renderer.reset_geometry();
        // ResizeBuffers requires every immediate-context reference to the old
        // backbuffer/output view to be released. The video renderer
        // binds resources on this context during the preceding frame.
        unsafe {
            self.context.ClearState();
            self.context.Flush();
        }
        self.render_target.take();
        self.backbuffer.take();
        let buffer_size = swap_chain_output_size(size);
        unsafe {
            self.swap_chain.ResizeBuffers(
                self.buffer_count,
                buffer_size.width,
                buffer_size.height,
                format,
                self.swap_chain_flags,
            )
        }
        .context("resize D3D11 video swap chain")?;
        let (backbuffer, target) = create_backbuffer(&self.device, &self.swap_chain)?;
        self.backbuffer = Some(backbuffer);
        self.render_target = Some(target);
        if let Some(target) = &self.render_target {
            unsafe {
                self.context
                    .ClearRenderTargetView(target, &[0.0, 0.0, 0.0, 1.0]);
            }
        }
        self.size = size;
        Ok(())
    }
}

impl Drop for D3D11Presenter {
    fn drop(&mut self) {
        if let Some(audit) = self.display_audit.take()
            && let Err(error) = audit.save()
        {
            tracing::warn!(%error, "could not save opt-in DXGI display audit");
        }
        self.release_active_input_sync();
        self.reset_video_resources();
        if self.frame_latency_waitable != 0 {
            if let Err(error) =
                unsafe { CloseHandle(HANDLE(self.frame_latency_waitable as *mut std::ffi::c_void)) }
            {
                tracing::debug!(%error, "failed to close DXGI frame-latency waitable object");
            }
            self.frame_latency_waitable = 0;
        }
    }
}
