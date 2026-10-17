//! A PipeWire video stream consumer, loaded at run time.
//!
//! `libpipewire-0.3.so.0` is opened with dlopen rather than linked: every
//! desktop with the ScreenCast portal has it, but its development headers (and
//! the Rust bindings generated from them) track newer releases than long-term
//! distributions ship. The few entry points used here have been stable since
//! PipeWire 0.3, and the SPA format descriptions are written by hand.
//!
//! Frames are requested in shared memory (MemPtr/MemFd), read on PipeWire's
//! loop thread into BGRA and handed to the capture thread as the latest
//! picture; DMA-BUF import belongs with a GPU encoder.
use super::Picture;
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

// --- SPA / PipeWire ABI (spa-0.2, pipewire-0.3) --------------------------------

#[repr(C)]
struct SpaList {
    next: *mut c_void,
    prev: *mut c_void,
}
#[repr(C)]
struct SpaCallbacks {
    funcs: *const c_void,
    data: *mut c_void,
}
#[repr(C)]
struct SpaHook {
    link: SpaList,
    cb: SpaCallbacks,
    removed: Option<unsafe extern "C" fn(*mut SpaHook)>,
    private: *mut c_void,
}
type Callback = Option<unsafe extern "C" fn(*mut c_void)>;
#[repr(C)]
struct StreamEvents {
    version: u32,
    destroy: Callback,
    state_changed: Option<unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_char)>,
    control_info: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    io_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *mut c_void, u32)>,
    param_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    add_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    process: Callback,
    drained: Callback,
    command: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
    trigger_done: Callback,
}
#[repr(C)]
struct PwBuffer {
    buffer: *mut SpaBuffer,
    user_data: *mut c_void,
    size: u64,
}
#[repr(C)]
struct SpaBuffer {
    n_metas: u32,
    n_datas: u32,
    metas: *mut SpaMeta,
    datas: *mut SpaData,
}
#[repr(C)]
struct SpaMeta {
    kind: u32,
    size: u32,
    data: *mut c_void,
}
#[repr(C)]
struct SpaData {
    kind: u32,
    flags: u32,
    fd: i64,
    mapoffset: u32,
    maxsize: u32,
    data: *mut c_void,
    chunk: *mut SpaChunk,
}
#[repr(C)]
struct SpaChunk {
    offset: u32,
    size: u32,
    stride: i32,
    flags: i32,
}
#[repr(C)]
struct SpaMetaRegion {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

const STREAM_EVENTS_VERSION: u32 = 2;
const DIRECTION_INPUT: u32 = 0;
const FLAG_AUTOCONNECT: u32 = 1 << 0;
// Shared-memory buffers are mapped explicitly; DMA-BUF is imported through VA.
const STATE_ERROR: c_int = -1;
const STATE_UNCONNECTED: c_int = 0;
const STATE_STREAMING: c_int = 3;

// Keep the modifier mandatory so DMA-BUF cannot silently become SHM.
// Let PipeWire fixate our supported choices normally. DONT_FIXATE here causes
// xdg-desktop-portal-hyprland 1.4.1 to repeat its allocation negotiation until
// its retry limit is exhausted, after which no new DMA-BUF frames arrive.
const SPA_POD_PROP_FLAG_MANDATORY: u32 = 1 << 3;

const TYPE_ID: u32 = 3;
const TYPE_INT: u32 = 4;
const TYPE_LONG: u32 = 5;
const TYPE_RECTANGLE: u32 = 10;
const TYPE_FRACTION: u32 = 11;
const TYPE_OBJECT: u32 = 15;
const TYPE_CHOICE: u32 = 19;
const OBJECT_FORMAT: u32 = 0x40003;
const OBJECT_PARAM_BUFFERS: u32 = 0x40004;
const OBJECT_PARAM_META: u32 = 0x40005;
const PARAM_ENUM_FORMAT: u32 = 3;
const PARAM_FORMAT: u32 = 4;
const PARAM_BUFFERS: u32 = 5;
const PARAM_META: u32 = 6;
const FORMAT_MEDIA_TYPE: u32 = 1;
const FORMAT_MEDIA_SUBTYPE: u32 = 2;
const FORMAT_VIDEO_FORMAT: u32 = 0x20001;
/// The DRM format modifier. SPA documents it as "use only with DMA-BUF and
/// omit for other buffer types", and that is the whole signal: the portal
/// negotiates shared memory unless this key is present, however the
/// buffer-type mask reads.
const FORMAT_VIDEO_MODIFIER: u32 = 0x20002;
const FORMAT_VIDEO_SIZE: u32 = 0x20003;
const FORMAT_VIDEO_FRAMERATE: u32 = 0x20004;
const MEDIA_TYPE_VIDEO: u32 = 2;
const MEDIA_SUBTYPE_RAW: u32 = 1;
const VIDEO_RGBX: u32 = 7;
const VIDEO_BGRX: u32 = 8;
const VIDEO_RGBA: u32 = 11;
const VIDEO_BGRA: u32 = 12;

/// `I915_FORMAT_MOD_Y_TILED`: what i965 exports and its encoder imports.
const DRM_MOD_Y_TILED: u64 = 0x0100_0000_0000_0002;
/// `I915_FORMAT_MOD_X_TILED`.
const DRM_MOD_X_TILED: u64 = 0x0100_0000_0000_0001;
/// `DRM_FORMAT_MOD_LINEAR`.
const DRM_MOD_LINEAR: u64 = 0;
/// `DRM_FORMAT_MOD_INVALID`, which SPA reads as "let the compositor choose".
const DRM_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
const BUFFERS_DATA_TYPE: u32 = 6;
const META_TYPE: u32 = 1;
const META_SIZE: u32 = 2;
const META_VIDEO_CROP: u32 = 2;
const DATA_MEM_PTR: u32 = 1;
const DATA_MEM_FD: u32 = 2;
const DATA_MEM_DMABUF: u32 = 3;
const CHOICE_RANGE: u32 = 1;
const CHOICE_ENUM: u32 = 3;
const CHOICE_FLAGS: u32 = 4;

struct Api {
    _library: libloading::Library,
    thread_loop_new: unsafe extern "C" fn(*const c_char, *const c_void) -> *mut c_void,
    thread_loop_get_loop: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    thread_loop_start: unsafe extern "C" fn(*mut c_void) -> c_int,
    thread_loop_stop: unsafe extern "C" fn(*mut c_void),
    thread_loop_destroy: unsafe extern "C" fn(*mut c_void),
    thread_loop_lock: unsafe extern "C" fn(*mut c_void),
    thread_loop_unlock: unsafe extern "C" fn(*mut c_void),
    context_new: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void,
    context_destroy: unsafe extern "C" fn(*mut c_void),
    context_connect_fd: unsafe extern "C" fn(*mut c_void, c_int, *mut c_void, usize) -> *mut c_void,
    core_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int,
    properties_new: unsafe extern "C" fn(*const c_char, ...) -> *mut c_void,
    stream_new: unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> *mut c_void,
    stream_add_listener:
        unsafe extern "C" fn(*mut c_void, *mut SpaHook, *const StreamEvents, *mut c_void),
    stream_connect:
        unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut *const c_void, u32) -> c_int,
    stream_update_params: unsafe extern "C" fn(*mut c_void, *mut *const c_void, u32) -> c_int,
    stream_dequeue_buffer: unsafe extern "C" fn(*mut c_void) -> *mut PwBuffer,
    stream_queue_buffer: unsafe extern "C" fn(*mut c_void, *mut PwBuffer) -> c_int,
    stream_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int,
    stream_destroy: unsafe extern "C" fn(*mut c_void),
}

