//! Per-process diagnostic files with shared, reloadable module filters.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, mpsc},
    thread::JoinHandle,
    time::Duration,
};
use tracing_appender::non_blocking::{ErrorCounter, WorkerGuard};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};
mod files;
pub use files::{MAX_FILE_MIB, MAX_TOTAL_MIB, RETENTION_DAYS};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}
impl Level {
    pub const ALL: [Self; 6] = [
        Self::Off,
        Self::Error,
        Self::Warn,
        Self::Info,
        Self::Debug,
        Self::Trace,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "OFF",
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
            Self::Trace => "TRACE",
        }
    }
    fn directive(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}
pub struct Module {
    pub id: &'static str,
    pub name: &'static str,
    pub targets: &'static [&'static str],
    pub external: bool,
}
macro_rules! modules {
    ($(($id:literal, $name:literal, $external:literal, [$($target:literal),+])),+ $(,)?) => {
        pub const MODULES: &[Module] = &[$(Module {id: $id, name: $name, external: $external, targets: &[$($target),+] }),+];
    };
}
modules! {
    ("app", "界面与设备管理", false, ["openuuyc::application::app", "openuuyc::session::controller"]),
    ("auth", "登录与凭据", false, ["openuuyc::account::auth", "openuuyc::account::login", "openuuyc::account::session_restore"]),
    ("api", "账号接口", false, ["openuuyc::account::api", "openuuyc::account::client", "openuuyc::account::nrd_http", "openuuyc::account::assist"]),
    ("presence", "在线状态", false, ["openuuyc::session::presence", "openuuyc::session::device_session"]),
    ("host", "本机被控", false, ["openuuyc::features::host", "openuuyc_codec::encoder", "openuuyc::media::encoding", "openuuyc::platform::windows::software_encoder", "openuuyc::platform::windows::capture", "openuuyc::platform::windows::encoder", "openuuyc::platform::windows::nvenc", "openuuyc::platform::windows::amf", "openuuyc::platform::windows::qsv", "openuuyc::platform::windows::gdi", "openuuyc::platform::windows::preprocess", "openuuyc::platform::windows::transfer", "openuuyc::platform::windows::gpu_conversion", "openuuyc::platform::windows::cursor", "openuuyc::platform::windows::wol", "openuuyc::platform::linux::capture", "openuuyc::platform::linux::encoder", "openuuyc::platform::linux::software_encoder", "openuuyc::platform::linux::vaapi_encoder", "openuuyc::platform::linux::host_service", "openuuyc::platform::linux::nvenc", "openuuyc::platform::linux::cuda", "openuuyc::platform::linux::cursor_shape", "openuuyc::platform::linux::wol"]),
    ("signal", "信令与协商", false, ["openuuyc::transport::signal"]),
    ("rtc", "实时传输", false, ["openuuyc::transport::rtc", "openuuyc::transport::uu_kcp"]),
    ("recovery", "丢包恢复", false, ["openuuyc::transport::official_receiver", "openuuyc::nack_audit", "openuuyc::transport::rsfec", "openuuyc::transport::ulpfec", "openuuyc::transport::flexfec", "openuuyc::transport::xor_fec"]),
    ("decoder", "视频解码", false, ["openuuyc::media::decoder", "openuuyc::media::decoder_pool", "openuuyc::media::decoder_result", "openuuyc::media::codec_parameters", "openuuyc_codec::decoder", "openuuyc_h264", "openuuyc_av1::bitstream", "openuuyc_av1::decoder", "openuuyc::platform::windows::decoder", "openuuyc::platform::windows::surface", "openuuyc::platform::linux::decoder"]),
    ("viewer", "播放窗口", false, ["openuuyc::application::viewer", "openuuyc::session::controller::windows", "openuuyc::media::video_color", "openuuyc::media::video_format", "openuuyc::platform::windows::video_shader", "openuuyc::platform::windows::swapchain", "openuuyc::platform::linux::video_layer"]),
    ("audio", "音频", false, ["openuuyc::media::audio", "openuuyc::media::microphone"]),
    ("control", "串流设置", false, ["openuuyc::features::stream_control", "openuuyc::features::viewing_settings"]),
    ("keyboard", "键盘输入", false, ["openuuyc::application::viewer::windows_keyboard", "openuuyc::application::viewer::linux_keyboard"]),
    ("mouse", "鼠标与光标", false, ["openuuyc::application::viewer::windows_mouse", "openuuyc::application::viewer::windows_cursor", "openuuyc::features::remote_cursor", "openuuyc::viewer::input"]),
    ("input", "控制发送", false, ["openuuyc::features::remote_input", "openuuyc::transport::rtc::input"]),
    ("performance", "性能与网络统计", false, ["openuuyc::diagnostics::performance", "openuuyc::features::network_control", "openuuyc::transport::timing", "openuuyc::transport::rtcp_timing", "openuuyc::transport::rtc::clock"]),
    ("plugins", "插件宿主", false, ["openuuyc::plugins"]),
    ("ui", "界面渲染", false, ["openuuyc::ui", "openuuyc::ui_timing", "openuuyc::platform::windows::graphics", "openuuyc::platform::linux::graphics"]),
    ("webrtc", "WebRTC", true, ["webrtc", "webrtc_data", "webrtc_util", "interceptor"]),
    ("kcp", "KCP", true, ["kcp"]),
    ("ice", "ICE 直连与中转", true, ["webrtc_ice", "stun", "turn", "webrtc_mdns"]),
    ("sctp", "SCTP / DTLS / SRTP", true, ["webrtc_sctp", "dtls", "webrtc_srtp"]),
    ("http", "HTTP / TLS / WebSocket", true, ["reqwest", "hyper", "hyper_util", "rustls", "tokio_tungstenite", "tungstenite", "tower"]),
    ("credentials", "系统凭据库", true, ["keyring_core"]),
    ("runtime", "异步运行时", true, ["tokio", "mio"]),
    ("graphics", "图像与字体", true, ["epaint", "glifo", "vello_common", "zune_core", "zune_jpeg", "fax"]),
    ("native", "窗口与音频后端", true, ["winit", "egui", "cpal", "display_info", "arboard", "webbrowser"]),
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub application: Level,
    pub dependencies: Level,
    pub modules: BTreeMap<String, Level>,
    // Applying the same settings must still revoke a CLI override in other processes.
    revision: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            application: Level::Info,
            dependencies: Level::Warn,
            modules: BTreeMap::new(),
            revision: String::new(),
        }
    }
}
impl Settings {
    fn filter(&self) -> Result<EnvFilter> {
        for id in self.modules.keys() {
            if !MODULES.iter().any(|m| m.id == id) {
                bail!("未知日志模块：{id}");
            }
        }
        let mut directives = format!(
            "{},openuuyc={app},openuuyc_codec={app},openuuyc_h264={app},openuuyc_av1={app}",
            self.dependencies.directive(),
            app = self.application.directive()
        );
        for module in MODULES {
            if let Some(level) = self.modules.get(module.id) {
                for target in module.targets {
                    directives.push_str(&format!(",{target}={}", level.directive()));
                }
            }
        }
        EnvFilter::try_new(directives).context("解析日志级别")
    }
}
type FilterHandle = tracing_subscriber::reload::Handle<EnvFilter, tracing_subscriber::Registry>;
struct State {
    settings: Settings,
    override_filter: Option<String>,
    observed: Option<Vec<u8>>,
    error: Option<String>,
}
struct Runtime {
    state: Mutex<State>,
    filter: FilterHandle,
    config: PathBuf,
    directory: PathBuf,
    session: String,
    stem: String,
    explicit_file: Option<PathBuf>,
    files: Arc<Mutex<files::Status>>,
    dropped: ErrorCounter,
}
static RUNTIME: OnceLock<Arc<Runtime>> = OnceLock::new();
pub struct LoggingGuard {
    stop: mpsc::Sender<()>,
    watcher: Option<JoinHandle<()>>,
    _file_guard: WorkerGuard,
}
impl Drop for LoggingGuard {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}
pub struct Snapshot {
    pub settings: Settings,
    pub directory: PathBuf,
    pub file: PathBuf,
    pub session: String,
    pub config: PathBuf,
    pub override_filter: Option<String>,
    pub dropped: usize,
    pub error: Option<String>,
    pub managed: bool,
}
pub fn snapshot() -> Option<Snapshot> {
    let r = RUNTIME.get()?;
    let state = r.state.lock().unwrap_or_else(|e| e.into_inner());
    let files = r.files.lock().unwrap_or_else(|e| e.into_inner());
    Some(Snapshot {
        settings: state.settings.clone(),
        directory: r.directory.clone(),
        file: files.path.clone(),
        session: r.session.clone(),
        config: r.config.clone(),
        override_filter: state.override_filter.clone(),
        dropped: r.dropped.dropped_lines(),
        error: files.error.clone().or_else(|| state.error.clone()),
        managed: r.explicit_file.is_none(),
    })
}
pub fn apply(mut settings: Settings) -> Result<()> {
    let r = RUNTIME.get().context("日志系统尚未初始化")?;
    let filter = settings.filter()?;
    settings.revision = uuid::Uuid::new_v4().to_string();
    let bytes = serde_json::to_vec_pretty(&settings)?;
    let mut state = r.state.lock().unwrap_or_else(|e| e.into_inner());
    files::save_config(&r.config, &bytes)?;
    r.filter.reload(filter).context("更新日志过滤器")?;
    state.settings = settings;
    state.observed = Some(bytes);
    state.override_filter = None;
    state.error = None;
    Ok(())
}
/// systemd 给服务的标准错误接上 journal 时会设 `JOURNAL_STREAM=设备号:inode`，两个
/// 数都是十进制。这个变量会被子进程继承，桌面自己拉起的程序也可能带着它（niri 的
/// 会话里就有），所以光看它在不在会给没接 journal 的进程也挂一层 stderr，要比一比
/// 自己标准错误的 `dev:ino`。
#[cfg(target_os = "linux")]
fn in_journal() -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(value) = std::env::var_os("JOURNAL_STREAM") else {
        return false;
    };
    let Ok(stderr) = std::fs::metadata("/proc/self/fd/2") else {
        return false;
    };
    value
        .to_str()
        .is_some_and(|value| value == format!("{}:{}", stderr.dev(), stderr.ino()))
}

