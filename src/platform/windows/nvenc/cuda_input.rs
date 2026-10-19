//! Same-adapter D3D11/CUDA input bridge for planar HEVC 4:4:4 10-bit.
//! Only the display driver's public API is loaded; no CUDA runtime/toolkit.
use anyhow::{Context, Result, ensure};
use std::{ffi::c_void, ptr, rc::Rc};
use windows::{
    Win32::Graphics::{Direct3D11::*, Dxgi::IDXGIDevice},
    core::Interface,
};

type Handle = *mut c_void;
type Status = i32;
struct Api {
    push: unsafe extern "system" fn(Handle) -> Status,
    pop: unsafe extern "system" fn(*mut Handle) -> Status,
    destroy: unsafe extern "system" fn(Handle) -> Status,
    free: unsafe extern "system" fn(u64) -> Status,
    unregister: unsafe extern "system" fn(Handle) -> Status,
    map: unsafe extern "system" fn(u32, *mut Handle, Handle) -> Status,
    unmap: unsafe extern "system" fn(u32, *mut Handle, Handle) -> Status,
    array: unsafe extern "system" fn(*mut Handle, Handle, u32, u32) -> Status,
    copy: unsafe extern "system" fn(*const Copy2d) -> Status,
    _library: libloading::os::windows::Library,
}
// CUDA_MEMCPY2D v2 ABI. Device pointers are 64-bit on supported Windows x64.
#[repr(C)]
#[derive(Default)]
struct Copy2d {
    src_x: usize,
    src_y: usize,
    src_type: u32,
    src_host: *const c_void,
    src_device: u64,
    src_array: Handle,
    src_pitch: usize,
    dst_x: usize,
    dst_y: usize,
    dst_type: u32,
    dst_host: *mut c_void,
    dst_device: u64,
    dst_array: Handle,
    dst_pitch: usize,
    width: usize,
    height: usize,
}
fn check(status: Status, operation: &str) -> Result<()> {
    ensure!(status == 0, "CUDA 编码输入 {operation} 失败（{status}）");
    Ok(())
}
pub(super) struct Input {
    api: Rc<Api>,
    pub context: Handle,
    pub pointer: u64,
    pub pitch: u32,
    resource: Handle,
    size: (u32, u32),
}
pub(super) struct Current(Rc<Api>);
impl Drop for Current {
    fn drop(&mut self) {
        unsafe {
            let mut previous = ptr::null_mut();
            (self.0.pop)(&mut previous);
        }
    }
}
impl Input {
    pub fn new(device: &ID3D11Device, texture: &ID3D11Texture2D, size: (u32, u32)) -> Result<Self> {
        unsafe {
            let library = libloading::os::windows::Library::load_with_flags("nvcuda.dll", 0x800)
                .context("未找到 NVIDIA CUDA 驱动接口")?;
            let init = *library.get::<unsafe extern "system" fn(u32) -> Status>(b"cuInit\0")?;
            let get_device = *library
                .get::<unsafe extern "system" fn(*mut i32, Handle) -> Status>(
                    b"cuD3D11GetDevice\0",
                )?;
            let create = *library
                .get::<unsafe extern "system" fn(*mut Handle, u32, i32) -> Status>(
                    b"cuCtxCreate_v2\0",
                )?;
            let register = *library
                .get::<unsafe extern "system" fn(*mut Handle, Handle, u32) -> Status>(
                    b"cuGraphicsD3D11RegisterResource\0",
                )?;
            let allocate = *library.get::<unsafe extern "system" fn(
                *mut u64,
                *mut usize,
                usize,
                usize,
                u32,
            ) -> Status>(b"cuMemAllocPitch_v2\0")?;
            let api = Rc::new(Api {
                push: *library.get(b"cuCtxPushCurrent_v2\0")?,
                pop: *library.get(b"cuCtxPopCurrent_v2\0")?,
                destroy: *library.get(b"cuCtxDestroy_v2\0")?,
                free: *library.get(b"cuMemFree_v2\0")?,
                unregister: *library.get(b"cuGraphicsUnregisterResource\0")?,
                map: *library.get(b"cuGraphicsMapResources\0")?,
                unmap: *library.get(b"cuGraphicsUnmapResources\0")?,
                array: *library.get(b"cuGraphicsSubResourceGetMappedArray\0")?,
                copy: *library.get(b"cuMemcpy2D_v2\0")?,
                _library: library,
            });
            check(init(0), "Init")?;
            let adapter = device.cast::<IDXGIDevice>()?.GetAdapter()?;
            let mut cuda_device = 0;
            check(
                get_device(&mut cuda_device, adapter.as_raw()),
                "D3D11GetDevice",
            )?;
            let mut input = Self {
                api,
                context: ptr::null_mut(),
                pointer: 0,
                pitch: 0,
                resource: ptr::null_mut(),
                size,
            };
            check(create(&mut input.context, 4, cuda_device), "CtxCreate")?;
            // Creation pushes the context. Pop even if registration/allocation fails.
            let _current = Current(input.api.clone());
            check(
                register(&mut input.resource, texture.as_raw(), 0),
                "RegisterResource",
            )?;
            let mut pitch = 0;
            check(
                allocate(
                    &mut input.pointer,
                    &mut pitch,
                    size.0 as usize * 2,
                    size.1 as usize * 3,
                    16,
                ),
                "MemAllocPitch",
            )?;
            input.pitch = pitch.try_into().context("CUDA input pitch overflow")?;
            Ok(input)
        }
    }
    pub fn enter(&self) -> Result<Current> {
        unsafe {
            check((self.api.push)(self.context), "CtxPush")?;
        }
        Ok(Current(self.api.clone()))
    }
    /// Mapping orders preceding D3D work before this synchronous GPU copy.
    /// Unmapping orders the next conversion after the copy; CPU pixels never participate.
    pub fn copy(&self) -> Result<()> {
        unsafe {
            let mut resource = self.resource;
            check(
                (self.api.map)(1, &mut resource, ptr::null_mut()),
                "MapResource",
            )?;
            let result = (|| {
                let mut array = ptr::null_mut();
                check(
                    (self.api.array)(&mut array, resource, 0, 0),
                    "GetMappedArray",
                )?;
                check(
                    (self.api.copy)(&Copy2d {
                        src_type: 3,
                        src_array: array,
                        dst_type: 2,
                        dst_device: self.pointer,
                        dst_pitch: self.pitch as usize,
                        width: self.size.0 as usize * 2,
                        height: self.size.1 as usize * 3,
                        ..Default::default()
                    }),
                    "Memcpy2D",
                )
            })();
            let unmap = check(
                (self.api.unmap)(1, &mut resource, ptr::null_mut()),
                "UnmapResource",
            );
            result.and(unmap)
        }
    }
}
impl Drop for Input {
    fn drop(&mut self) {
        if self.context.is_null() {
            return;
        }
        unsafe {
            if let Ok(_current) = self.enter() {
                if !self.resource.is_null() {
                    (self.api.unregister)(self.resource);
                }
                if self.pointer != 0 {
                    (self.api.free)(self.pointer);
                }
            }
            (self.api.destroy)(self.context);
        }
    }
}