fn api() -> Result<&'static Api> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| load().map_err(|error| format!("{error:#}")))
        .as_ref()
        .map_err(|error| anyhow!("{error}"))
}

fn load() -> Result<Api> {
    // SAFETY: loading the system PipeWire client library; the symbols below
    // are declared with their C signatures from pipewire-0.3.
    unsafe {
        let library = libloading::Library::new("libpipewire-0.3.so.0")
            .context("没有找到 PipeWire（libpipewire-0.3.so.0）")?;
        macro_rules! symbol {
            ($name:literal) => {
                *library
                    .get(concat!($name, "\0").as_bytes())
                    .with_context(|| format!("PipeWire 缺少 {}", $name))?
            };
        }
        let init: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char) = symbol!("pw_init");
        init(std::ptr::null_mut(), std::ptr::null_mut());
        Ok(Api {
            thread_loop_new: symbol!("pw_thread_loop_new"),
            thread_loop_get_loop: symbol!("pw_thread_loop_get_loop"),
            thread_loop_start: symbol!("pw_thread_loop_start"),
            thread_loop_stop: symbol!("pw_thread_loop_stop"),
            thread_loop_destroy: symbol!("pw_thread_loop_destroy"),
            thread_loop_lock: symbol!("pw_thread_loop_lock"),
            thread_loop_unlock: symbol!("pw_thread_loop_unlock"),
            context_new: symbol!("pw_context_new"),
            context_destroy: symbol!("pw_context_destroy"),
            context_connect_fd: symbol!("pw_context_connect_fd"),
            core_disconnect: symbol!("pw_core_disconnect"),
            properties_new: symbol!("pw_properties_new"),
            stream_new: symbol!("pw_stream_new"),
            stream_add_listener: symbol!("pw_stream_add_listener"),
            stream_connect: symbol!("pw_stream_connect"),
            stream_update_params: symbol!("pw_stream_update_params"),
            stream_dequeue_buffer: symbol!("pw_stream_dequeue_buffer"),
            stream_queue_buffer: symbol!("pw_stream_queue_buffer"),
            stream_disconnect: symbol!("pw_stream_disconnect"),
            stream_destroy: symbol!("pw_stream_destroy"),
            _library: library,
        })
    }
}

// --- SPA pod encoding ------------------------------------------------------------

/// Builds one SPA pod in an 8-byte aligned buffer, the layout `spa_pod_builder`
/// produces: every pod is `{u32 size, u32 type}` then its body, padded to 8.
#[derive(Default)]
struct Pod(Vec<u8>);
impl Pod {
    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_ne_bytes());
    }
    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_ne_bytes());
    }
    fn pad(&mut self) {
        while self.0.len() % 8 != 0 {
            self.0.push(0);
        }
    }
    fn value(&mut self, kind: u32, body: &[u32]) {
        self.u32((body.len() * 4) as u32);
        self.u32(kind);
        for word in body {
            self.u32(*word);
        }
        self.pad();
    }
    /// A choice of `Long` values, the form SPA uses for DRM modifiers.
    fn choice_long(&mut self, choice: u32, values: &[u64]) {
        self.u32((16 + values.len() * 8) as u32);
        self.u32(TYPE_CHOICE);
        self.u32(choice);
        self.u32(0);
        self.u32(8);
        self.u32(TYPE_LONG);
        for value in values {
            self.u64(*value);
        }
        self.pad();
    }
    /// A choice of `values`, each `child_words` 32-bit words of `child_type`.
    fn choice(&mut self, choice: u32, child_type: u32, child_words: usize, values: &[u32]) {
        self.u32((16 + values.len() * 4) as u32);
        self.u32(TYPE_CHOICE);
        self.u32(choice);
        self.u32(0);
        self.u32((child_words * 4) as u32);
        self.u32(child_type);
        for word in values {
            self.u32(*word);
        }
        self.pad();
    }
    /// An object with properties written by `props`, each started with `key`.
    fn object(kind: u32, id: u32, props: impl FnOnce(&mut Self)) -> Self {
        let mut pod = Self::default();
        pod.u32(0);
        pod.u32(TYPE_OBJECT);
        pod.u32(kind);
        pod.u32(id);
        props(&mut pod);
        let size = (pod.0.len() - 8) as u32;
        pod.0[..4].copy_from_slice(&size.to_ne_bytes());
        pod
    }
    fn key(&mut self, key: u32) {
        self.key_flags(key, 0);
    }
    /// A property key with `spa_pod_prop` flags.
    fn key_flags(&mut self, key: u32, flags: u32) {
        self.u32(key);
        self.u32(flags);
    }
    /// The bytes, copied into 8-byte aligned storage as SPA requires.
    fn aligned(&self) -> Vec<u64> {
        let mut words = vec![0u64; self.0.len().div_ceil(8)];
        // SAFETY: `words` has at least as many bytes as the pod.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.0.as_ptr(),
                words.as_mut_ptr().cast::<u8>(),
                self.0.len(),
            );
        }
        words
    }
}