/// journal 是 Linux 的 systemd 才有的东西。
#[cfg(not(target_os = "linux"))]
fn in_journal() -> bool {
    false
}

/// All viewer children belong to this application launch.
pub fn configure_child(command: &mut std::process::Command) {
    if let Some(r) = RUNTIME.get() {
        // 继承下去的 `JOURNAL_STREAM` 是父进程的流，子进程的标准错误接到哪儿由父进程
        // 决定，留着这个变量只会让子进程以为自己也在 journal 里。
        command.env_remove("JOURNAL_STREAM");
        command.env("OPENUUYC_LOG_SESSION", &r.session);
        command.env("OPENUUYC_LOG_STEM", &r.stem);
        let state = r.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(filter) = &state.override_filter {
            command.arg("--log-level").arg(filter);
        }
        if let Some(path) = &r.explicit_file {
            command.arg("--log-file").arg(path);
        }
    }
}
pub fn open_directory() -> Result<()> {
    files::open_directory(&RUNTIME.get().context("日志系统尚未初始化")?.directory)
}
fn read_settings(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("读取日志设置"),
    };
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        bail!("日志设置文件过大");
    }
    Ok(Some(bytes))
}
fn decode_settings(bytes: Option<&[u8]>) -> Result<Settings> {
    let settings = bytes
        .map(serde_json::from_slice)
        .transpose()?
        .unwrap_or_default();
    Settings::filter(&settings)?;
    Ok(settings)
}
pub fn init(level: Option<&str>, path: Option<&Path>) -> Result<LoggingGuard> {
    let (config_directory, default_directory) = files::directories()?;
    std::fs::create_dir_all(&config_directory).context("创建日志设置目录")?;
    let config = dunce::canonicalize(&config_directory)?.join("logging.json");
    let (observed, settings, error) = match read_settings(&config) {
        Ok(bytes) => match decode_settings(bytes.as_deref()) {
            Ok(settings) => (bytes, settings, None),
            Err(e) => (
                bytes,
                Settings::default(),
                Some(format!("日志设置无效，暂用默认级别：{e:#}")),
            ),
        },
        Err(e) => (
            None,
            Settings::default(),
            Some(format!("日志设置读取失败：{e:#}")),
        ),
    };
    let filter = if let Some(level) = level {
        EnvFilter::try_new(level).context("无效的 --log-level")?
    } else {
        settings.filter()?
    };
    let explicit_file = path.map(std::path::absolute).transpose()?;
    if explicit_file
        .as_ref()
        .is_some_and(|path| path.file_name().is_none())
    {
        bail!("日志路径必须包含文件名");
    }
    let directory = explicit_file
        .as_ref()
        .and_then(|p| p.parent())
        .map(Path::to_owned)
        .unwrap_or(default_directory);
    std::fs::create_dir_all(&directory).context("创建日志目录")?;
    // Resolve MSIX/AppData virtualization and shell-incompatible verbatim prefixes.
    let directory = dunce::canonicalize(&directory).context("解析实际日志目录")?;
    let explicit_file = explicit_file.map(|path| directory.join(path.file_name().unwrap()));
    let session = std::env::var("OPENUUYC_LOG_SESSION")
        .ok()
        .and_then(|value| uuid::Uuid::parse_str(&value).ok())
        .unwrap_or_else(uuid::Uuid::new_v4)
        .simple()
        .to_string();
    let stem = std::env::var("OPENUUYC_LOG_STEM")
        .ok()
        .filter(|stem| {
            stem.ends_with(&format!("-{session}"))
                && files::managed_name(&format!("{stem}.0000.log"))
        })
        .unwrap_or_else(|| {
            format!(
                "openuuyc-{}-{}-{session}",
                chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
                std::process::id()
            )
        });
    let files = Arc::new(Mutex::new(files::Status::default()));
    let sink = files::Writer::new(&directory, explicit_file.as_deref(), files.clone(), &stem)?;
    let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .buffered_lines_limit(8192)
        .lossy(true)
        .thread_name("log-writer")
        .finish(sink);
    let dropped = writer.error_counter();
    // 被 systemd 起来时标准错误接在 journal 上，日志平时只落文件，这时再挂一层
    // stderr，`journalctl --user -u openuuyc` 才有内容。
    let journal = in_journal().then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .event_format(JournalEventFormat(
                tracing_subscriber::fmt::format().without_time(),
            ))
    });
    // 过滤器挂在订阅者上而不是某一层上：两层（文件与 stderr）用同一份，改设置后
    // 两层一起跟随。
    let (filter_layer, filter) = tracing_subscriber::reload::Layer::new(filter);
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .event_format(ProcessEventFormat(
                    tracing_subscriber::fmt::format()
                        .with_target(true)
                        .with_thread_ids(true),
                    session.clone(),
                )),
        )
        .with(journal)
        .try_init()
        .context("初始化日志系统")?;
    let runtime = Arc::new(Runtime {
        state: Mutex::new(State {
            settings,
            override_filter: level.map(str::to_owned),
            observed,
            error,
        }),
        filter,
        config,
        directory,
        session,
        stem,
        explicit_file,
        files,
        dropped,
    });
    RUNTIME
        .set(runtime.clone())
        .map_err(|_| anyhow::anyhow!("日志系统已初始化"))?;
    let (stop, rx) = mpsc::channel();
    let watcher = std::thread::Builder::new()
        .name("log-settings".into())
        .spawn(move || {
            while matches!(
                rx.recv_timeout(Duration::from_secs(1)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                files::sync_pending(&runtime.files);
                // Serialize read/apply so polling cannot install stale settings after a GUI save.
                let mut state = runtime.state.lock().unwrap_or_else(|e| e.into_inner());
                match read_settings(&runtime.config) {
                    Ok(bytes) if bytes != state.observed => {
                        let result = decode_settings(bytes.as_deref()).and_then(|settings| {
                            runtime.filter.reload(settings.filter()?)?;
                            state.settings = settings;
                            state.override_filter = None;
                            Ok(())
                        });
                        state.error = result.err().map(|e: anyhow::Error| {
                            format!("更新日志设置失败，保留当前级别：{e:#}")
                        });
                        state.observed = bytes;
                    }
                    Err(e) => state.error = Some(format!("读取日志设置失败：{e:#}")),
                    _ => {}
                }
            }
        })
        .context("启动日志设置同步")?;
    Ok(LoggingGuard {
        stop,
        watcher: Some(watcher),
        _file_guard: guard,
    })
}
struct ProcessEventFormat(tracing_subscriber::fmt::format::Format, String);

