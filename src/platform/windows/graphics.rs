//! D3D11 egui composition shared by the device center and playback windows.
use crate::ui::gfx::nonzero_size;
use anyhow::{Context, Result, bail};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;
use winit::dpi::PhysicalSize;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

mod content;
mod readiness;
mod visibility;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiFrame {
    Presented,
    Unchanged,
    Pending,
    Deferred,
}
impl UiFrame {
    pub fn ready(self) -> bool {
        matches!(self, Self::Presented | Self::Unchanged)
    }
    pub fn presented(self) -> bool {
        self == Self::Presented
    }
}

pub(crate) struct UiPresenter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain1,
    target: Option<ID3D11RenderTargetView>,
    backbuffer: Option<ID3D11Texture2D>,
    renderer: egui_directx11::Renderer,
    size: PhysicalSize<u32>,
    requested_size: PhysicalSize<u32>,
    window: HWND,
    deferred: bool,
    // Keep the entire composition tree alive until its swap chain is released.
    composition: IDCompositionDevice,
    composition_target: IDCompositionTarget,
    visual: IDCompositionVisual,
    pending_output: Option<egui_directx11::RendererOutput>,
    pending_present: bool,
    attached: bool,
    content: Option<content::Content>,
    readiness: readiness::Readiness,
}

impl UiPresenter {
    pub(crate) fn new(window: std::sync::Arc<Window>, graphics: &Graphics) -> Result<Self> {
        Self::from_device(&window, graphics.device.clone(), graphics.context.clone())
    }

