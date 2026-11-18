//! Desktop sound capture from a playback device's monitor, over the PulseAudio
//! client API as Sunshine does. It is served by PulseAudio itself and by
//! PipeWire's pulse server alike.
//!
//! `libpulse.so.0` is opened at run time, so machines without a sound server
//! report why there is no desktop sound instead of failing to start. The
//! monitor is recorded as 48 kHz float stereo: the server resamples and
//! downmixes, and this side only cuts it into 10-ms blocks on the capture
//! clock, as the Windows loopback does.
use crate::media::audio::sender::{BLOCK, RATE};
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub(crate) type Samples = Arc<dyn Fn([f32; BLOCK * 2], Instant) + Send + Sync>;

// --- libpulse ABI -------------------------------------------------------------

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}
#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}
#[repr(C)]
struct ChannelMap {
    channels: u8,
    map: [c_int; 32],
}
#[repr(C)]
struct CVolume {
    channels: u8,
    values: [u32; 32],
}
/// The leading fields of `pa_sink_info`; libpulse owns the rest.
#[repr(C)]
struct SinkInfo {
    name: *const c_char,
    index: u32,
    description: *const c_char,
    sample_spec: SampleSpec,
    channel_map: ChannelMap,
    owner_module: u32,
    volume: CVolume,
    mute: c_int,
    monitor_source: u32,
    monitor_source_name: *const c_char,
}
/// The leading fields of `pa_server_info`.
#[repr(C)]
struct ServerInfo {
    user_name: *const c_char,
    host_name: *const c_char,
    server_version: *const c_char,
    server_name: *const c_char,
    sample_spec: SampleSpec,
    default_sink_name: *const c_char,
}

const SAMPLE_FLOAT32LE: c_int = 5;
const CONTEXT_NOAUTOSPAWN: c_int = 1;
const CONTEXT_READY: c_int = 4;
const CONTEXT_FAILED: c_int = 5;
const CONTEXT_TERMINATED: c_int = 6;
const STREAM_READY: c_int = 2;
const STREAM_FAILED: c_int = 3;
const STREAM_TERMINATED: c_int = 4;
const STREAM_DONT_MOVE: c_int = 0x200;
const STREAM_ADJUST_LATENCY: c_int = 0x2000;
const OPERATION_RUNNING: c_int = 0;
const SUBSCRIPTION_MASK_SINK: u32 = 0x1;
const SUBSCRIPTION_MASK_SERVER: u32 = 0x80;
const EVENT_FACILITY_MASK: c_int = 0xf;
const EVENT_SINK: c_int = 0;
const EVENT_SERVER: c_int = 7;
const EVENT_TYPE_MASK: c_int = 0x30;
const EVENT_NEW: c_int = 0;
const EVENT_REMOVE: c_int = 0x20;

type SinkCallback = unsafe extern "C" fn(*mut c_void, *const SinkInfo, c_int, *mut c_void);
type ServerCallback = unsafe extern "C" fn(*mut c_void, *const ServerInfo, *mut c_void);
type SubscribeCallback = unsafe extern "C" fn(*mut c_void, c_int, u32, *mut c_void);

