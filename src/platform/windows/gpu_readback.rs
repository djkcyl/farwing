//! Readback waits remain cancellable without adding queued frames.
use crate::i18n::t;
use anyhow::{Context, Result};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::{Direct3D11::*, Dxgi::DXGI_ERROR_WAS_STILL_DRAWING};

pub(super) fn map(
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    active: impl Fn() -> bool,
) -> Result<D3D11_MAPPED_SUBRESOURCE> {
    let started = Instant::now();
    // DO_NOT_WAIT does not guarantee command submission. Submit once, then
    // poll without repeatedly flushing or overwriting the pending copy.
    unsafe { context.Flush() };
    loop {
        anyhow::ensure!(active(), "{}", t().system.gpu_readback_cancelled);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        match unsafe {
            context.Map(
                texture,
                0,
                D3D11_MAP_READ,
                D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                Some(&mut mapped),
            )
        } {
            Ok(()) => return Ok(mapped),
            Err(error) if error.code() == DXGI_ERROR_WAS_STILL_DRAWING => {
                anyhow::ensure!(
                    started.elapsed() < Duration::from_secs(8),
                    "{}",
                    t().system.gpu_readback_timeout
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error).context(t().system.gpu_readback_failed),
        }
    }
}
