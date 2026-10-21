//! Stable oneVPL D3D11 handles; native Lock/Unlock bind MemId without CPU readback.
use super::lock;
use crate::i18n::t;
use anyhow::{Context, Result, ensure};
use farwing_vpl_sys as v;
use std::{collections::HashMap, ffi::c_void, ptr, sync::Mutex};
use windows::{
    Win32::Graphics::{Direct3D11::*, Dxgi::Common::*},
    core::Interface,
};

enum Surface {
    Texture(ID3D11Texture2D),
    Buffer(ID3D11Buffer),
}
impl Surface {
    fn raw(&self) -> *mut c_void {
        match self {
            Self::Texture(t) => t.as_raw(),
            Self::Buffer(b) => b.as_raw(),
        }
    }
}
#[derive(Default)]
struct State {
    surfaces: HashMap<usize, Box<Surface>>,
    allocations: HashMap<usize, Box<[v::mfxMemId]>>,
}
pub(super) struct Allocator {
    device: ID3D11Device,
    bind_flags: u32,
    state: Mutex<State>,
}
impl Allocator {
    pub fn new(device: &ID3D11Device, bind_flags: u32) -> Result<Box<Self>> {
        Ok(Box::new(Self {
            device: device.clone(),
            bind_flags,
            state: Mutex::new(State::default()),
        }))
    }
    pub fn callbacks(&mut self) -> v::mfxFrameAllocator {
        v::mfxFrameAllocator {
            pthis: std::ptr::from_mut(self).cast(),
            Alloc: Some(allocate),
            Free: Some(free),
            GetHDL: Some(handle),
            Lock: Some(map),
            Unlock: Some(unmap),
            ..Default::default()
        }
    }
    pub fn register(&self, texture: ID3D11Texture2D) -> v::mfxMemId {
        let surface = Box::new(Surface::Texture(texture));
        let key = std::ptr::from_ref(&*surface) as usize;
        lock(&self.state).surfaces.insert(key, surface);
        key as *mut c_void
    }
    /// Only after ENCODE_Close, with the old input no longer in flight.
    pub fn prepare_reinitialize(&mut self, bind_flags: u32) -> Result<()> {
        let mut state = lock(&self.state);
        ensure!(
            state.allocations.is_empty(),
            "{}",
            t().system.qsv_allocations_outstanding
        );
        state.surfaces.clear();
        // The callback object and pthis remain stable; only future allocations
        // change their resource bindings after all old native work is closed.
        self.bind_flags = bind_flags;
        Ok(())
    }
}
fn ffi(body: impl FnOnce() -> Result<()>) -> v::mfxStatus {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(Ok(())) => 0,
        Ok(Err(error)) => {
            tracing::debug!(%error,"QSV allocator rejected request");
            v::mfxStatus_MFX_ERR_MEMORY_ALLOC
        }
        Err(_) => v::mfxStatus_MFX_ERR_UNKNOWN,
    }
}
pub(super) fn dxgi(fourcc: u32) -> Result<DXGI_FORMAT> {
    match fourcc as i32 {
        v::MFX_FOURCC_NV12 => Ok(DXGI_FORMAT_NV12),
        v::MFX_FOURCC_P010 => Ok(DXGI_FORMAT_P010),
        v::MFX_FOURCC_AYUV => Ok(DXGI_FORMAT_AYUV),
        v::MFX_FOURCC_Y410 => Ok(DXGI_FORMAT_Y410),
        v::MFX_FOURCC_RGB4 => Ok(DXGI_FORMAT_B8G8R8A8_UNORM),
        v::MFX_FOURCC_BGR4 => Ok(DXGI_FORMAT_R8G8B8A8_UNORM),
        v::MFX_FOURCC_YUY2 => Ok(DXGI_FORMAT_YUY2),
        _ => anyhow::bail!("QSV不支持的外部纹理格式 {fourcc}"),
    }
}
unsafe extern "C" fn allocate(
    owner: *mut c_void,
    request: *mut v::mfxFrameAllocRequest,
    response: *mut v::mfxFrameAllocResponse,
) -> v::mfxStatus {
    ffi(|| unsafe {
        ensure!(
            !owner.is_null() && !request.is_null() && !response.is_null(),
            "QSV Alloc空参数"
        );
        let allocator = &*owner.cast::<Allocator>();
        let request = &*request;
        let dimensions = request.Info.__bindgen_anon_1.__bindgen_anon_1;
        let count = request.NumFrameSuggested.max(request.NumFrameMin);
        ensure!(
            count > 0 && count <= 128 && dimensions.Width > 0 && dimensions.Height > 0,
            "QSV分配范围无效"
        );
        let mut surfaces = Vec::new();
        for _ in 0..count {
            if request.Info.FourCC == v::MFX_FOURCC_P8 as u32 {
                let mut buffer = None;
                allocator.device.CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: u32::from(dimensions.Width) * u32::from(dimensions.Height),
                        Usage: D3D11_USAGE_STAGING,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut buffer),
                )?;
                surfaces.push(Box::new(Surface::Buffer(buffer.context("QSV P8缓冲")?)));
                continue;
            }
            let format = dxgi(request.Info.FourCC)?;
            let mut texture = None;
            allocator.device.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Width: dimensions.Width.into(),
                    Height: dimensions.Height.into(),
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: format,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: allocator.bind_flags,
                    ..Default::default()
                },
                None,
                Some(&mut texture),
            )?;
            surfaces.push(Box::new(Surface::Texture(texture.context("QSV分配纹理")?)));
        }
        let mut ids = surfaces
            .iter()
            .map(|s| std::ptr::from_ref(&**s) as v::mfxMemId)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let address = ids.as_mut_ptr();
        let mut state = lock(&allocator.state);
        for (id, surface) in ids.iter().zip(surfaces) {
            state.surfaces.insert(*id as usize, surface);
        }
        state.allocations.insert(address as usize, ids);
        *response = v::mfxFrameAllocResponse {
            AllocId: request.__bindgen_anon_1.AllocId,
            mids: address,
            NumFrameActual: count,
            ..Default::default()
        };
        Ok(())
    })
}
unsafe extern "C" fn free(
    owner: *mut c_void,
    response: *mut v::mfxFrameAllocResponse,
) -> v::mfxStatus {
    ffi(|| unsafe {
        ensure!(!owner.is_null() && !response.is_null(), "QSV Free空参数");
        let allocator = &*owner.cast::<Allocator>();
        let mut state = lock(&allocator.state);
        let ids = state
            .allocations
            .remove(&((*response).mids as usize))
            .context("QSV未知分配");
        for id in ids?.iter() {
            state.surfaces.remove(&(*id as usize));
        }
        (*response).mids = ptr::null_mut();
        (*response).NumFrameActual = 0;
        Ok(())
    })
}
unsafe extern "C" fn handle(
    owner: *mut c_void,
    id: v::mfxMemId,
    out: *mut v::mfxHDL,
) -> v::mfxStatus {
    ffi(|| unsafe {
        ensure!(!owner.is_null() && !out.is_null(), "QSV GetHDL空参数");
        let state = lock(&(*owner.cast::<Allocator>()).state);
        let surface = state
            .surfaces
            .get(&(id as usize))
            .context("QSV未知内存句柄")?;
        *out.cast::<v::mfxHDLPair>() = v::mfxHDLPair {
            first: surface.raw(),
            second: ptr::null_mut(),
        };
        Ok(())
    })
}
unsafe extern "C" fn map(
    owner: *mut c_void,
    id: v::mfxMemId,
    out: *mut v::mfxFrameData,
) -> v::mfxStatus {
    ffi(|| unsafe {
        ensure!(!owner.is_null() && !out.is_null(), "QSV Lock空参数");
        ensure!(
            lock(&(*owner.cast::<Allocator>()).state)
                .surfaces
                .contains_key(&(id as usize)),
            "QSV未知内存句柄"
        );
        (*out).MemId = id;
        Ok(())
    })
}
unsafe extern "C" fn unmap(
    owner: *mut c_void,
    id: v::mfxMemId,
    out: *mut v::mfxFrameData,
) -> v::mfxStatus {
    ffi(|| unsafe {
        ensure!(!owner.is_null(), "QSV Unlock空参数");
        ensure!(
            lock(&(*owner.cast::<Allocator>()).state)
                .surfaces
                .contains_key(&(id as usize)),
            "QSV未知内存句柄"
        );
        if !out.is_null() {
            (*out).MemId = ptr::null_mut();
        }
        Ok(())
    })
}