struct Api {
    _library: libloading::Library,
    mainloop_new: unsafe extern "C" fn() -> *mut c_void,
    mainloop_get_api: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    mainloop_free: unsafe extern "C" fn(*mut c_void),
    mainloop_prepare: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    mainloop_poll: unsafe extern "C" fn(*mut c_void) -> c_int,
    mainloop_dispatch: unsafe extern "C" fn(*mut c_void) -> c_int,
    context_new: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    context_connect:
        unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *const c_void) -> c_int,
    context_get_state: unsafe extern "C" fn(*mut c_void) -> c_int,
    context_errno: unsafe extern "C" fn(*mut c_void) -> c_int,
    context_disconnect: unsafe extern "C" fn(*mut c_void),
    context_unref: unsafe extern "C" fn(*mut c_void),
    context_get_server_info:
        unsafe extern "C" fn(*mut c_void, ServerCallback, *mut c_void) -> *mut c_void,
    context_get_sink_info_list:
        unsafe extern "C" fn(*mut c_void, SinkCallback, *mut c_void) -> *mut c_void,
    context_get_sink_info_by_name:
        unsafe extern "C" fn(*mut c_void, *const c_char, SinkCallback, *mut c_void) -> *mut c_void,
    context_set_subscribe_callback:
        unsafe extern "C" fn(*mut c_void, Option<SubscribeCallback>, *mut c_void),
    context_subscribe:
        unsafe extern "C" fn(*mut c_void, u32, *const c_void, *mut c_void) -> *mut c_void,
    operation_get_state: unsafe extern "C" fn(*mut c_void) -> c_int,
    operation_cancel: unsafe extern "C" fn(*mut c_void),
    operation_unref: unsafe extern "C" fn(*mut c_void),
    stream_new: unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        *const SampleSpec,
        *const c_void,
    ) -> *mut c_void,
    stream_connect_record:
        unsafe extern "C" fn(*mut c_void, *const c_char, *const BufferAttr, c_int) -> c_int,
    stream_get_state: unsafe extern "C" fn(*mut c_void) -> c_int,
    stream_readable_size: unsafe extern "C" fn(*mut c_void) -> usize,
    stream_peek: unsafe extern "C" fn(*mut c_void, *mut *const c_void, *mut usize) -> c_int,
    stream_drop: unsafe extern "C" fn(*mut c_void) -> c_int,
    stream_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int,
    stream_unref: unsafe extern "C" fn(*mut c_void),
    strerror: unsafe extern "C" fn(c_int) -> *const c_char,
}

impl Api {
    fn get() -> Result<&'static Self> {
        static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
        API.get_or_init(|| unsafe { Self::load() }.map_err(|error| format!("{error:#}")))
            .as_ref()
            .map_err(|error| anyhow!("{error}"))
    }

    unsafe fn load() -> Result<Self> {
        unsafe {
            let library = libloading::Library::new("libpulse.so.0")
                .context("没有找到 PulseAudio 客户端库（libpulse.so.0）")?;
            macro_rules! symbol {
                ($name:literal) => {
                    *library
                        .get(concat!($name, "\0").as_bytes())
                        .context(concat!("libpulse 缺少 ", $name))?
                };
            }
            Ok(Self {
                mainloop_new: symbol!("pa_mainloop_new"),
                mainloop_get_api: symbol!("pa_mainloop_get_api"),
                mainloop_free: symbol!("pa_mainloop_free"),
                mainloop_prepare: symbol!("pa_mainloop_prepare"),
                mainloop_poll: symbol!("pa_mainloop_poll"),
                mainloop_dispatch: symbol!("pa_mainloop_dispatch"),
                context_new: symbol!("pa_context_new"),
                context_connect: symbol!("pa_context_connect"),
                context_get_state: symbol!("pa_context_get_state"),
                context_errno: symbol!("pa_context_errno"),
                context_disconnect: symbol!("pa_context_disconnect"),
                context_unref: symbol!("pa_context_unref"),
                context_get_server_info: symbol!("pa_context_get_server_info"),
                context_get_sink_info_list: symbol!("pa_context_get_sink_info_list"),
                context_get_sink_info_by_name: symbol!("pa_context_get_sink_info_by_name"),
                context_set_subscribe_callback: symbol!("pa_context_set_subscribe_callback"),
                context_subscribe: symbol!("pa_context_subscribe"),
                operation_get_state: symbol!("pa_operation_get_state"),
                operation_cancel: symbol!("pa_operation_cancel"),
                operation_unref: symbol!("pa_operation_unref"),
                stream_new: symbol!("pa_stream_new"),
                stream_connect_record: symbol!("pa_stream_connect_record"),
                stream_get_state: symbol!("pa_stream_get_state"),
                stream_readable_size: symbol!("pa_stream_readable_size"),
                stream_peek: symbol!("pa_stream_peek"),
                stream_drop: symbol!("pa_stream_drop"),
                stream_disconnect: symbol!("pa_stream_disconnect"),
                stream_unref: symbol!("pa_stream_unref"),
                strerror: symbol!("pa_strerror"),
                _library: library,
            })
        }
    }
}

fn text(value: *const c_char) -> Option<String> {
    (!value.is_null())
        .then(|| {
            unsafe { CStr::from_ptr(value) }
                .to_string_lossy()
                .into_owned()
        })
        .filter(|value| !value.is_empty())
}

