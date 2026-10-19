use super::Layout;
use crate::platform::windows::host_service::pipe::Handle;
use anyhow::{Context, Result, ensure};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
        System::Threading::*,
    },
    core::Interface,
};
pub(super) struct Surface {
    pub texture: ID3D11Texture2D,
    pub sync: IDXGIKeyedMutex,
    pub handle: Handle,
    pub layout: Layout,
}
pub(super) fn descriptor(layout: Layout) -> Result<D3D11_TEXTURE2D_DESC> {
    ensure!(
        layout.width > 0 && layout.height > 0 && layout.width <= 16384 && layout.height <= 16384,
        "共享采集尺寸无效"
    );
    let format = DXGI_FORMAT(layout.format);
    ensure!(
        matches!(
            format,
            DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_R16G16B16A16_FLOAT
        ),
        "共享采集格式无效"
    );
    Ok(D3D11_TEXTURE2D_DESC {
        Width: layout.width,
        Height: layout.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    })
}
pub(super) fn texture(
    device: &ID3D11Device,
    desc: &D3D11_TEXTURE2D_DESC,
) -> Result<ID3D11Texture2D> {
    let mut value = None;
    unsafe {
        device.CreateTexture2D(desc, None, Some(&mut value))?;
    }
    value.context("未创建共享采集纹理")
}
impl Surface {
    pub fn new(device: &ID3D11Device, layout: Layout) -> Result<Self> {
        let mut desc = descriptor(layout)?;
        desc.MiscFlags =
            (D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX).0 as u32;
        let texture = texture(device, &desc)?;
        let resource: IDXGIResource1 = texture.cast()?;
        let handle = Handle(unsafe {
            resource.CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                windows::core::PCWSTR::null(),
            )?
        });
        Ok(Self {
            sync: texture.cast()?,
            texture,
            handle,
            layout,
        })
    }
    pub fn open(device: &ID3D11Device, client: u32, value: u64, layout: Layout) -> Result<Self> {
        descriptor(layout)?;
        ensure!(value != 0 && value <= isize::MAX as u64, "采集共享句柄无效");
        let process = Handle(unsafe { OpenProcess(PROCESS_DUP_HANDLE, false, client)? });
        let mut duplicated = HANDLE::default();
        unsafe {
            DuplicateHandle(
                process.0,
                HANDLE(value as usize as *mut _),
                GetCurrentProcess(),
                &mut duplicated,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )?;
        }
        let handle = Handle(duplicated);
        let texture: ID3D11Texture2D = unsafe {
            device
                .cast::<ID3D11Device1>()?
                .OpenSharedResource1(handle.0)?
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            texture.GetDesc(&mut desc);
        }
        ensure!(
            desc.Width == layout.width
                && desc.Height == layout.height
                && desc.Format.0 == layout.format
                && desc.ArraySize == 1
                && desc.MipLevels == 1
                && desc.SampleDesc.Count == 1
                && desc.MiscFlags & D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32 != 0,
            "共享采集纹理不匹配"
        );
        Ok(Self {
            sync: texture.cast()?,
            texture,
            handle,
            layout,
        })
    }
    pub fn acquire(&self, key: u64) -> Result<Option<Guard<'_>>> {
        let status =
            unsafe { (Interface::vtable(&self.sync).AcquireSync)(self.sync.as_raw(), key, 100) };
        // These are nonnegative HRESULTs, but neither grants usable ownership.
        // Renegotiate the shared surface instead of ending the video session.
        if status.0 == WAIT_TIMEOUT.0 as i32 || status.0 == WAIT_ABANDONED.0 as i32 {
            tracing::debug!(?status, "capture shared surface needs replacement");
            return Ok(None);
        }
        ensure!(
            status == windows::core::HRESULT(0),
            "共享采集同步失败：{status:?}"
        );
        Ok(Some(Guard {
            sync: &self.sync,
            release: Some(key),
        }))
    }
}
pub(super) struct Guard<'a> {
    sync: &'a IDXGIKeyedMutex,
    release: Option<u64>,
}
impl Guard<'_> {
    pub fn release(mut self, key: u64) -> Result<()> {
        self.release = None;
        unsafe {
            self.sync.ReleaseSync(key)?;
        }
        Ok(())
    }
}
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        if let Some(key) = self.release.take() {
            let _ = unsafe { self.sync.ReleaseSync(key) };
        }
    }
}
