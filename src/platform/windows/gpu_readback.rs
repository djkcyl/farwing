//! Readback waits remain cancellable without adding queued frames.
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
        anyhow::ensure!(active(), "GPU读回已取消");
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
                    "GPU读回等待超时"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error).context("GPU读回失败"),
        }
    }
}