// --- Connection ---------------------------------------------------------------

/// A client connection driven from the owning thread. Callbacks only run
/// inside `run`, so they may borrow state owned by the caller.
struct Connection {
    api: &'static Api,
    mainloop: *mut c_void,
    context: *mut c_void,
}

impl Connection {
    fn open() -> Result<Self> {
        let api = Api::get()?;
        let mainloop = unsafe { (api.mainloop_new)() };
        ensure!(!mainloop.is_null(), "创建 PulseAudio 事件循环失败");
        let name = c"OpenUUYC";
        let context = unsafe { (api.context_new)((api.mainloop_get_api)(mainloop), name.as_ptr()) };
        let connection = Self {
            api,
            mainloop,
            context,
        };
        ensure!(!context.is_null(), "创建 PulseAudio 连接失败");
        let connected = unsafe {
            (api.context_connect)(
                context,
                std::ptr::null(),
                CONTEXT_NOAUTOSPAWN,
                std::ptr::null(),
            )
        };
        ensure!(connected >= 0, "连接声音服务失败：{}", connection.error());
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match unsafe { (api.context_get_state)(context) } {
                CONTEXT_READY => return Ok(connection),
                CONTEXT_FAILED | CONTEXT_TERMINATED => {
                    bail!("连接声音服务失败：{}", connection.error())
                }
                _ => {}
            }
            ensure!(Instant::now() < deadline, "连接声音服务超时");
            connection.run(Duration::from_millis(100))?;
        }
    }

    fn error(&self) -> String {
        unsafe { text((self.api.strerror)((self.api.context_errno)(self.context))) }
            .unwrap_or_else(|| "未知错误".into())
    }

    fn check(&self) -> Result<()> {
        ensure!(
            unsafe { (self.api.context_get_state)(self.context) } == CONTEXT_READY,
            "与声音服务的连接已断开：{}",
            self.error()
        );
        Ok(())
    }

    /// One round of the event loop, waiting at most `timeout` for events.
    fn run(&self, timeout: Duration) -> Result<()> {
        let timeout = c_int::try_from(timeout.as_micros()).unwrap_or(c_int::MAX);
        unsafe {
            ensure!(
                (self.api.mainloop_prepare)(self.mainloop, timeout) >= 0
                    && (self.api.mainloop_poll)(self.mainloop) >= 0
                    && (self.api.mainloop_dispatch)(self.mainloop) >= 0,
                "声音服务事件循环已停止"
            );
        }
        Ok(())
    }

    /// Runs the loop until `operation` completes. Its callback's user data
    /// must outlive this call.
    fn wait(&self, operation: *mut c_void) -> Result<()> {
        ensure!(!operation.is_null(), "声音服务请求失败：{}", self.error());
        let deadline = Instant::now() + Duration::from_secs(3);
        let result = (|| {
            while unsafe { (self.api.operation_get_state)(operation) } == OPERATION_RUNNING {
                self.check()?;
                ensure!(Instant::now() < deadline, "声音服务请求超时");
                self.run(Duration::from_millis(100))?;
            }
            Ok(())
        })();
        unsafe {
            if result.is_err() {
                (self.api.operation_cancel)(operation);
            }
            (self.api.operation_unref)(operation);
        }
        result
    }

    fn sinks(&self, name: Option<&CStr>) -> Result<Vec<Sink>> {
        unsafe extern "C" fn collect(
            _: *mut c_void,
            info: *const SinkInfo,
            end: c_int,
            data: *mut c_void,
        ) {
            if end != 0 || info.is_null() {
                return;
            }
            let (info, sinks) = unsafe { (&*info, &mut *data.cast::<Vec<Sink>>()) };
            if let (Some(name), Some(monitor)) = (text(info.name), text(info.monitor_source_name)) {
                sinks.push(Sink {
                    index: info.index,
                    description: text(info.description).unwrap_or_else(|| name.clone()),
                    name,
                    monitor,
                });
            }
        }
        let mut sinks = Vec::<Sink>::new();
        let data = (&raw mut sinks).cast();
        let operation = unsafe {
            match name {
                Some(name) => (self.api.context_get_sink_info_by_name)(
                    self.context,
                    name.as_ptr(),
                    collect,
                    data,
                ),
                None => (self.api.context_get_sink_info_list)(self.context, collect, data),
            }
        };
        self.wait(operation)?;
        Ok(sinks)
    }

    fn default_sink(&self) -> Result<Option<String>> {
        unsafe extern "C" fn read(_: *mut c_void, info: *const ServerInfo, data: *mut c_void) {
            if !info.is_null() {
                unsafe {
                    *data.cast::<Option<String>>() = text((*info).default_sink_name);
                }
            }
        }
        let mut name = None::<String>;
        let operation = unsafe {
            (self.api.context_get_server_info)(self.context, read, (&raw mut name).cast())
        };
        self.wait(operation)?;
        Ok(name)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        unsafe {
            if !self.context.is_null() {
                (self.api.context_disconnect)(self.context);
                (self.api.context_unref)(self.context);
            }
            (self.api.mainloop_free)(self.mainloop);
        }
    }
}

