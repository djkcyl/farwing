//! The CUDA driver API, as far as capture and NVENC need it: one context on
//! the GPU driving the desktop, pitched device buffers, and 2D copies between
//! host and device memory. `libcuda.so.1` comes with the NVIDIA driver and is
//! loaded at run time, so machines without it simply have no CUDA path.
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::sync::{Arc, OnceLock};

type CuResult = c_int;
pub(crate) type DevicePtr = u64;

const MEMORY_HOST: c_uint = 1;
const MEMORY_DEVICE: c_uint = 2;

#[repr(C)]
struct Memcpy2d {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: c_uint,
    src_host: *const c_void,
    src_device: DevicePtr,
    src_array: *mut c_void,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: c_uint,
    dst_host: *mut c_void,
    dst_device: DevicePtr,
    dst_array: *mut c_void,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}
impl Default for Memcpy2d {
    fn default() -> Self {
        // SAFETY: all-zero is the documented "unused" value of every field.
        unsafe { std::mem::zeroed() }
    }
}

struct Api {
    _library: libloading::Library,
    device_get_count: unsafe extern "C" fn(*mut c_int) -> CuResult,
    device_get: unsafe extern "C" fn(*mut c_int, c_int) -> CuResult,
    device_get_pci_bus_id: unsafe extern "C" fn(*mut c_char, c_int, c_int) -> CuResult,
    primary_ctx_retain: unsafe extern "C" fn(*mut *mut c_void, c_int) -> CuResult,
    primary_ctx_release: unsafe extern "C" fn(c_int) -> CuResult,
    ctx_push: unsafe extern "C" fn(*mut c_void) -> CuResult,
    ctx_pop: unsafe extern "C" fn(*mut *mut c_void) -> CuResult,
    ctx_synchronize: unsafe extern "C" fn() -> CuResult,
    ctx_set_limit: unsafe extern "C" fn(c_uint, usize) -> CuResult,
    mem_alloc_pitch:
        unsafe extern "C" fn(*mut DevicePtr, *mut usize, usize, usize, c_uint) -> CuResult,
    mem_free: unsafe extern "C" fn(DevicePtr) -> CuResult,
    memcpy_2d: unsafe extern "C" fn(*const Memcpy2d) -> CuResult,
    get_error_string: unsafe extern "C" fn(CuResult, *mut *const c_char) -> CuResult,
}

/// Call before opening any VA-API display. nvidia-vaapi-driver calls `cuInit`
/// from its library constructor, inside `dlopen` and so under the dynamic
/// loader's lock, and a first `cuInit` dlopens libraries itself: if NvFBC or
/// NVENC runs that first `cuInit` on another thread at the same moment, the
/// two wait on each other forever. Finishing CUDA's initialization here,
/// once, before the VA driver loads leaves its constructor nothing to wait
/// for. A machine without an NVIDIA render node never loads that driver.
pub(crate) fn initialize_before_va() {
    let nvidia = std::fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("renderD"))
        .any(|entry| {
            std::fs::read_to_string(entry.path().join("device/vendor"))
                .is_ok_and(|vendor| vendor.trim() == "0x10de")
        });
    if nvidia && let Err(error) = api() {
        tracing::debug!(%error, "CUDA unavailable before VA-API initialization");
    }
}

fn api() -> Result<&'static Api> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| load().map_err(|error| format!("{error:#}")))
        .as_ref()
        .map_err(|error| anyhow!("{error}"))
}

fn load() -> Result<Api> {
    // SAFETY: the NVIDIA driver's CUDA library, with the documented C
    // signatures of the driver API (the `_v2` entry points are the ABI the
    // unversioned names resolve to in `cuda.h`).
    unsafe {
        let library =
            libloading::Library::new("libcuda.so.1").context("未找到 NVIDIA CUDA 驱动")?;
        macro_rules! symbol {
            ($name:literal) => {
                *library
                    .get(concat!($name, "\0").as_bytes())
                    .with_context(|| format!("CUDA 驱动缺少 {}", $name))?
            };
        }
        let init: unsafe extern "C" fn(c_uint) -> CuResult = symbol!("cuInit");
        let api = Api {
            device_get_count: symbol!("cuDeviceGetCount"),
            device_get: symbol!("cuDeviceGet"),
            device_get_pci_bus_id: symbol!("cuDeviceGetPCIBusId"),
            primary_ctx_retain: symbol!("cuDevicePrimaryCtxRetain"),
            primary_ctx_release: symbol!("cuDevicePrimaryCtxRelease_v2"),
            ctx_push: symbol!("cuCtxPushCurrent_v2"),
            ctx_pop: symbol!("cuCtxPopCurrent_v2"),
            ctx_synchronize: symbol!("cuCtxSynchronize"),
            ctx_set_limit: symbol!("cuCtxSetLimit"),
            mem_alloc_pitch: symbol!("cuMemAllocPitch_v2"),
            mem_free: symbol!("cuMemFree_v2"),
            memcpy_2d: symbol!("cuMemcpy2D_v2"),
            get_error_string: symbol!("cuGetErrorString"),
            _library: library,
        };
        check(&api, init(0), "cuInit")?;
        Ok(api)
    }
}

