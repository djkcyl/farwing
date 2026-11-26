//! NVIDIA Frame Buffer Capture into CUDA memory, Sunshine's `cuda.cpp` path:
//! the driver copies the tracked output (scaled, cursor included on request)
//! straight into video memory, where NVENC encodes it without the picture
//! ever reaching system memory. X11 only, on NVIDIA GPUs that allow NvFBC.
//!
//! As in Sunshine it is only chosen when NVENC works: with a software encoder
//! every frame would have to be read back, which X11 capture does cheaper.
//! `libnvidia-fbc.so.1` ships with the driver and is loaded at run time; the
//! structures follow `NvFBC.h` 1.7 (NVIDIA, MIT licence).
use super::super::cuda::{self, Buffer, Context, DevicePtr};
use super::Screen;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use std::ffi::{CStr, c_char, c_void};
use std::sync::{Arc, OnceLock};

const VERSION: u32 = 7 | (1 << 8);
const fn structure<T>(revision: u32) -> u32 {
    size_of::<T>() as u32 | (revision << 16) | (VERSION << 24)
}
type Status = u32;
type Handle = u64;
const SUCCESS: Status = 0;
const FALSE: u32 = 0;
const TRUE: u32 = 1;
const CAPTURE_SHARED_CUDA: u32 = 1;
const TRACKING_DEFAULT: u32 = 0;
const TRACKING_OUTPUT: u32 = 1;
const BUFFER_BGRA: u32 = 5;
const GRAB_NOWAIT_IF_NEW_FRAME_READY: u32 = 1 << 2;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Box_ {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Size {
    w: u32,
    h: u32,
}
#[repr(C)]
#[derive(Default)]
struct FrameGrabInfo {
    width: u32,
    height: u32,
    byte_size: u32,
    current_frame: u32,
    is_new_frame: u32,
    timestamp_us: u64,
    missed_frames: u32,
    required_post_processing: u32,
    direct_capture: u32,
}
#[repr(C)]
struct CreateHandleParams {
    version: u32,
    private_data: *const c_void,
    private_data_size: u32,
    externally_managed_context: u32,
    glx_context: *mut c_void,
    glx_fb_config: *mut c_void,
}
#[repr(C)]
struct VersionOnly {
    version: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Output {
    id: u32,
    name: [c_char; 128],
    tracked_box: Box_,
}
#[repr(C)]
struct GetStatusParams {
    version: u32,
    is_capture_possible: u32,
    currently_capturing: u32,
    can_create_now: u32,
    screen_size: Size,
    xrandr_available: u32,
    outputs: [Output; 5],
    output_count: u32,
    nvfbc_version: u32,
    in_modeset: u32,
}
#[repr(C)]
#[derive(Default)]
struct CreateCaptureSessionParams {
    version: u32,
    capture_type: u32,
    tracking_type: u32,
    output_id: u32,
    capture_box: Box_,
    frame_size: Size,
    with_cursor: u32,
    disable_auto_modeset_recovery: u32,
    round_frame_size: u32,
    sampling_rate_ms: u32,
    push_model: u32,
    allow_direct_capture: u32,
}
#[repr(C)]
struct ToCudaSetupParams {
    version: u32,
    buffer_format: u32,
}
#[repr(C)]
struct ToCudaGrabFrameParams {
    version: u32,
    flags: u32,
    cuda_device_buffer: *mut c_void,
    frame_grab_info: *mut FrameGrabInfo,
    timeout_ms: u32,
}
type Fn<T> = Option<unsafe extern "C" fn(Handle, *mut T) -> Status>;
#[repr(C)]
struct FunctionList {
    version: u32,
    get_last_error_str: Option<unsafe extern "C" fn(Handle) -> *const c_char>,
    create_handle: Option<unsafe extern "C" fn(*mut Handle, *mut CreateHandleParams) -> Status>,
    destroy_handle: Fn<VersionOnly>,
    get_status: Fn<GetStatusParams>,
    create_capture_session: Fn<CreateCaptureSessionParams>,
    destroy_capture_session: Fn<VersionOnly>,
    to_sys_set_up: *mut c_void,
    to_sys_grab_frame: *mut c_void,
    to_cuda_set_up: Fn<ToCudaSetupParams>,
    to_cuda_grab_frame: Fn<ToCudaGrabFrameParams>,
    pad1: *mut c_void,
    pad2: *mut c_void,
    pad3: *mut c_void,
    bind_context: Fn<VersionOnly>,
    release_context: Fn<VersionOnly>,
    pad4: *mut c_void,
    pad5: *mut c_void,
    pad6: *mut c_void,
    pad7: *mut c_void,
    to_gl_set_up: *mut c_void,
    to_gl_grab_frame: *mut c_void,
}

struct Api {
    _library: libloading::Library,
    functions: FunctionList,
}
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

fn api() -> Result<&'static Api> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| load().map_err(|error| format!("{error:#}")))
        .as_ref()
        .map_err(|error| anyhow!("{error}"))
}

fn load() -> Result<Api> {
    // SAFETY: the NVIDIA driver's NvFBC library and its documented entry point.
    unsafe {
        let library =
            libloading::Library::new("libnvidia-fbc.so.1").context("未找到 NVIDIA NvFBC 驱动库")?;
        let create: unsafe extern "C" fn(*mut FunctionList) -> Status =
            *library.get(b"NvFBCCreateInstance\0")?;
        let mut functions: FunctionList = std::mem::zeroed();
        functions.version = VERSION;
        let status = create(&mut functions);
        ensure!(status == SUCCESS, "创建 NvFBC 实例失败（{status}）");
        ensure!(
            functions.create_handle.is_some()
                && functions.destroy_handle.is_some()
                && functions.get_status.is_some()
                && functions.create_capture_session.is_some()
                && functions.destroy_capture_session.is_some()
                && functions.to_cuda_set_up.is_some()
                && functions.to_cuda_grab_frame.is_some()
                && functions.release_context.is_some(),
            "NvFBC 驱动接口不完整"
        );
        Ok(Api {
            _library: library,
            functions,
        })
    }
}

fn check(api: &Api, handle: Handle, status: Status, operation: &str) -> Result<()> {
    if status == SUCCESS {
        return Ok(());
    }
    let detail = api
        .functions
        .get_last_error_str
        .map(|get| unsafe {
            let text = get(handle);
            if text.is_null() {
                String::new()
            } else {
                CStr::from_ptr(text).to_string_lossy().into_owned()
            }
        })
        .unwrap_or_default();
    bail!("NvFBC {operation} 失败（{status}）：{detail}")
}

pub(super) struct Grab {
    api: &'static Api,
    handle: Handle,
    context: Arc<Context>,
    size: (u32, u32),
    cursor: bool,
    /// Copies of the driver's buffer; a copy still held by the encoder or the
    /// capture loop's cache is never overwritten.
    pool: Vec<Arc<Buffer>>,
}

// NvFBC handles are bound to a thread; the capture thread that owns this
// grab binds it before every use and releases it afterwards.
unsafe impl Send for Grab {}

impl Grab {
    /// Capture `screen` at `size`, with the cursor drawn in when asked.
    pub fn open(screen: &Screen, cursor: bool, size: (u32, u32)) -> Result<Self> {
        ensure!(!super::wayland_session(), "NvFBC 只支持 X11 会话");
        ensure!(
            crate::media::encoding::nvenc::Api::load().is_ok(),
            "NVENC 不可用，NvFBC 画面无法直接编码"
        );
        let api = api()?;
        let context = cuda::display_context()?;
        let _current = context.enter()?;
        let mut handle = 0;
        let mut params = CreateHandleParams {
            version: structure::<CreateHandleParams>(2),
            private_data: std::ptr::null(),
            private_data_size: 0,
            externally_managed_context: FALSE,
            glx_context: std::ptr::null_mut(),
            glx_fb_config: std::ptr::null_mut(),
        };
        // SAFETY: NvFBC calls with parameter structures laid out as NvFBC.h.
        let status = unsafe { api.functions.create_handle.unwrap()(&mut handle, &mut params) };
        check(api, handle, status, "CreateHandle")?;
        let grab = Self {
            api,
            handle,
            context: context.clone(),
            size,
            cursor,
            pool: Vec::new(),
        };
        let result = (|| -> Result<()> {
            let mut status_params: GetStatusParams = unsafe { std::mem::zeroed() };
            status_params.version = structure::<GetStatusParams>(2);
            let status = unsafe { api.functions.get_status.unwrap()(handle, &mut status_params) };
            check(api, handle, status, "GetStatus")?;
            ensure!(
                status_params.is_capture_possible == TRUE,
                "NvFBC 在这块显卡或驱动上被禁用"
            );
            ensure!(
                status_params.can_create_now == TRUE,
                "NvFBC 暂时无法创建采集会话"
            );
            let count = (status_params.output_count as usize).min(status_params.outputs.len());
            let output = status_params.outputs[..count].iter().find(|output| {
                unsafe { CStr::from_ptr(output.name.as_ptr()) }.to_string_lossy()
                    == screen.device_name
            });
            let whole = (status_params.screen_size.w, status_params.screen_size.h)
                == (screen.width, screen.height);
            let (tracking, output_id) = match output {
                Some(output) => (TRACKING_OUTPUT, output.id),
                None if whole => (TRACKING_DEFAULT, 0),
                None => bail!("NvFBC 找不到显示器 {}", screen.device_name),
            };
            let mut session = CreateCaptureSessionParams {
                version: structure::<CreateCaptureSessionParams>(6),
                capture_type: CAPTURE_SHARED_CUDA,
                tracking_type: tracking,
                output_id,
                frame_size: Size {
                    w: size.0,
                    h: size.1,
                },
                with_cursor: if cursor { TRUE } else { FALSE },
                // Poll at up to 240 Hz; the grab waits for a new frame.
                sampling_rate_ms: 4,
                ..Default::default()
            };
            let status =
                unsafe { api.functions.create_capture_session.unwrap()(handle, &mut session) };
            check(api, handle, status, "CreateCaptureSession")?;
            let mut setup = ToCudaSetupParams {
                version: structure::<ToCudaSetupParams>(1),
                buffer_format: BUFFER_BGRA,
            };
            let status = unsafe { api.functions.to_cuda_set_up.unwrap()(handle, &mut setup) };
            check(api, handle, status, "ToCudaSetUp")
        })();
        grab.release();
        drop(_current);
        result.map(|()| grab)
    }

    pub fn name(&self) -> &'static str {
        "NVIDIA NvFBC · CUDA"
    }

    /// The frame size and cursor mode this session was created with.
    pub fn matches(&self, cursor: bool, size: (u32, u32)) -> bool {
        self.cursor == cursor && self.size == size
    }

    fn bind(&self) -> Result<()> {
        let mut params = VersionOnly {
            version: structure::<VersionOnly>(1),
        };
        if let Some(bind) = self.api.functions.bind_context {
            let status = unsafe { bind(self.handle, &mut params) };
            check(self.api, self.handle, status, "BindContext")?;
        }
        Ok(())
    }

    fn release(&self) {
        let mut params = VersionOnly {
            version: structure::<VersionOnly>(1),
        };
        unsafe {
            self.api.functions.release_context.unwrap()(self.handle, &mut params);
        }
    }

    /// The next new frame within `timeout_ms`, in a device buffer of this
    /// session's size. `None` when nothing changed.
    pub fn next(&mut self, timeout_ms: u32) -> Result<Option<Arc<Buffer>>> {
        let context = self.context.clone();
        let _current = context.enter()?;
        self.bind()?;
        let result = self.grab(timeout_ms);
        self.release();
        result
    }

    fn grab(&mut self, timeout_ms: u32) -> Result<Option<Arc<Buffer>>> {
        let mut device: DevicePtr = 0;
        let mut info = FrameGrabInfo::default();
        let mut params = ToCudaGrabFrameParams {
            version: structure::<ToCudaGrabFrameParams>(2),
            flags: GRAB_NOWAIT_IF_NEW_FRAME_READY,
            cuda_device_buffer: (&mut device as *mut DevicePtr).cast(),
            frame_grab_info: &mut info,
            timeout_ms: timeout_ms.max(1),
        };
        let status =
            unsafe { self.api.functions.to_cuda_grab_frame.unwrap()(self.handle, &mut params) };
        check(self.api, self.handle, status, "ToCudaGrabFrame")?;
        if info.is_new_frame != TRUE || device == 0 {
            return Ok(None);
        }
        let (width, height) = (info.width, info.height);
        ensure!(width > 0 && height > 0, "NvFBC 返回空画面");
        self.pool
            .retain(|buffer| (buffer.width, buffer.height) == (width, height));
        let buffer = match self
            .pool
            .iter()
            .find(|buffer| Arc::strong_count(buffer) == 1)
        {
            Some(buffer) => buffer.clone(),
            None => {
                ensure!(self.pool.len() < 4, "NvFBC 画面缓冲全部被占用");
                let buffer = Arc::new(cuda::Buffer::new(&self.context, width, height)?);
                self.pool.push(buffer.clone());
                buffer
            }
        };
        // The driver reuses its buffer on the next grab; keep a copy.
        buffer.copy_from_device(device, width as usize * 4)?;
        Ok(Some(buffer))
    }
}

impl Drop for Grab {
    fn drop(&mut self) {
        let Ok(_current) = self.context.enter() else {
            return;
        };
        if self.bind().is_ok() {
            let mut params = VersionOnly {
                version: structure::<VersionOnly>(1),
            };
            unsafe {
                self.api.functions.destroy_capture_session.unwrap()(self.handle, &mut params);
                let mut params = VersionOnly {
                    version: structure::<VersionOnly>(1),
                };
                self.api.functions.destroy_handle.unwrap()(self.handle, &mut params);
            }
        }
    }
}