fn enum_format() -> Pod {
    Pod::object(OBJECT_FORMAT, PARAM_ENUM_FORMAT, |p| {
        p.key(FORMAT_MEDIA_TYPE);
        p.value(TYPE_ID, &[MEDIA_TYPE_VIDEO]);
        p.key(FORMAT_MEDIA_SUBTYPE);
        p.value(TYPE_ID, &[MEDIA_SUBTYPE_RAW]);
        // The modifier comes first: SPA treats its presence as the request for
        // DMA-BUF, and omits it for the shared-memory formats below. The portal
        // reads the pair together, so a format with no modifier means
        // shared memory.
        p.key_flags(FORMAT_VIDEO_MODIFIER, SPA_POD_PROP_FLAG_MANDATORY);
        p.choice_long(
            CHOICE_ENUM,
            &[
                DRM_MOD_LINEAR,
                DRM_MOD_LINEAR,
                DRM_MOD_X_TILED,
                DRM_MOD_Y_TILED,
            ],
        );
        // The format stays a `spa_video_format`: the compositor offers AR24 and
        // AB24 from wlr, and the portal reports those as `BGRx`/`RGBA`-family
        // enums with the modifier carrying the tiling. A DRM fourcc here is a
        // different, unused encoding and never matches.
        p.key(FORMAT_VIDEO_FORMAT);
        p.choice(
            CHOICE_ENUM,
            TYPE_ID,
            1,
            &[VIDEO_BGRA, VIDEO_RGBA, VIDEO_BGRX, VIDEO_RGBX],
        );
        p.key(FORMAT_VIDEO_SIZE);
        p.choice(
            CHOICE_RANGE,
            TYPE_RECTANGLE,
            2,
            &[1920, 1080, 1, 1, 16384, 16384],
        );
        p.key(FORMAT_VIDEO_FRAMERATE);
        p.choice(CHOICE_RANGE, TYPE_FRACTION, 2, &[60, 1, 0, 1, 1000, 1]);
    })
}

/// The shared-memory format offer, used when the compositor will not do DMA-BUF.
fn enum_format_shm() -> Pod {
    Pod::object(OBJECT_FORMAT, PARAM_ENUM_FORMAT, |p| {
        p.key(FORMAT_MEDIA_TYPE);
        p.value(TYPE_ID, &[MEDIA_TYPE_VIDEO]);
        p.key(FORMAT_MEDIA_SUBTYPE);
        p.value(TYPE_ID, &[MEDIA_SUBTYPE_RAW]);
        p.key(FORMAT_VIDEO_FORMAT);
        p.choice(
            CHOICE_ENUM,
            TYPE_ID,
            1,
            &[VIDEO_BGRX, VIDEO_BGRX, VIDEO_BGRA, VIDEO_RGBX, VIDEO_RGBA],
        );
        p.key(FORMAT_VIDEO_SIZE);
        p.choice(
            CHOICE_RANGE,
            TYPE_RECTANGLE,
            2,
            &[1920, 1080, 1, 1, 16384, 16384],
        );
        p.key(FORMAT_VIDEO_FRAMERATE);
        p.choice(CHOICE_RANGE, TYPE_FRACTION, 2, &[60, 1, 0, 1, 1000, 1]);
    })
}

fn buffers() -> Pod {
    Pod::object(OBJECT_PARAM_BUFFERS, PARAM_BUFFERS, |p| {
        p.key(BUFFERS_DATA_TYPE);
        // A flags choice holds one value: the mask. The count `SPA_CHOICE_FLAGS`
        // writes is not part of the pod -- `SPA_POD_CHOICE_N_VALUES` derives it
        // from the body size, which `choice` sets from the value count here.
        p.choice(
            CHOICE_FLAGS,
            TYPE_INT,
            1,
            &[if dmabuf_requested() {
                1 << DATA_MEM_DMABUF
            } else {
                (1 << DATA_MEM_PTR) | (1 << DATA_MEM_FD)
            }],
        );
    })
}

fn crop_meta() -> Pod {
    Pod::object(OBJECT_PARAM_META, PARAM_META, |p| {
        p.key(META_TYPE);
        p.value(TYPE_ID, &[META_VIDEO_CROP]);
        p.key(META_SIZE);
        p.value(TYPE_INT, &[size_of::<SpaMetaRegion>() as u32]);
    })
}

/// The negotiated format from a Format object.
///
/// `modifier` is present only when the compositor agreed to DMA-BUF; a shared
/// memory stream leaves it `None`. It is what decides which path the frame
/// takes, not the format code, because a DRM fourcc can also arrive over
/// shared memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct NegotiatedFormat {
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub modifier: Option<u64>,
}