fn check(api: &Api, result: CuResult, operation: &str) -> Result<()> {
    if result == 0 {
        return Ok(());
    }
    let mut text = std::ptr::null();
    // SAFETY: cuGetErrorString only writes a static string pointer.
    let detail = unsafe {
        if (api.get_error_string)(result, &mut text) == 0 && !text.is_null() {
            CStr::from_ptr(text).to_string_lossy().into_owned()
        } else {
            String::new()
        }
    };
    bail!("CUDA {operation} 失败（{result}）：{detail}")
}

/// The primary context of one GPU, retained for as long as this is alive.
pub(crate) struct Context {
    api: &'static Api,
    device: c_int,
    raw: *mut c_void,
    pci_bus: String,
}
// A CUDA context may be made current on any thread; every use pushes it.
unsafe impl Send for Context {}
unsafe impl Sync for Context {}

impl Context {
    /// The GPUs CUDA can see, by ordinal.
    pub fn devices() -> Result<Vec<i32>> {
        let api = api()?;
        let mut count = 0;
        check(
            api,
            unsafe { (api.device_get_count)(&mut count) },
            "cuDeviceGetCount",
        )?;
        Ok((0..count).collect())
    }

    /// The (shared) primary context of CUDA device `ordinal`.
    pub fn shared(ordinal: i32) -> Result<Arc<Self>> {
        static CONTEXTS: std::sync::Mutex<Vec<std::sync::Weak<Context>>> =
            std::sync::Mutex::new(Vec::new());
        let mut contexts = CONTEXTS.lock().unwrap_or_else(|e| e.into_inner());
        contexts.retain(|c| c.strong_count() > 0);
        if let Some(context) = contexts
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .find(|c| c.device == ordinal)
        {
            return Ok(context);
        }
        let context = Arc::new(Self::retain(ordinal)?);
        contexts.push(Arc::downgrade(&context));
        Ok(context)
    }

    fn retain(ordinal: i32) -> Result<Self> {
        let api = api()?;
        let mut device = 0;
        check(
            api,
            unsafe { (api.device_get)(&mut device, ordinal) },
            "cuDeviceGet",
        )?;
        let mut bus = [0 as c_char; 32];
        check(
            api,
            unsafe { (api.device_get_pci_bus_id)(bus.as_mut_ptr(), bus.len() as c_int, device) },
            "cuDeviceGetPCIBusId",
        )?;
        let pci_bus = unsafe { CStr::from_ptr(bus.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let mut raw = std::ptr::null_mut();
        check(
            api,
            unsafe { (api.primary_ctx_retain)(&mut raw, device) },
            "cuDevicePrimaryCtxRetain",
        )?;
        let context = Self {
            api,
            device,
            raw,
            pci_bus,
        };
        context.trim()?;
        Ok(context)
    }

    /// Give back the context's default reservations. This process launches
    /// no kernels of its own, and the ones inside NvFBC and NVENC need no
    /// device heap or printf buffer; the per-thread stack, reserved for every
    /// resident thread of the GPU, dominates a fresh context (about 90 MiB on
    /// an 84-SM GPU), and the driver grows it again for any launch that needs
    /// more. A fresh context drops from about 260 MiB to about 140 MiB.
    fn trim(&self) -> Result<()> {
        const STACK_SIZE: c_uint = 0;
        const PRINTF_FIFO_SIZE: c_uint = 1;
        const MALLOC_HEAP_SIZE: c_uint = 2;
        let _current = self.enter()?;
        for (limit, name) in [
            (STACK_SIZE, "stack"),
            (PRINTF_FIFO_SIZE, "printf"),
            (MALLOC_HEAP_SIZE, "heap"),
        ] {
            check(
                self.api,
                unsafe { (self.api.ctx_set_limit)(limit, 0) },
                &format!("cuCtxSetLimit({name})"),
            )?;
        }
        Ok(())
    }

    pub fn raw(&self) -> *mut c_void {
        self.raw
    }

    pub fn pci_bus(&self) -> &str {
        &self.pci_bus
    }

    /// Make this context current on the calling thread until the guard drops.
    pub fn enter(&self) -> Result<Current<'_>> {
        check(
            self.api,
            unsafe { (self.api.ctx_push)(self.raw) },
            "cuCtxPushCurrent",
        )?;
        Ok(Current(self))
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe {
            (self.api.primary_ctx_release)(self.device);
        }
    }
}

/// The context made current by [`Context::enter`].
pub(crate) struct Current<'a>(&'a Context);
impl Current<'_> {
    pub fn synchronize(&self) -> Result<()> {
        let api = self.0.api;
        check(api, unsafe { (api.ctx_synchronize)() }, "cuCtxSynchronize")
    }
}
impl Drop for Current<'_> {
    fn drop(&mut self) {
        let mut previous = std::ptr::null_mut();
        unsafe {
            (self.0.api.ctx_pop)(&mut previous);
        }
    }
}

