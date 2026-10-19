//! Readback only for explicit local diagnostics; never in the playback path.
use super::WindowsGpuVideoFrame;
use anyhow::{Context, Result, ensure};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::{
    Direct3D11::*,
    Dxgi::{Common::*, DXGI_ERROR_WAS_STILL_DRAWING, IDXGIKeyedMutex},
};

struct Acquired(IDXGIKeyedMutex);
impl Drop for Acquired {
    fn drop(&mut self) {
        let _ = unsafe { self.0.ReleaseSync(0) };
    }
}
struct Mapped<'a>(&'a ID3D11DeviceContext, &'a ID3D11Texture2D);
impl Drop for Mapped<'_> {
    fn drop(&mut self) {
        unsafe { self.0.Unmap(self.1, 0) };
    }
}

impl WindowsGpuVideoFrame {
    /// Samples are normalized to 8-bit Y/U/V; preserves the actual texture stride
    /// and SPS crop origin. No production surface is modified.
    pub(crate) fn diagnostic_pixels(
        &self,
        points: &[(u32, u32)],
        cancel: &AtomicBool,
    ) -> Result<Vec<[u16; 3]>> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            self.texture().GetDesc(&mut desc);
        }
        ensure!(
            matches!(
                desc.Format,
                DXGI_FORMAT_NV12 | DXGI_FORMAT_P010 | DXGI_FORMAT_AYUV | DXGI_FORMAT_Y410
            ),
            "不支持的读回格式"
        );
        ensure!(
            points.iter().all(|&(x, y)| x < self.width()
                && y < self.height()
                && self
                    .visible_x()
                    .checked_add(x)
                    .is_some_and(|x| x < desc.Width)
                && self
                    .visible_y()
                    .checked_add(y)
                    .is_some_and(|y| y < desc.Height)),
            "读回坐标超出纹理"
        );
        let device: ID3D11Device = unsafe { self.texture().GetDevice()? };
        let context = unsafe { device.GetImmediateContext()? };
        desc.ArraySize = 1;
        desc.MipLevels = 1;
        desc.BindFlags = 0;
        desc.MiscFlags = 0;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        unsafe {
            device.CreateTexture2D(&desc, None, Some(&mut staging))?;
        }
        let staging = staging.context("无法创建诊断读回纹理")?;
        let _acquired = crate::platform::surface::acquire_owned_texture_sync(self.texture(), 1000)?
            .map(Acquired);
        unsafe {
            context.CopySubresourceRegion(
                &staging,
                0,
                0,
                0,
                0,
                self.texture(),
                self.subresource(),
                None,
            );
            context.Flush();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        loop {
            ensure!(!cancel.load(Ordering::Acquire), "检查已取消");
            match unsafe {
                context.Map(
                    &staging,
                    0,
                    D3D11_MAP_READ,
                    D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                    Some(&mut mapped),
                )
            } {
                Ok(()) => break,
                Err(e) if e.code() == DXGI_ERROR_WAS_STILL_DRAWING => {
                    ensure!(Instant::now() < deadline, "GPU像素读回超时");
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => return Err(e.into()),
            }
        }
        let _mapped = Mapped(&context, &staging);
        let bytes = match desc.Format {
            DXGI_FORMAT_NV12 => 1,
            DXGI_FORMAT_P010 => 2,
            _ => 4,
        };
        ensure!(
            !mapped.pData.is_null() && u64::from(mapped.RowPitch) >= u64::from(desc.Width) * bytes,
            "无效GPU行跨度"
        );
        let pitch = mapped.RowPitch as usize;
        let base = mapped.pData.cast::<u8>();
        let mut values = Vec::with_capacity(points.len());
        for &(x, y) in points {
            let x = (x + self.visible_x()) as usize;
            let y = (y + self.visible_y()) as usize;
            // All reads stay inside validated visible coordinates and the mapped plane stride.
            let value = unsafe {
                match desc.Format {
                    DXGI_FORMAT_NV12 => [
                        *base.add(y * pitch + x) as u16,
                        *base.add((desc.Height as usize + y / 2) * pitch + (x / 2) * 2) as u16,
                        *base.add((desc.Height as usize + y / 2) * pitch + (x / 2) * 2 + 1) as u16,
                    ],
                    DXGI_FORMAT_P010 => [
                        base.add(y * pitch + x * 2).cast::<u16>().read_unaligned() >> 8,
                        base.add((desc.Height as usize + y / 2) * pitch + (x / 2) * 4)
                            .cast::<u16>()
                            .read_unaligned()
                            >> 8,
                        base.add((desc.Height as usize + y / 2) * pitch + (x / 2) * 4 + 2)
                            .cast::<u16>()
                            .read_unaligned()
                            >> 8,
                    ],
                    DXGI_FORMAT_AYUV => [
                        *base.add(y * pitch + x * 4 + 2) as u16,
                        *base.add(y * pitch + x * 4 + 1) as u16,
                        *base.add(y * pitch + x * 4) as u16,
                    ],
                    DXGI_FORMAT_Y410 => {
                        let v = base.add(y * pitch + x * 4).cast::<u32>().read_unaligned();
                        [
                            ((v >> 12) & 255) as u16,
                            ((v >> 2) & 255) as u16,
                            ((v >> 22) & 255) as u16,
                        ]
                    }
                    _ => unreachable!(),
                }
            };
            values.push(value);
        }
        Ok(values)
    }
}