impl<S, N> tracing_subscriber::fmt::format::FormatEvent<S, N> for ProcessEventFormat
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::format::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        context: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        // Preserve ownership when users combine separate process logs, or opt
        // into a shared diagnostic file through --log-file.
        write!(writer, "session={} pid={} ", self.1, std::process::id())?;
        self.0.format_event(context, writer, event)
    }
}

/// journal 那一层的行格式：journald 自己记时间，不再写一遍；行首加 sd-daemon 的
/// `<N>` 级别前缀，journald 读标准输出、标准错误时默认（`SyslogLevelPrefix=yes`）
/// 据此记优先级，`journalctl -p warning` 才筛得出来。多行事件每行都加，否则后面
/// 几行会按默认的 info 记。
struct JournalEventFormat<F>(F);

impl<S, N, F> tracing_subscriber::fmt::format::FormatEvent<S, N> for JournalEventFormat<F>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::format::FormatFields<'a> + 'static,
    F: tracing_subscriber::fmt::format::FormatEvent<S, N>,
{
    fn format_event(
        &self,
        context: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let mut text = String::new();
        self.0.format_event(
            context,
            tracing_subscriber::fmt::format::Writer::new(&mut text),
            event,
        )?;
        let priority = syslog_priority(event.metadata().level());
        for line in text.strip_suffix('\n').unwrap_or(&text).split('\n') {
            writeln!(writer, "<{priority}>{line}")?;
        }
        Ok(())
    }
}