/// A pitched 2D allocation of 32-bit pixels in device memory.
pub(crate) struct Buffer {
    context: Arc<Context>,
    pub ptr: DevicePtr,
    pub pitch: usize,
    pub width: u32,
    pub height: u32,
}

impl Buffer {
    pub fn new(context: &Arc<Context>, width: u32, height: u32) -> Result<Self> {
        let api = context.api;
        let _current = context.enter()?;
        let (mut ptr, mut pitch) = (0, 0);
        check(
            api,
            unsafe {
                (api.mem_alloc_pitch)(
                    &mut ptr,
                    &mut pitch,
                    width as usize * 4,
                    height as usize,
                    16,
                )
            },
            "cuMemAllocPitch",
        )?;
        Ok(Self {
            context: context.clone(),
            ptr,
            pitch,
            width,
            height,
        })
    }

    fn copy(&self, copy: Memcpy2d) -> Result<()> {
        let api = self.context.api;
        let current = self.context.enter()?;
        check(api, unsafe { (api.memcpy_2d)(&copy) }, "cuMemcpy2D")?;
        current.synchronize()
    }

    /// Fill from tightly packed BGRA in host memory.
    pub fn upload(&self, pixels: &[u8]) -> Result<()> {
        let row = self.width as usize * 4;
        anyhow::ensure!(
            pixels.len() >= row * self.height as usize,
            "上传画面尺寸不符"
        );
        self.copy(Memcpy2d {
            src_memory_type: MEMORY_HOST,
            src_host: pixels.as_ptr().cast(),
            src_pitch: row,
            dst_memory_type: MEMORY_DEVICE,
            dst_device: self.ptr,
            dst_pitch: self.pitch,
            width_in_bytes: row,
            height: self.height as usize,
            ..Default::default()
        })
    }

    /// Fill from another device allocation of the same size.
    pub fn copy_from_device(&self, source: DevicePtr, pitch: usize) -> Result<()> {
        self.copy(Memcpy2d {
            src_memory_type: MEMORY_DEVICE,
            src_device: source,
            src_pitch: pitch,
            dst_memory_type: MEMORY_DEVICE,
            dst_device: self.ptr,
            dst_pitch: self.pitch,
            width_in_bytes: self.width as usize * 4,
            height: self.height as usize,
            ..Default::default()
        })
    }

    /// Read back as tightly packed BGRA.
    pub fn download(&self) -> Result<Vec<u8>> {
        let row = self.width as usize * 4;
        let mut pixels = vec![0u8; row * self.height as usize];
        self.copy(Memcpy2d {
            src_memory_type: MEMORY_DEVICE,
            src_device: self.ptr,
            src_pitch: self.pitch,
            dst_memory_type: MEMORY_HOST,
            dst_host: pixels.as_mut_ptr().cast(),
            dst_pitch: row,
            width_in_bytes: row,
            height: self.height as usize,
            ..Default::default()
        })?;
        Ok(pixels)
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        let api = self.context.api;
        if let Ok(_current) = self.context.enter() {
            unsafe {
                (api.mem_free)(self.ptr);
            }
        }
    }
}

/// The CUDA device that drives the X screen: NvFBC hands frames to a context
/// on that GPU. Matched through the PCI address of the DRM card with a
/// connected output; the first device when that cannot be told.
pub(crate) fn display_context() -> Result<Arc<Context>> {
    let connected = std::fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            std::fs::read_to_string(entry.path().join("status"))
                .is_ok_and(|status| status.trim() == "connected")
        })
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let card = name.split('-').next()?.to_owned();
            let uevent =
                std::fs::read_to_string(format!("/sys/class/drm/{card}/device/uevent")).ok()?;
            uevent
                .lines()
                .find_map(|line| line.strip_prefix("PCI_SLOT_NAME="))
                .map(|slot| slot.to_ascii_lowercase())
        })
        .collect::<Vec<_>>();
    let devices = Context::devices()?;
    ensure!(!devices.is_empty(), "没有可用的 CUDA 设备");
    for &ordinal in &devices {
        let context = Context::shared(ordinal)?;
        if connected.iter().any(|slot| {
            context
                .pci_bus()
                .to_ascii_lowercase()
                .ends_with(slot.as_str())
        }) {
            return Ok(context);
        }
    }
    Context::shared(devices[0])
}