struct Sink {
    index: u32,
    name: String,
    description: String,
    monitor: String,
}

// --- Devices ------------------------------------------------------------------

/// A playback device. `id` is the sink name, stable across restarts.
pub(crate) struct Endpoint {
    pub id: String,
    description: String,
    monitor: String,
}
impl Endpoint {
    pub fn name(&self) -> String {
        self.description.clone()
    }
}
#[derive(Default)]
pub(crate) struct Change {
    pub refresh: bool,
    pub rebuild: bool,
}

pub(crate) struct Devices {
    connection: Connection,
    /// Subscription events since the last `changes`, filled during `run`.
    events: Box<RefCell<Vec<(c_int, u32)>>>,
    /// Sink indices of the names handed out, to match removal events.
    indices: RefCell<HashMap<String, u32>>,
}
impl Devices {
    pub fn new() -> Result<Self> {
        unsafe extern "C" fn record(_: *mut c_void, event: c_int, index: u32, data: *mut c_void) {
            let events = unsafe { &*data.cast::<RefCell<Vec<(c_int, u32)>>>() };
            if let Ok(mut events) = events.try_borrow_mut() {
                events.push((event, index));
            }
        }
        let connection = Connection::open()?;
        let events = Box::new(RefCell::new(Vec::new()));
        unsafe {
            (connection.api.context_set_subscribe_callback)(
                connection.context,
                Some(record),
                (&raw const *events).cast_mut().cast(),
            );
            let operation = (connection.api.context_subscribe)(
                connection.context,
                SUBSCRIPTION_MASK_SINK | SUBSCRIPTION_MASK_SERVER,
                std::ptr::null(),
                std::ptr::null_mut(),
            );
            connection.wait(operation)?;
        }
        Ok(Self {
            connection,
            events,
            indices: RefCell::new(HashMap::new()),
        })
    }

    pub fn changes(&self, selected: Option<&str>) -> Change {
        // A broken connection rebuilds; `endpoint` then reports the reason.
        if self
            .connection
            .check()
            .and_then(|()| self.connection.run(Duration::ZERO))
            .is_err()
        {
            return Change {
                refresh: true,
                rebuild: true,
            };
        }
        let selected = selected.and_then(|name| self.indices.borrow().get(name).copied());
        let mut change = Change::default();
        for (event, index) in self.events.borrow_mut().drain(..) {
            match (event & EVENT_FACILITY_MASK, event & EVENT_TYPE_MASK) {
                // The default device may have changed.
                (EVENT_SERVER, _) => change.refresh = true,
                (EVENT_SINK, EVENT_NEW) => change.refresh = true,
                (EVENT_SINK, EVENT_REMOVE) => {
                    change.refresh = true;
                    change.rebuild |= selected == Some(index);
                }
                // Volume and port changes keep the monitor stream valid.
                _ => {}
            }
        }
        change
    }

    pub fn endpoint(&self, selected: Option<&str>) -> Result<Option<Endpoint>> {
        self.connection.check()?;
        let name = match selected {
            Some(name) => name.to_owned(),
            None => match self.connection.default_sink()? {
                Some(name) => name,
                None => return Ok(None),
            },
        };
        let name = CString::new(name).context("播放设备标识无效")?;
        // A missing sink ends the lookup with no entry.
        let sinks = self.connection.sinks(Some(&name))?;
        Ok(sinks.into_iter().next().map(|sink| self.describe(sink)))
    }