/// The negotiated format from a Format object.
///
/// # Safety
/// `pod` must point at a valid SPA pod.
unsafe fn parse_format(pod: *const c_void) -> Option<NegotiatedFormat> {
    let read =
        |offset: usize| unsafe { pod.cast::<u8>().add(offset).cast::<u32>().read_unaligned() };
    let read64 =
        |offset: usize| unsafe { pod.cast::<u8>().add(offset).cast::<u64>().read_unaligned() };
    let size = read(0) as usize;
    if read(4) != TYPE_OBJECT || size < 8 {
        return None;
    }
    let end = 8 + size;
    let mut offset = 16;
    let (mut format, mut width, mut height) = (None, None, None);
    let mut modifier = None;
    while offset + 16 <= end {
        let key = read(offset);
        let value_size = read(offset + 8) as usize;
        let mut value_type = read(offset + 12);
        let mut value = offset + 16;
        if value_type == TYPE_CHOICE {
            // A fixed property may still come wrapped in a None choice; its
            // first element is the value.
            value_type = read(value + 12);
            value += 16;
        }
        match (key, value_type) {
            (FORMAT_VIDEO_FORMAT, TYPE_ID) => format = Some(read(value)),
            (FORMAT_VIDEO_MODIFIER, TYPE_LONG) => modifier = Some(read64(value)),
            (FORMAT_VIDEO_SIZE, TYPE_RECTANGLE) => {
                width = Some(read(value));
                height = Some(read(value + 4));
            }
            _ => {}
        }
        offset += 16 + value_size.next_multiple_of(8);
    }
    Some(NegotiatedFormat {
        format: format?,
        width: width?,
        height: height?,
        modifier,
    })
}

fn dmabuf_requested() -> bool {
    std::env::var("OPENUUYC_CAPTURE_DMABUF").is_ok_and(|value| value != "0")
}

/// Import into the baseline's owned BGRA picture while the PipeWire buffer is
/// dequeued. No compositor buffer or fd is retained by the encoder/cache.
fn read_dmabuf_picture(
    buffer: &SpaBuffer,
    data: &SpaData,
    format: NegotiatedFormat,
    target: Option<(i32, (u32, u32))>,
) -> Result<Picture> {
    ensure!(
        buffer.n_datas == 1 && data.fd >= 0 && !data.chunk.is_null(),
        "DMA-BUF 要求一个有效 RGB 平面"
    );
    let chunk = unsafe { &*data.chunk };
    ensure!(
        chunk.stride > 0 && format.width > 0 && format.height > 0,
        "DMA-BUF 尺寸或 stride 无效"
    );
    let size = unsafe { libc::lseek(data.fd as i32, 0, libc::SEEK_END) };
    let size = u32::try_from(size).context("读取 DMA-BUF 真实大小失败")?;
    ensure!(size > 0, "空 DMA-BUF");
    let (va_fourcc, drm_fourcc) = match format.format {
        VIDEO_BGRA => (cros_libva::VA_FOURCC_BGRA, u32::from_le_bytes(*b"AR24")),
        VIDEO_BGRX => (cros_libva::VA_FOURCC_BGRX, u32::from_le_bytes(*b"XR24")),
        VIDEO_RGBA => (cros_libva::VA_FOURCC_RGBA, u32::from_le_bytes(*b"AB24")),
        VIDEO_RGBX => (cros_libva::VA_FOURCC_RGBX, u32::from_le_bytes(*b"XB24")),
        _ => bail!("不支持的 DMA-BUF RGB 格式"),
    };
    thread_local! {
        static IMPORT: std::cell::OnceCell<(std::rc::Rc<cros_libva::Display>, bool)> = const { std::cell::OnceCell::new() };
    }
    IMPORT.with(|cell| {
        if cell.get().is_none() {
            super::super::cuda::initialize_before_va();
            let display = cros_libva::Display::open().context("找不到 VA-API render node")?;
            let mut config = display
                .create_config(
                    Vec::new(),
                    cros_libva::VAProfile::VAProfileNone,
                    cros_libva::VAEntrypoint::VAEntrypointVideoProc,
                )
                .map_err(|e| anyhow!(e.to_string()))?;
            let types = config
                .query_surface_attributes_by_type(
                    cros_libva::VASurfaceAttribType::VASurfaceAttribMemoryType,
                )
                .map_err(|e| anyhow!(e.to_string()))?
                .iter()
                .fold(0u32, |mask, value| match value {
                    cros_libva::GenericValue::Integer(value) => mask | *value as u32,
                    _ => mask,
                });
            let prime2 = types & cros_libva::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 != 0;
            ensure!(
                prime2 || types & cros_libva::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME != 0,
                "驱动不支持 PRIME 导入"
            );
            tracing::info!(prime2, types, "DMA-BUF 基线采集导入类型已选择");
            drop(config);
            let _ = cell.set((display, prime2));
        }
        let (display, prime2) = cell.get().expect("initialized above");
        let modifier = format.modifier.context("DMA-BUF 缺少 modifier")?;
        ensure!(modifier != DRM_MOD_INVALID, "DMA-BUF modifier 未确定");
        // Experimental independent shader path, never a compositor lease.
        // Leave the proven DMA capture baseline available during validation.
        thread_local! {
            static GPU_FAILED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        }
        if std::env::var("OPENUUYC_GPU_CONVERT").as_deref() == Ok("1")
            && !GPU_FAILED.with(|failed|failed.get()) {

            thread_local! {
                static GPU: std::cell::OnceCell<Result<super::super::gpu_convert::Converter, String>> = const { std::cell::OnceCell::new() };
            }
            let crop = picture_crop(buffer, format.width, format.height);
            // The converter writes the final geometry directly, so a controller
            // quality that differs from the source is scaled on the GPU rather
            // than read back to the CPU. Without a request the cropped source
            // size is kept, which is the exact pass the baseline used.
            let target = match target {
                Some((quality, maximum)) => {
                    let size = crate::media::geometry::output_size(crop.2, crop.3, quality);
                    crate::media::geometry::fit_size(size.0, size.1, maximum)
                }
                None => (crop.2, crop.3),
            };
            let result = GPU.with(|cell| {
                let converter=cell.get_or_init(||super::super::gpu_convert::Converter::new(display.clone()).map_err(|e|e.to_string()));
                match converter {
                    Ok(converter)=>converter.convert(&super::super::gpu_convert::RgbDma { fd:data.fd as i32,width:format.width,height:format.height,pitch:chunk.stride as u32,offset:chunk.offset,fourcc:drm_fourcc,modifier },crop,target),
                    Err(e)=>Err(anyhow!("{e}")),
                }
            });
            match result {
                Ok(frame)=>return Ok(Picture { width:crop.2,height:crop.3,pixels:vec![],nv12:Some(frame) }),
                Err(error)=> {
                    GPU_FAILED.with(|failed|failed.set(true));
                    tracing::warn!(%error,"独立 GPU 转换不可用，保持 DMA-BUF 的基线采集路径");
                },
            }
        }
        let descriptor = CaptureImport {
            va_fourcc,
            drm_fourcc,
            width: format.width,
            height: format.height,
            size,
            fd: data.fd as i32,
            offset: chunk.offset,
            pitch: chunk.stride as u32,
            modifier,
            prime2: *prime2,
        };
        let surfaces = display
            .create_surfaces(
                cros_libva::VA_RT_FORMAT_RGB32,
                Some(va_fourcc),
                format.width,
                format.height,
                None,
                vec![descriptor],
            )
            .map_err(|e| anyhow!(e.to_string()))?;
        let image = cros_libva::Image::derive_from(&surfaces[0], (format.width, format.height))
            .map_err(|e| anyhow!(e.to_string()))?;
        let info = image.image();
        let pixel_format = match info.format.fourcc {
            cros_libva::VA_FOURCC_BGRA | cros_libva::VA_FOURCC_BGRX => VIDEO_BGRA,
            cros_libva::VA_FOURCC_RGBA | cros_libva::VA_FOURCC_RGBX => VIDEO_RGBA,
            _ => bail!("导入后未提供 packed RGB 图像"),
        };
        copy_picture_pixels(
            buffer,
            image.as_ref(),
            info.offsets[0] as usize,
            info.pitches[0] as usize,
            pixel_format,
            format.width,
            format.height,
        )
        .context("导入图像的尺寸或裁剪越界")
    })
}