    /// The player already owns a device shared with its decoder surfaces.
    pub(crate) fn from_device(
        window: &Window,
        device: ID3D11Device,
        context: ID3D11DeviceContext,
    ) -> Result<Self> {
        let size = nonzero_size(window.inner_size());
        let dxgi: IDXGIDevice = device.cast()?;
        let adapter = unsafe { dxgi.GetAdapter() }?;
        let factory: IDXGIFactory2 = unsafe { adapter.GetParent() }?;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: size.width,
            Height: size.height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
            ..Default::default()
        };
        let swap_chain =
            unsafe { factory.CreateSwapChainForComposition(&device, &desc, None::<&IDXGIOutput>) }
                .context("create independent UI composition swap chain")?;
        let readiness = readiness::Readiness::new(&swap_chain)?;
        let composition: IDCompositionDevice = unsafe { DCompositionCreateDevice(&dxgi) }?;
        let composition_target =
            unsafe { composition.CreateTargetForHwnd(window_hwnd(window)?, true) }?;
        let visual = unsafe { composition.CreateVisual() }?;
        unsafe {
            visual.SetContent(&swap_chain)?;
        }
        let (backbuffer, target) = create_backbuffer(&device, &swap_chain)?;
        let renderer = egui_directx11::Renderer::new(&device)?;
        Ok(Self {
            device,
            context,
            swap_chain,
            target: Some(target),
            backbuffer: Some(backbuffer),
            renderer,
            size,
            requested_size: size,
            window: window_hwnd(window)?,
            deferred: false,
            composition,
            composition_target,
            visual,
            pending_output: None,
            pending_present: false,
            attached: false,
            content: None,
            readiness,
        })
    }

    pub(crate) fn resize(&mut self, size: PhysicalSize<u32>) -> Result<()> {
        if size.width > 0 && size.height > 0 {
            self.requested_size = size;
        }
        Ok(())
    }

    fn resize_ready(&mut self) -> Result<()> {
        let size = self.requested_size;
        unsafe {
            self.context.ClearState();
            self.context.Flush();
        }
        self.target.take();
        self.backbuffer.take();
        self.pending_present = false;
        self.content = None;
        unsafe {
            self.swap_chain.ResizeBuffers(
                2,
                size.width,
                size.height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
            )
        }
        .context("resize UI composition swap chain")?;
        let (buffer, target) = create_backbuffer(&self.device, &self.swap_chain)?;
        self.backbuffer = Some(buffer);
        self.target = Some(target);
        self.size = size;
        Ok(())
    }

    pub(crate) fn render(
        &mut self,
        context: &egui::Context,
        output: egui_directx11::RendererOutput,
        transparent: bool,
    ) -> Result<UiFrame> {
        self.defer_output(output);
        let deferred = self.attached && !visibility::drawable(self.window);
        if deferred != self.deferred {
            self.deferred = deferred;
            tracing::debug!(deferred, "UI presentation visibility changed");
        }
        if deferred {
            return Ok(UiFrame::Deferred);
        }
        let resized = self.requested_size != self.size;
        if resized {
            // A frame not yet submitted at the old size is obsolete. Keep the
            // acquired readiness permit and every pending texture delta.
            self.pending_present = false;
        }
        let mut presented = false;
        if self.pending_present {
            if !self.try_present()? {
                context.request_repaint_after(Duration::from_millis(8));
                return Ok(UiFrame::Pending);
            }
            presented = true;
        }
        let output = self.pending_output.as_ref().expect("queued UI output");
        if !resized
            && self
                .content
                .as_ref()
                .is_some_and(|last| last.matches(output, context.zoom_factor(), transparent))
        {
            self.pending_output = None;
            return Ok(if presented {
                UiFrame::Presented
            } else {
                UiFrame::Unchanged
            });
        }
        if !self.readiness.ready()? {
            context.request_repaint_after(Duration::from_millis(8));
            return Ok(UiFrame::Pending);
        }
        if resized {
            self.resize_ready()?;
        }
        let output = self.pending_output.take().expect("queued UI output");
        let content = content::Content::new(&output, context.zoom_factor(), transparent);
        let target = self
            .target
            .as_ref()
            .context("UI render target unavailable")?;
        let alpha = if transparent { 0.0 } else { 1.0 };
        unsafe {
            self.context
                .ClearRenderTargetView(target, &[0.0, 0.0, 0.0, alpha]);
        }
        self.renderer
            .render(&self.context, target, context, output)
            .context("draw UI layer")?;
        self.content = Some(content);
        self.pending_present = true;
        let presented = self.try_present()?;
        if !presented {
            context.request_repaint_after(Duration::from_millis(8));
        }
        Ok(if presented {
            UiFrame::Presented
        } else {
            UiFrame::Pending
        })
    }

    pub(crate) fn defer_output(&mut self, output: egui_directx11::RendererOutput) {
        // Same merge rule as egui::FullOutput::append: only the newest shapes,
        // but all required texture/font changes. This is UI output, not a
        // video-frame queue. Keep processing input while DXGI is occupied.
        if let Some(pending) = &mut self.pending_output {
            content::merge(pending, output);
        } else {
            self.pending_output = Some(output);
        }
    }

    fn try_present(&mut self) -> Result<bool> {
        let started = Instant::now();
        // A UI message-pump thread must never wait for the composition queue.
        // WAS_STILL_DRAWING is backpressure, not a device failure. Retry on a
        // later repaint, without repeatedly uploading/drawing the same frame.
        let status = unsafe { self.swap_chain.Present(0, DXGI_PRESENT_DO_NOT_WAIT) };
        let elapsed = started.elapsed();
        if status == DXGI_ERROR_WAS_STILL_DRAWING {
            return Ok(false);
        }
        if elapsed >= Duration::from_millis(50) || status != windows::core::HRESULT(0) {
            tracing::warn!(
                elapsed_ms = elapsed.as_secs_f64() * 1000.0,
                hresult = status.0,
                "player UI Present completed with delay or nonzero status"
            );
        }
        status.ok().context("present player UI layer")?;
        if !self.attached {
            // Do not expose an uninitialized swap chain during startup or replacement.
            unsafe {
                self.composition_target.SetRoot(&self.visual)?;
                self.composition.Commit()?;
            }
            self.attached = true;
        }
        self.pending_present = false;
        self.readiness.presented();
        Ok(true)
    }
}