    pub fn list(&self) -> Result<Vec<Endpoint>> {
        self.connection.check()?;
        Ok(self
            .connection
            .sinks(None)?
            .into_iter()
            .map(|sink| self.describe(sink))
            .collect())
    }

    fn describe(&self, sink: Sink) -> Endpoint {
        self.indices
            .borrow_mut()
            .insert(sink.name.clone(), sink.index);
        Endpoint {
            id: sink.name,
            description: sink.description,
            monitor: sink.monitor,
        }
    }
}

// --- Capture ------------------------------------------------------------------

pub(crate) struct Capture {
    stream: *mut c_void,
    connection: Connection,
    blocks: Blocks,
    samples: Samples,
    opened: Instant,
    delivered: Option<Instant>,
}

pub(crate) fn open(endpoint: &Endpoint, samples: Samples) -> Result<Capture> {
    let connection = Connection::open()?;
    let api = connection.api;
    let spec = SampleSpec {
        format: SAMPLE_FLOAT32LE,
        rate: RATE,
        channels: 2,
    };
    let name = c"桌面声音";
    let stream =
        unsafe { (api.stream_new)(connection.context, name.as_ptr(), &spec, std::ptr::null()) };
    ensure!(
        !stream.is_null(),
        "创建桌面声音采集失败：{}",
        connection.error()
    );
    let capture = Capture {
        stream,
        connection,
        blocks: Blocks::default(),
        samples,
        opened: Instant::now(),
        delivered: None,
    };
    // One 10-ms block per fragment keeps the server-side latency low.
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: (BLOCK * 2 * size_of::<f32>()) as u32,
    };
    let monitor = CString::new(endpoint.monitor.as_str()).context("播放设备标识无效")?;
    // The stream follows this device only; a device change is decided by the
    // caller, and a removed device fails the stream.
    let connected = unsafe {
        (api.stream_connect_record)(
            stream,
            monitor.as_ptr(),
            &attr,
            STREAM_ADJUST_LATENCY | STREAM_DONT_MOVE,
        )
    };
    ensure!(
        connected >= 0,
        "初始化桌面声音采集失败：{}",
        capture.connection.error()
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match unsafe { (api.stream_get_state)(stream) } {
            STREAM_READY => break,
            STREAM_FAILED | STREAM_TERMINATED => {
                bail!("初始化桌面声音采集失败：{}", capture.connection.error())
            }
            _ => {}
        }
        ensure!(Instant::now() < deadline, "初始化桌面声音采集超时");
        capture.connection.run(Duration::from_millis(100))?;
    }
    Ok(capture)
}

impl Capture {
    pub fn producing(&self) -> bool {
        self.delivered.is_some()
    }

    pub fn poll(&mut self) -> Result<()> {
        let api = self.connection.api;
        self.connection.check()?;
        self.connection.run(Duration::ZERO)?;
        ensure!(
            unsafe { (api.stream_get_state)(self.stream) } == STREAM_READY,
            "桌面声音采集已中断：{}",
            self.connection.error()
        );
        // Bound a single poll, so a malfunctioning server cannot delay revocation.
        for _ in 0..32 {
            let readable = unsafe { (api.stream_readable_size)(self.stream) };
            ensure!(readable != usize::MAX, "读取桌面声音失败");
            if readable == 0 {
                break;
            }
            let mut data = std::ptr::null();
            let mut bytes = 0;
            ensure!(
                unsafe { (api.stream_peek)(self.stream, &mut data, &mut bytes) } >= 0,
                "读取桌面声音失败：{}",
                self.connection.error()
            );
            if bytes == 0 {
                break;
            }
            let now = Instant::now();
            // The oldest queued fragment started this long before now.
            let at = now
                .checked_sub(Duration::from_secs_f64(
                    readable as f64 / (2. * 4. * f64::from(RATE)),
                ))
                .unwrap_or(now);
            if data.is_null() {
                // A hole in the monitor: restart the block clock after it.
                self.blocks.reset();
            } else {
                let samples = unsafe {
                    std::slice::from_raw_parts(data.cast::<f32>(), bytes / size_of::<f32>())
                };
                self.blocks.push(samples, at, &self.samples);
            }
            ensure!(
                unsafe { (api.stream_drop)(self.stream) } >= 0,
                "读取桌面声音失败"
            );
            self.delivered = Some(now);
        }
        // A running sink feeds its monitor even while silent, so a stalled
        // stream cannot leave the status claiming capture is healthy. A
        // suspended sink resumes for its monitor first, which can take
        // seconds on HDMI outputs.
        match self.delivered {
            Some(last) => ensure!(
                last.elapsed() < Duration::from_secs(1),
                "桌面音频缓冲停止交付"
            ),
            None => ensure!(
                self.opened.elapsed() < Duration::from_secs(5),
                "播放设备没有开始工作"
            ),
        }
        Ok(())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            (self.connection.api.stream_disconnect)(self.stream);
            (self.connection.api.stream_unref)(self.stream);
        }
    }
}