/// syslog 优先级：err、warning、info、debug。
fn syslog_priority(level: &tracing::Level) -> u8 {
    match *level {
        tracing::Level::ERROR => 3,
        tracing::Level::WARN => 4,
        tracing::Level::INFO => 6,
        _ => 7,
    }
}

pub(crate) fn open_folder(path: &Path) -> Result<()> {
    files::open_directory(path)
}

// Export worker only: sync bytes already consumed by the nonblocking writer.
pub(crate) fn sync_written() {
    if let Some(runtime) = RUNTIME.get() {
        files::sync_pending(&runtime.files);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Self;
        fn make_writer(&'a self) -> Self {
            self.clone()
        }
    }

    /// 每一行（多行事件的每一行）都带 journald 认的 `<N>` 前缀，且没有时间戳。
    #[test]
    fn journal_lines_carry_the_syslog_priority() {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(captured.clone())
                .with_ansi(false)
                .event_format(JournalEventFormat(
                    tracing_subscriber::fmt::format().without_time(),
                )),
        );
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!("broken");
            tracing::warn!("careful");
            tracing::info!("hello");
            tracing::debug!("detail");
            tracing::trace!("noise");
            tracing::warn!("first\nsecond");
        });
        let text = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let expected = [
            ("<3>", "ERROR", "broken"),
            ("<4>", "WARN", "careful"),
            ("<6>", "INFO", "hello"),
            ("<7>", "DEBUG", "detail"),
            ("<7>", "TRACE", "noise"),
            ("<4>", "WARN", "first"),
        ];
        assert_eq!(lines.len(), expected.len() + 1, "{text}");
        for (line, (prefix, level, message)) in lines.iter().zip(expected) {
            let rest = line
                .strip_prefix(prefix)
                .unwrap_or_else(|| panic!("{line}"));
            assert!(rest.trim_start().starts_with(level), "{line}");
            assert!(line.ends_with(message), "{line}");
        }
        assert_eq!(lines[6], "<4>second");
    }
}