/// The effective uncommitted PRIME fix: choose the supported import API,
/// map DRM/VA byte orders correctly, and keep descriptor storage alive.
struct CaptureImport {
    va_fourcc: u32,
    drm_fourcc: u32,
    width: u32,
    height: u32,
    size: u32,
    fd: i32,
    offset: u32,
    pitch: u32,
    modifier: u64,
    prime2: bool,
}
impl cros_libva::SurfaceMemoryDescriptor for CaptureImport {
    fn add_attrs(
        &mut self,
        attrs: &mut Vec<cros_libva::VASurfaceAttrib>,
    ) -> Option<Box<dyn std::any::Any>> {
        let mut memory =
            cros_libva::VASurfaceAttrib::new_memory_type(cros_libva::MemoryType::DrmPrime2);
        if self.prime2 {
            let mut descriptor: Box<cros_libva::VADRMPRIMESurfaceDescriptor> = Box::default();
            descriptor.fourcc = self.va_fourcc;
            descriptor.width = self.width;
            descriptor.height = self.height;
            descriptor.num_objects = 1;
            descriptor.objects[0].fd = self.fd;
            descriptor.objects[0].size = self.size;
            descriptor.objects[0].drm_format_modifier = self.modifier;
            descriptor.num_layers = 1;
            descriptor.layers[0].drm_format = self.drm_fourcc;
            descriptor.layers[0].num_planes = 1;
            descriptor.layers[0].offset[0] = self.offset;
            descriptor.layers[0].pitch[0] = self.pitch;
            attrs.push(memory);
            attrs.push(cros_libva::VASurfaceAttrib::new_buffer_descriptor(
                &mut *descriptor,
            ));
            Some(descriptor)
        } else {
            let fds = Box::new([self.fd as usize]);
            let mut descriptor: Box<cros_libva::VASurfaceAttribExternalBuffers> = Box::default();
            descriptor.pixel_format = self.va_fourcc;
            descriptor.width = self.width;
            descriptor.height = self.height;
            descriptor.data_size = self.size;
            descriptor.num_planes = 1;
            descriptor.pitches[0] = self.pitch;
            descriptor.offsets[0] = self.offset;
            descriptor.buffers = fds.as_ptr() as *mut usize;
            descriptor.num_buffers = 1;
            memory.value.value.i = cros_libva::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME as i32;
            attrs.push(memory);
            attrs.push(cros_libva::VASurfaceAttrib::new_buffer_descriptor(
                &mut *descriptor,
            ));
            Some(Box::new((descriptor, fds)))
        }
    }
}

// --- Stream ----------------------------------------------------------------------