/// Cuts interleaved 48 kHz stereo into 10-ms blocks, each stamped with the
/// capture time of its first sample.
#[derive(Default)]
struct Blocks {
    frame: Vec<f32>,
    next: Option<Instant>,
    expected: Option<Instant>,
}
impl Blocks {
    fn reset(&mut self) {
        self.frame.clear();
        self.next = None;
        self.expected = None;
    }

    fn push(&mut self, data: &[f32], at: Instant, output: &Samples) {
        let gap = self.expected.is_some_and(|expected| {
            at.saturating_duration_since(expected)
                .max(expected.saturating_duration_since(at))
                > Duration::from_millis(50)
        });
        if gap {
            self.frame.clear();
        }
        // Anchor each fragment to the capture clock, keeping the partial
        // block's age, so sample counts do not drift from wall time.
        let pending = self.frame.len() as f64 / (2. * f64::from(RATE));
        self.next = Some(
            at.checked_sub(Duration::from_secs_f64(pending))
                .unwrap_or(at),
        );
        self.expected =
            Some(at + Duration::from_secs_f64((data.len() / 2) as f64 / f64::from(RATE)));
        for sample in data {
            self.frame.push(if sample.is_finite() {
                sample.clamp(-1., 1.)
            } else {
                0.
            });
            if self.frame.len() == BLOCK * 2 {
                let timestamp = self.next.unwrap();
                output(self.frame.as_slice().try_into().unwrap(), timestamp);
                self.next = Some(timestamp + Duration::from_millis(10));
                self.frame.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Needs a sound server: `cargo test -- --ignored records_the_default`.
    #[test]
    #[ignore]
    fn records_the_default_playback_device() {
        let devices = Devices::new().unwrap();
        for endpoint in devices.list().unwrap() {
            println!(
                "{} = {} ({})",
                endpoint.id,
                endpoint.name(),
                endpoint.monitor
            );
        }
        let sink = std::env::var("OPENUUYC_TEST_SINK").ok();
        let endpoint = devices.endpoint(sink.as_deref()).unwrap().expect("sink");
        println!("recording {}", endpoint.id);
        let blocks = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(std::sync::Mutex::new(0f32));
        let counter = (blocks.clone(), peak.clone());
        let mut capture = open(
            &endpoint,
            Arc::new(move |samples, _| {
                counter.0.fetch_add(1, Ordering::Relaxed);
                let mut peak = counter.1.lock().unwrap();
                *peak = samples.iter().fold(*peak, |p, s| p.max(s.abs()));
            }),
        )
        .unwrap();
        let started = Instant::now();
        let mut first = None;
        while started.elapsed() < Duration::from_secs(5) {
            capture.poll().unwrap();
            if first.is_none() && capture.producing() {
                first = Some((started.elapsed(), blocks.load(Ordering::Relaxed)));
            }
            let change = devices.changes(Some(&endpoint.id));
            if change.refresh {
                println!("device change rebuild={}", change.rebuild);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let (delay, before) = first.expect("no audio delivered");
        let rate = (blocks.load(Ordering::Relaxed) - before) as f64
            / (started.elapsed() - delay).as_secs_f64();
        println!(
            "first audio after {delay:?}, {rate:.0} blocks/s, peak {}",
            peak.lock().unwrap()
        );
        assert!(
            (90. ..=110.).contains(&rate),
            "expected 100 blocks per second"
        );
    }
}