impl Drop for UiPresenter {
    fn drop(&mut self) {
        if let Some(mut pending) = self.pending_output.take() {
            pending.textures_delta.clear();
        }
        unsafe {
            let _ = self.visual.SetContent(None::<&windows::core::IUnknown>);
            let _ = self
                .composition_target
                .SetRoot(None::<&IDCompositionVisual>);
            let _ = self.composition.Commit();
            self.context.ClearState();
            self.context.Flush();
        }
    }
}

/// Win32 hides a window in place (`can_hide` is always true), so the control
/// center never destroys its window while it lives in the tray: the updater
/// and notification activation find the process by that HWND, and the
/// presenter's composition tree stays bound to it. No drawing resources ever
/// outlive their window here, hence an uninhabited type.
pub(crate) enum Retained {}

impl Retained {
    pub(crate) fn take(_presenter: UiPresenter) -> Self {
        unreachable!("Windows hides the control center instead of destroying its window")
    }

    pub(crate) fn reuse(
        self,
        _window: std::sync::Arc<Window>,
        _graphics: &Graphics,
    ) -> Result<UiPresenter> {
        match self {}
    }

    pub(crate) fn defer_output(&mut self, _output: egui_directx11::RendererOutput) {
        match *self {}
    }
}

/// `set_visible(false)` takes a Win32 window off screen.
pub(crate) fn can_hide(_window: &Window) -> bool {
    true
}

pub(crate) fn window_hwnd(window: &Window) -> Result<HWND> {
    let RawWindowHandle::Win32(handle) = window
        .window_handle()
        .context("get Windows video window handle")?
        .as_raw()
    else {
        bail!("video window did not expose a Win32 handle");
    };
    Ok(HWND(handle.hwnd.get() as *mut std::ffi::c_void))
}

pub(crate) fn create_backbuffer(
    device: &ID3D11Device,
    swap_chain: &IDXGISwapChain1,
) -> Result<(ID3D11Texture2D, ID3D11RenderTargetView)> {
    let backbuffer = unsafe { swap_chain.GetBuffer::<ID3D11Texture2D>(0) }
        .context("get D3D11 swap-chain backbuffer")?;
    let mut target = None;
    unsafe { device.CreateRenderTargetView(&backbuffer, None, Some(&raw mut target)) }
        .context("create D3D11 swap-chain render target")?;
    Ok((
        backbuffer,
        target.context("D3D11 did not return a render target")?,
    ))
}

/// The D3D11 device behind each shell window.
pub(crate) use device::{Graphics, create_device};
/// egui output for the presenter, kept apart from what the platform consumes.
pub(crate) use egui_directx11::split_output;

mod device {
    use anyhow::{Context, Result, anyhow};
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_10_0,
        D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
    };
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::{IDXGIAdapter1, IDXGIDevice};
    use windows::core::Interface;

    pub(crate) struct Graphics {
        pub(crate) device: ID3D11Device,
        pub(crate) context: ID3D11DeviceContext,
        label: String,
    }

    impl Graphics {
        pub(crate) fn label(&self) -> &str {
            &self.label
        }
    }

    /// Same shape as the Linux device, which is picked to suit the first
    /// window; a D3D11 device presents to any window through DXGI.
    pub(crate) fn create_device(
        _event_loop: &winit::event_loop::ActiveEventLoop,
        _window: &winit::window::Window,
    ) -> Result<Graphics> {
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
                    let label = format!(
                        "{} · D3D11",
                        String::from_utf16_lossy(&desc.Description[..length])
                    );
                    return Ok(Graphics {
                        device,
                        context,
                        label,
                    });
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(anyhow!("create GUI D3D11 device: {:?}", last_error))
    }
}