#[derive(Default)]
struct State {
    picture: Option<Picture>,
    sequence: u64,
    format: Option<NegotiatedFormat>,
    streaming: bool,
    error: Option<String>,
    buffers_sent: bool,
    /// Output geometry request from the capture worker: `(quality, maximum)`.
    /// The GPU converter derives the final size from the picture's own
    /// dimensions, exactly as the CPU path's `output()` does, so a controller
    /// quality that differs from the source is scaled on the GPU instead of by
    /// a CPU readback. `None` keeps the native size.
    target: Option<(i32, (u32, u32))>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

/// What the loop thread's callbacks reach through their `data` pointer.
struct Callbacks {
    api: &'static Api,
    stream: *mut c_void,
    shared: Arc<Shared>,
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

unsafe extern "C" fn on_state_changed(
    data: *mut c_void,
    _old: c_int,
    state: c_int,
    error: *const c_char,
) {
    let callbacks = unsafe { &*data.cast::<Callbacks>() };
    let mut guard = lock(&callbacks.shared.state);
    match state {
        STATE_STREAMING => guard.streaming = true,
        STATE_ERROR => {
            let text = if error.is_null() {
                "未知错误".to_owned()
            } else {
                unsafe { CStr::from_ptr(error) }
                    .to_string_lossy()
                    .into_owned()
            };
            guard.error = Some(format!("PipeWire 画面流出错：{text}"));
        }
        STATE_UNCONNECTED if guard.streaming => {
            guard.error = Some("屏幕共享已结束".into());
        }
        _ => {}
    }
    drop(guard);
    callbacks.shared.changed.notify_all();
}

unsafe extern "C" fn on_param_changed(data: *mut c_void, id: u32, param: *const c_void) {
    if param.is_null() || id != PARAM_FORMAT {
        return;
    }
    let callbacks = unsafe { &*data.cast::<Callbacks>() };
    let format = unsafe { parse_format(param) };
    let mut state = lock(&callbacks.shared.state);
    state.format = format;
    if state.buffers_sent {
        return;
    }
    state.buffers_sent = true;
    drop(state);
    // The portal must fixate the format before it receives a buffer request.
    // Submit allocation and metadata parameters once after Format is received.
    let buffers = buffers().aligned();
    let meta = crop_meta().aligned();
    let mut params = [
        buffers.as_ptr().cast::<c_void>(),
        meta.as_ptr().cast::<c_void>(),
    ];
    unsafe {
        (callbacks.api.stream_update_params)(
            callbacks.stream,
            params.as_mut_ptr(),
            params.len() as u32,
        )
    };
    tracing::debug!(?format, "PipeWire screencast format negotiated");
}

unsafe extern "C" fn on_process(data: *mut c_void) {
    let callbacks = unsafe { &*data.cast::<Callbacks>() };
    let api = callbacks.api;
    // Only the newest buffer matters; return any older ones unread.
    let mut newest: *mut PwBuffer = std::ptr::null_mut();
    loop {
        let buffer = unsafe { (api.stream_dequeue_buffer)(callbacks.stream) };
        if buffer.is_null() {
            break;
        }
        if !newest.is_null() {
            unsafe { (api.stream_queue_buffer)(callbacks.stream, newest) };
        }
        newest = buffer;
    }
    if newest.is_null() {
        return;
    }
    let (format, target) = {
        let state = lock(&callbacks.shared.state);
        (state.format, state.target)
    };
    let picture =
        format.and_then(|format| unsafe { read_picture((*newest).buffer, format, target) });
    unsafe { (api.stream_queue_buffer)(callbacks.stream, newest) };
    if let Some(picture) = picture {
        let mut state = lock(&callbacks.shared.state);
        state.picture = Some(picture);
        state.sequence += 1;
        drop(state);
        callbacks.shared.changed.notify_all();
    }
}

/// Copy a buffer's pixels out as tightly packed BGRA with opaque alpha.
///
/// # Safety
/// `buffer` must be a buffer the stream just dequeued.
unsafe fn read_picture(
    buffer: *mut SpaBuffer,
    negotiated: NegotiatedFormat,
    target: Option<(i32, (u32, u32))>,
) -> Option<Picture> {
    let NegotiatedFormat {
        format,
        width,
        height,
        modifier: _,
    } = negotiated;
    let buffer = unsafe { &*buffer };
    if buffer.n_datas == 0 || buffer.datas.is_null() {
        return None;
    }
    let data = unsafe { &*buffer.datas };
    if data.kind == DATA_MEM_DMABUF {
        return match read_dmabuf_picture(buffer, data, negotiated, target) {
            Ok(picture) => Some(picture),
            Err(error) => {
                tracing::warn!(%error, "DMA-BUF 导入失败");
                None
            }
        };
    }
    if data.data.is_null() || data.chunk.is_null() {
        // A DMA-BUF was not mapped for the CPU. Asking the portal for one does
        // not help: Hyprland's ScreenCast offers only MemFd, so this stays the
        // path a GPU encoder would need to import through PipeWire directly.
        tracing::debug!(data_type = data.kind, "PipeWire buffer is not CPU-mappable");
        return None;
    }
    let chunk = unsafe { &*data.chunk };
    // The type the portal actually chose, which is the only way to tell whether
    // a DMA-BUF request was honoured or quietly answered with shared memory.
    tracing::debug!(
        data_type = data.kind,
        maxsize = data.maxsize,
        "PipeWire buffer delivered"
    );
    let stride = if chunk.stride > 0 {
        chunk.stride as usize
    } else {
        width as usize * 4
    };
    let base = (chunk.offset % data.maxsize.max(1)) as usize;
    let source =
        unsafe { std::slice::from_raw_parts(data.data.cast::<u8>(), data.maxsize as usize) };
    copy_picture_pixels(buffer, source, base, stride, format, width, height)
}

fn copy_picture_pixels(
    buffer: &SpaBuffer,
    source: &[u8],
    base: usize,
    stride: usize,
    format: u32,
    width: u32,
    height: u32,
) -> Option<Picture> {
    let (x, y, w, h) = picture_crop(buffer, width, height);
    let (x, y, w, h) = (x as usize, y as usize, w as usize, h as usize);
    let needed = base + (y + h).saturating_sub(1) * stride + (x + w) * 4;
    if w == 0 || h == 0 || needed > source.len() {
        return None;
    }
    copy_cropped_pixels(source, base, stride, format, x, y, w, h)
}

fn picture_crop(buffer: &SpaBuffer, width: u32, height: u32) -> (u32, u32, u32, u32) {
    let (mut x, mut y, mut w, mut h) = (0usize, 0usize, width as usize, height as usize);
    for index in 0..buffer.n_metas as usize {
        let meta = unsafe { &*buffer.metas.add(index) };
        if meta.kind == META_VIDEO_CROP
            && meta.size as usize >= size_of::<SpaMetaRegion>()
            && !meta.data.is_null()
        {
            let region = unsafe { &*meta.data.cast::<SpaMetaRegion>() };
            if region.width > 0 && region.height > 0 {
                x = region.x.max(0) as usize;
                y = region.y.max(0) as usize;
                w = (region.width as usize).min(width as usize - x.min(width as usize));
                h = (region.height as usize).min(height as usize - y.min(height as usize));
            }
        }
    }
    (x as u32, y as u32, w as u32, h as u32)
}

fn copy_cropped_pixels(
    source: &[u8],
    base: usize,
    stride: usize,
    format: u32,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
) -> Option<Picture> {
    // Only the encoders read these pixels, and both convert with the colour
    // channels alone: the alpha byte is never looked at. Writing it would cost
    // a pass over the frame for nothing, so the buffer is left as the
    // compositor sent it.
    let swap = matches!(format, VIDEO_RGBX | VIDEO_RGBA);
    let mut pixels = vec![0u8; w * h * 4];
    if !swap {
        // The common case is already BGRX, so each row is a straight copy.
        for row in 0..h {
            let start = base + (y + row) * stride + x * 4;
            pixels[row * w * 4..(row + 1) * w * 4].copy_from_slice(&source[start..start + w * 4]);
        }
    } else {
        // Swap red and blue while copying, so the result is BGRA either way.
        for row in 0..h {
            let start = base + (y + row) * stride + x * 4;
            let source_row = &source[start..start + w * 4];
            let target = &mut pixels[row * w * 4..(row + 1) * w * 4];
            for (texel, out) in source_row.chunks_exact(4).zip(target.chunks_exact_mut(4)) {
                out[0] = texel[2];
                out[1] = texel[1];
                out[2] = texel[0];
                out[3] = texel[3];
            }
        }
    }
    Some(Picture {
        width: w as u32,
        height: h as u32,
        pixels,
        nv12: None,
    })
}

/// A video stream from one PipeWire node, on its own loop thread.
pub(super) struct Stream {
    api: &'static Api,
    thread_loop: *mut c_void,
    context: *mut c_void,
    core: *mut c_void,
    stream: *mut c_void,
    _hook: Box<SpaHook>,
    _events: Box<StreamEvents>,
    callbacks: *mut Callbacks,
    shared: Arc<Shared>,
    delivered: u64,
}

// The raw handles are only used under the thread loop's lock or after it stopped.
unsafe impl Send for Stream {}

impl Stream {
    /// Connect to `node` on the PipeWire remote behind `fd` (from the portal).
    pub fn connect(fd: std::os::fd::OwnedFd, node: u32) -> Result<Self> {
        use std::os::fd::IntoRawFd;
        let api = api()?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let name = CString::new("openuuyc-capture")?;
        // SAFETY: the PipeWire calls follow the documented setup sequence; every
        // handle is checked and owned by the returned value, which tears them
        // down in reverse order.
        unsafe {
            let thread_loop = (api.thread_loop_new)(name.as_ptr(), std::ptr::null());
            ensure!(!thread_loop.is_null(), "无法创建 PipeWire 线程");
            let context = (api.context_new)(
                (api.thread_loop_get_loop)(thread_loop),
                std::ptr::null_mut(),
                0,
            );
            if context.is_null() {
                (api.thread_loop_destroy)(thread_loop);
                bail!("无法创建 PipeWire 上下文");
            }
            let mut this = Self {
                api,
                thread_loop,
                context,
                core: std::ptr::null_mut(),
                stream: std::ptr::null_mut(),
                _hook: Box::new(std::mem::zeroed()),
                _events: Box::new(StreamEvents {
                    version: STREAM_EVENTS_VERSION,
                    destroy: None,
                    state_changed: Some(on_state_changed),
                    control_info: None,
                    io_changed: None,
                    param_changed: Some(on_param_changed),
                    add_buffer: None,
                    remove_buffer: None,
                    process: Some(on_process),
                    drained: None,
                    command: None,
                    trigger_done: None,
                }),
                callbacks: std::ptr::null_mut(),
                shared: shared.clone(),
                delivered: 0,
            };
            ensure!(
                (api.thread_loop_start)(thread_loop) >= 0,
                "无法启动 PipeWire 线程"
            );
            (api.thread_loop_lock)(thread_loop);
            let result = (|| -> Result<()> {
                this.core =
                    (api.context_connect_fd)(context, fd.into_raw_fd(), std::ptr::null_mut(), 0);
                ensure!(!this.core.is_null(), "无法连接屏幕共享的 PipeWire 远端");
                let key = |s: &str| CString::new(s).unwrap();
                let (media_type, video, category, capture, role, screen) = (
                    key("media.type"),
                    key("Video"),
                    key("media.category"),
                    key("Capture"),
                    key("media.role"),
                    key("Screen"),
                );
                let props = (api.properties_new)(
                    media_type.as_ptr(),
                    video.as_ptr(),
                    category.as_ptr(),
                    capture.as_ptr(),
                    role.as_ptr(),
                    screen.as_ptr(),
                    std::ptr::null::<c_char>(),
                );
                this.stream = (api.stream_new)(this.core, name.as_ptr(), props);
                ensure!(!this.stream.is_null(), "无法创建 PipeWire 画面流");
                this.callbacks = Box::into_raw(Box::new(Callbacks {
                    api,
                    stream: this.stream,
                    shared,
                }));
                (api.stream_add_listener)(
                    this.stream,
                    &mut *this._hook,
                    &*this._events,
                    this.callbacks.cast(),
                );
                let format = if dmabuf_requested() {
                    enum_format()
                } else {
                    enum_format_shm()
                }
                .aligned();
                let mut params = [format.as_ptr().cast::<c_void>()];
                let connected = (api.stream_connect)(
                    this.stream,
                    DIRECTION_INPUT,
                    node,
                    FLAG_AUTOCONNECT,
                    params.as_mut_ptr(),
                    params.len() as u32,
                );
                ensure!(connected >= 0, "连接 PipeWire 画面流失败：{connected}");
                Ok(())
            })();
            (api.thread_loop_unlock)(thread_loop);
            result.map(|()| this)
        }
    }

