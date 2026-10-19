//! DXGI swap-chain creation and output geometry.
use crate::platform::graphics::nonzero_size;
use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{BOOL, Interface};
use winit::dpi::PhysicalSize;

pub(crate) struct SwapChainCreation {
    pub(crate) swap_chain: IDXGISwapChain1,
    pub(crate) allow_tearing: bool,
    pub(crate) waitable: bool,
    pub(crate) flags: DXGI_SWAP_CHAIN_FLAG,
    pub(crate) buffer_count: u32,
}

pub(crate) fn create_swap_chain(
    device: &ID3D11Device,
    hwnd: HWND,
    size: PhysicalSize<u32>,
) -> Result<SwapChainCreation> {
    let dxgi_device: IDXGIDevice = device.cast().context("query DXGI device")?;
    let adapter = unsafe { dxgi_device.GetAdapter() }.context("get DXGI adapter")?;
    let factory: IDXGIFactory2 = unsafe { adapter.GetParent() }.context("get DXGI factory")?;
    let allow_tearing = factory.cast::<IDXGIFactory5>().ok().is_some_and(|factory| {
        let mut supported = BOOL::default();
        unsafe {
            factory.CheckFeatureSupport(
                DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                (&raw mut supported).cast(),
                std::mem::size_of::<BOOL>() as u32,
            )
        }
        .is_ok()
            && supported.as_bool()
    });
    let buffer_size = swap_chain_output_size(size);
    let mut selected = None;
    for (effect, buffer_count, waitable, tearing) in [
        (DXGI_SWAP_EFFECT_FLIP_DISCARD, 2, true, allow_tearing),
        (DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, 2, true, allow_tearing),
        (DXGI_SWAP_EFFECT_DISCARD, 1, false, false),
    ] {
        let flags = if waitable {
            swap_chain_flags(tearing)
        } else {
            DXGI_SWAP_CHAIN_FLAG(0)
        };
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: buffer_size.width,
            Height: buffer_size.height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: buffer_count,
            Scaling: DXGI_SCALING_NONE,
            SwapEffect: effect,
            AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
            Flags: flags.0 as u32,
        };
        match unsafe {
            factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None::<&IDXGIOutput>)
        } {
            Ok(swap_chain) => {
                selected = Some(SwapChainCreation {
                    swap_chain,
                    allow_tearing: tearing,
                    waitable,
                    flags,
                    buffer_count,
                });
                tracing::info!(
                    swap_effect = effect.0,
                    buffer_count,
                    waitable,
                    tearing,
                    "created D3D11 video swap chain"
                );
                break;
            }
            Err(error) => tracing::debug!(
                %error,
                swap_effect = effect.0,
                "D3D11 swap-chain mode unavailable"
            ),
        }
    }
    let selected = selected.context("create a supported D3D11 video swap chain")?;
    unsafe { factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER) }
        .context("disable DXGI Alt+Enter handling")?;
    Ok(selected)
}

fn swap_chain_flags(allow_tearing: bool) -> DXGI_SWAP_CHAIN_FLAG {
    if allow_tearing {
        DXGI_SWAP_CHAIN_FLAG(
            DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0
                | DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0,
        )
    } else {
        DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT
    }
}

pub(crate) fn is_device_lost(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<windows::core::Error>()
        .is_some_and(|error| {
            matches!(
                error.code(),
                DXGI_ERROR_DEVICE_REMOVED
                    | DXGI_ERROR_DEVICE_RESET
                    | DXGI_ERROR_DRIVER_INTERNAL_ERROR
            )
        })
}

pub(crate) fn swap_chain_output_size(size: PhysicalSize<u32>) -> PhysicalSize<u32> {
    nonzero_size(size)
}

pub(crate) fn validate_visible_geometry(
    visible_x: u32,
    visible_y: u32,
    visible_width: u32,
    visible_height: u32,
    coded_width: u32,
    coded_height: u32,
) -> Result<()> {
    if visible_width == 0
        || visible_height == 0
        || visible_x.saturating_add(visible_width) > coded_width
        || visible_y.saturating_add(visible_height) > coded_height
    {
        bail!(
            "invalid D3D11 video geometry: visible={}x{}+{},{} coded={}x{}",
            visible_width,
            visible_height,
            visible_x,
            visible_y,
            coded_width,
            coded_height
        );
    }
    Ok(())
}

pub(crate) fn fit_rect(
    video_width: u32,
    video_height: u32,
    output_width: u32,
    output_height: u32,
) -> RECT {
    let video_aspect = video_width as f64 / video_height.max(1) as f64;
    let output_aspect = output_width as f64 / output_height.max(1) as f64;
    let (width, height) = if video_aspect > output_aspect {
        (
            output_width,
            (output_width as f64 / video_aspect).round() as u32,
        )
    } else {
        (
            (output_height as f64 * video_aspect).round() as u32,
            output_height,
        )
    };
    let left = output_width.saturating_sub(width) / 2;
    let top = output_height.saturating_sub(height) / 2;
    RECT {
        left: left as i32,
        top: top as i32,
        right: left.saturating_add(width) as i32,
        bottom: top.saturating_add(height) as i32,
    }
}
