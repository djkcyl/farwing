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
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};
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
    pub name: ModuleName,
    pub targets: &'static [&'static str],
    pub external: bool,
}
/// A module's name in the current interface language (read when shown).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModuleName(&'static str);
impl ModuleName {
    pub fn text(self) -> &'static str {
        let t = &crate::i18n::t().session;
        match self.0 {
            "app" => t.log_module_app,
            "auth" => t.log_module_auth,
            "api" => t.log_module_api,
            "presence" => t.log_module_presence,
            "host" => t.log_module_host,
            "signal" => t.log_module_signal,
            "rtc" => t.log_module_rtc,
            "recovery" => t.log_module_recovery,
            "decoder" => t.log_module_decoder,
            "viewer" => t.log_module_viewer,
            "audio" => t.log_module_audio,
            "control" => t.log_module_control,
            "keyboard" => t.log_module_keyboard,
            "mouse" => t.log_module_mouse,
            "input" => t.log_module_input,
            "performance" => t.log_module_performance,
            "plugins" => t.log_module_plugins,
            "ui" => t.log_module_ui,
            "webrtc" => "WebRTC",
            "kcp" => "KCP",
            "ice" => t.log_module_ice,
            "sctp" => "SCTP / DTLS / SRTP",
            "http" => "HTTP / TLS / WebSocket",
            "credentials" => t.log_module_credentials,
            "runtime" => t.log_module_runtime,
            "graphics" => t.log_module_graphics,
            "native" => t.log_module_native,
            other => other,
        }
    }
}
impl std::fmt::Display for ModuleName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.text())
    }
}
impl From<ModuleName> for egui::WidgetText {
    fn from(name: ModuleName) -> Self {
        name.text().into()
    }
}
macro_rules! modules {
    ($(($id:literal, $external:literal, [$($target:literal),+])),+ $(,)?) => {
        pub const MODULES: &[Module] = &[$(Module {id: $id, name: ModuleName($id), external: $external, targets: &[$($target),+] }),+];
    };
}
modules! {
    ("app", false, ["farwing::application::app", "farwing::session::controller"]),
    ("auth", false, ["farwing::account::auth", "farwing::account::login", "farwing::account::session_restore"]),
    ("api", false, ["farwing::account::api", "farwing::account::client", "farwing::account::nrd_http", "farwing::account::assist"]),
    ("presence", false, ["farwing::session::presence", "farwing::session::device_session"]),
    ("host", false, ["farwing::features::host", "farwing_codec::encoder", "farwing::platform::windows::software_encoder", "farwing::media::encoding", "farwing::platform::windows::capture", "farwing::platform::windows::encoder", "farwing::platform::windows::nvenc", "farwing::platform::windows::amf", "farwing::platform::windows::qsv", "farwing::platform::windows::gdi", "farwing::platform::windows::preprocess", "farwing::platform::windows::transfer", "farwing::platform::windows::gpu_conversion", "farwing::platform::windows::cursor", "farwing::platform::windows::wol"]),
    ("signal", false, ["farwing::transport::signal"]),
    ("rtc", false, ["farwing::transport::rtc", "farwing::transport::fct"]),
    ("recovery", false, ["farwing::transport::video_receiver", "farwing::nack_audit", "farwing::transport::rsfec", "farwing::transport::ulpfec", "farwing::transport::flexfec", "farwing::transport::xor_fec"]),
    ("decoder", false, ["farwing::media::decoder", "farwing::media::decoder_pool", "farwing::media::decoder_result", "farwing::media::codec_parameters", "farwing_codec::decoder", "farwing_h264", "farwing_av1::bitstream", "farwing_av1::decoder", "farwing::platform::windows::decoder", "farwing::platform::windows::surface"]),
    ("viewer", false, ["farwing::application::viewer", "farwing::session::controller::windows", "farwing::media::video_color", "farwing::media::video_format", "farwing::platform::windows::video_shader", "farwing::platform::windows::swapchain"]),
    ("audio", false, ["farwing::media::audio", "farwing::media::microphone"]),
    ("control", false, ["farwing::features::stream_control", "farwing::features::viewing_settings"]),
    ("keyboard", false, ["farwing::application::viewer::windows_keyboard"]),
    ("mouse", false, ["farwing::application::viewer::windows_mouse", "farwing::application::viewer::windows_cursor", "farwing::features::remote_cursor"]),
    ("input", false, ["farwing::features::remote_input", "farwing::transport::rtc::input"]),
    ("performance", false, ["farwing::diagnostics::performance", "farwing::features::network_control", "farwing::transport::timing", "farwing::transport::rtcp_timing", "farwing::transport::rtc::clock"]),
    ("plugins", false, ["farwing::plugins"]),
    ("ui", false, ["farwing::ui", "farwing::ui_timing", "farwing::platform::windows::graphics"]),
    ("webrtc", true, ["webrtc", "webrtc_data", "webrtc_util", "interceptor"]),
    ("kcp", true, ["kcp"]),
    ("ice", true, ["webrtc_ice", "stun", "turn", "webrtc_mdns"]),
    ("sctp", true, ["webrtc_sctp", "dtls", "webrtc_srtp"]),
    ("http", true, ["reqwest", "hyper", "hyper_util", "rustls", "tokio_tungstenite", "tungstenite", "tower"]),
    ("credentials", true, ["keyring_core"]),
    ("runtime", true, ["tokio", "mio"]),
    ("graphics", true, ["epaint", "glifo", "vello_common", "zune_core", "zune_jpeg", "fax"]),
    ("native", true, ["winit", "egui", "cpal", "display_info", "arboard", "webbrowser"]),
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
                bail!("{}", (crate::i18n::t().session.log_module_unknown)(id));
            }
        }
        let mut directives = format!(
            // Hole-punch decisions (a few lines per session) go with the
            // application's own.
            "{},farwing={app},farwing_codec={app},farwing_h264={app},farwing_av1={app},webrtc_ice::agent::punch={app}",
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
        EnvFilter::try_new(directives).context(crate::i18n::t().session.log_levels_invalid)
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
    let r = RUNTIME.get().context(crate::i18n::t().session.logging_not_ready)?;
    let filter = settings.filter()?;
    settings.revision = uuid::Uuid::new_v4().to_string();
    let bytes = serde_json::to_vec_pretty(&settings)?;
    let mut state = r.state.lock().unwrap_or_else(|e| e.into_inner());
    files::save_config(&r.config, &bytes)?;
    r.filter.reload(filter).context(crate::i18n::t().session.log_filter_update_failed)?;
    state.settings = settings;
    state.observed = Some(bytes);
    state.override_filter = None;
    state.error = None;
    Ok(())
}
/// All viewer children belong to this application launch.
pub fn configure_child(command: &mut std::process::Command) {
    if let Some(r) = RUNTIME.get() {
        command.env("FARWING_LOG_SESSION", &r.session);
        command.env("FARWING_LOG_STEM", &r.stem);
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
    files::open_directory(&RUNTIME.get().context(crate::i18n::t().session.logging_not_ready)?.directory)
}
fn read_settings(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context(crate::i18n::t().session.log_settings_unreadable),
    };
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        bail!("{}", crate::i18n::t().session.log_settings_too_large);
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
    std::fs::create_dir_all(&config_directory).context(crate::i18n::t().session.log_settings_folder_failed)?;
    let config = dunce::canonicalize(&config_directory)?.join("logging.json");
    let (observed, settings, error) = match read_settings(&config) {
        Ok(bytes) => match decode_settings(bytes.as_deref()) {
            Ok(settings) => (bytes, settings, None),
            Err(e) => (
                bytes,
                Settings::default(),
                Some((crate::i18n::t().session.log_settings_invalid_default)(&format!("{e:#}"))),
            ),
        },
        Err(e) => (
            None,
            Settings::default(),
            Some((crate::i18n::t().session.log_settings_read_failed)(&format!("{e:#}"))),
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
    std::fs::create_dir_all(&directory).context(crate::i18n::t().session.log_folder_failed)?;
    // Resolve MSIX/AppData virtualization and shell-incompatible verbatim prefixes.
    let directory = dunce::canonicalize(&directory).context(crate::i18n::t().session.log_folder_unresolved)?;
    let explicit_file = explicit_file.map(|path| directory.join(path.file_name().unwrap()));
    let session = std::env::var("FARWING_LOG_SESSION")
        .ok()
        .and_then(|value| uuid::Uuid::parse_str(&value).ok())
        .unwrap_or_else(uuid::Uuid::new_v4)
        .simple()
        .to_string();
    let stem = std::env::var("FARWING_LOG_STEM")
        .ok()
        .filter(|stem| {
            stem.ends_with(&format!("-{session}"))
                && files::managed_name(&format!("{stem}.0000.log"))
        })
        .unwrap_or_else(|| {
            format!(
                "farwing-{}-{}-{session}",
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
    let (filter_layer, filter) = tracing_subscriber::reload::Layer::new(filter);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .event_format(ProcessEventFormat(
                    tracing_subscriber::fmt::format()
                        .with_target(true)
                        .with_thread_ids(true),
                    session.clone(),
                ))
                .with_filter(filter_layer),
        )
        .try_init()
        .context(crate::i18n::t().session.logging_init_failed)?;
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
        .map_err(|_| anyhow::anyhow!("{}", crate::i18n::t().session.logging_already_initialized))?;
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
                            (crate::i18n::t().session.log_settings_update_failed)(&format!("{e:#}"))
                        });
                        state.observed = bytes;
                    }
                    Err(e) => state.error = Some((crate::i18n::t().session.log_settings_read_failed)(&format!("{e:#}"))),
                    _ => {}
                }
            }
        })
        .context(crate::i18n::t().session.log_settings_sync_failed)?;
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

pub(crate) fn open_folder(path: &Path) -> Result<()> {
    files::open_directory(path)
}

// Export worker only: sync bytes already consumed by the nonblocking writer.
pub(crate) fn sync_written() {
    if let Some(runtime) = RUNTIME.get() {
        files::sync_pending(&runtime.files);
    }
}