    /// Set the output geometry request. The worker calls this when the
    /// negotiated quality changes, so a mismatch is handled on the GPU instead
    /// of by a CPU readback and scale.
    pub fn set_target(&self, target: Option<(i32, (u32, u32))>) {
        let mut state = lock(&self.shared.state);
        state.target = target;
    }

    /// The next picture newer than the last one returned, waiting up to
    /// `timeout`. `None` when nothing new arrived.
    pub fn next(&mut self, timeout: Duration) -> Result<Option<Picture>> {
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.shared.state);
        loop {
            if let Some(error) = &state.error {
                bail!("{error}");
            }
            if state.sequence != self.delivered
                && let Some(picture) = state.picture.take()
            {
                self.delivered = state.sequence;
                return Ok(Some(picture));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let api = self.api;
        // SAFETY: teardown in reverse order of setup; the loop is stopped
        // before the callback data it could still call into is freed.
        unsafe {
            (api.thread_loop_lock)(self.thread_loop);
            if !self.stream.is_null() {
                (api.stream_disconnect)(self.stream);
                (api.stream_destroy)(self.stream);
            }
            if !self.core.is_null() {
                (api.core_disconnect)(self.core);
            }
            (api.thread_loop_unlock)(self.thread_loop);
            (api.thread_loop_stop)(self.thread_loop);
            (api.context_destroy)(self.context);
            (api.thread_loop_destroy)(self.thread_loop);
            if !self.callbacks.is_null() {
                drop(Box::from_raw(self.callbacks));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_format_parses_back() {
        // A fixed format written the way enum_format writes a choice's first
        // element must be read by parse_format.
        let pod = Pod::object(OBJECT_FORMAT, PARAM_FORMAT, |p| {
            p.key(FORMAT_MEDIA_TYPE);
            p.value(TYPE_ID, &[MEDIA_TYPE_VIDEO]);
            p.key(FORMAT_VIDEO_FORMAT);
            p.value(TYPE_ID, &[VIDEO_BGRX]);
            p.key(FORMAT_VIDEO_SIZE);
            p.value(TYPE_RECTANGLE, &[2560, 1440]);
        })
        .aligned();
        assert_eq!(
            unsafe { parse_format(pod.as_ptr().cast()) },
            Some(NegotiatedFormat {
                format: VIDEO_BGRX,
                width: 2560,
                height: 1440,
                modifier: None
            })
        );
        let choices = enum_format().aligned();
        let size = unsafe { choices.as_ptr().cast::<u32>().read() } as usize;
        assert_eq!((size + 8) % 8, 0);
    }

    #[test]
    fn imported_rgb_preserves_baseline_crop_channel_order_and_bounds() {
        let mut region = SpaMetaRegion {
            x: 1,
            y: 1,
            width: 1,
            height: 1,
        };
        let mut meta = SpaMeta {
            kind: META_VIDEO_CROP,
            size: size_of::<SpaMetaRegion>() as u32,
            data: (&mut region as *mut SpaMetaRegion).cast(),
        };
        let buffer = SpaBuffer {
            n_metas: 1,
            n_datas: 0,
            metas: &mut meta,
            datas: std::ptr::null_mut(),
        };
        let source = [
            0, 0, 0, 0, 0, 0, 0, 0, 9, 9, 9, 9, 0, 0, 0, 0, 10, 20, 30, 255,
        ];
        let picture = copy_picture_pixels(&buffer, &source, 0, 12, VIDEO_RGBA, 2, 2).unwrap();
        assert_eq!((picture.width, picture.height), (1, 1));
        assert_eq!(picture.pixels, vec![30, 20, 10, 255]);
        assert!(copy_picture_pixels(&buffer, &source[..19], 0, 12, VIDEO_RGBA, 2, 2).is_none());
    }

    #[test]
    fn buffers_offers_dmabuf_in_one_flags_choice() {
        // The portal only offers DMA-BUF when the mask reaches it intact, and
        // the value count is derived from the body size rather than stored, so
        // a wrong word count silently produces a different number of values.
        let pod = buffers().aligned();
        let words: Vec<u32> = (0..14)
            .map(|index| unsafe { pod.as_ptr().cast::<u32>().add(index).read() })
            .collect();
        // [size, TYPE_OBJECT, ParamBuffers, PARAM_BUFFERS, key, pad,
        //  choice size, TYPE_CHOICE, CHOICE_FLAGS, 0, child size, TYPE_INT, mask]
        assert_eq!(words[1], TYPE_OBJECT);
        assert_eq!(words[2], OBJECT_PARAM_BUFFERS);
        assert_eq!(words[3], PARAM_BUFFERS);
        assert_eq!(words[4], BUFFERS_DATA_TYPE);
        assert_eq!(words[7], TYPE_CHOICE);
        assert_eq!(words[8], CHOICE_FLAGS);
        assert_eq!(words[11], TYPE_INT);
        let mask = words[12];
        let expected = if dmabuf_requested() {
            1 << DATA_MEM_DMABUF
        } else {
            (1 << DATA_MEM_PTR) | (1 << DATA_MEM_FD)
        };
        assert_eq!(mask, expected);
    }
}
