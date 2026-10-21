//! Desktop media preparation within an already authorized controlled connection.
use super::{VideoConfig, capture, encoder, format};
use crate::features::stream_control::publisher;
use crate::i18n::t;
use crate::protocol::capability::DeviceCapability;
use anyhow::{Context, Result, ensure};
use std::sync::Arc;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Capabilities {
    pub screen: capture::Screen,
    pub codecs: Vec<format::Capability>,
    pub adapters: Vec<capture::EncodingAdapter>,
}

#[derive(Default)]
pub(super) struct Cache(tokio::sync::Mutex<Option<Arc<Capabilities>>>);

fn same_probe_source(a: &capture::Screen, b: &capture::Screen) -> bool {
    // Placement, label, protocol ID, DPI and display refresh rate do not change
    // this 720p/30fps encoder check. Keep actual source/graphics/size/HDR bounds.
    a.identity == b.identity
        && a.device_name == b.device_name
        && a.adapter == b.adapter
        && a.render_adapter == b.render_adapter
        && a.width == b.width
        && a.height == b.height
        && a.hdr == b.hdr
}
impl Cache {
    pub fn snapshot(&self) -> Option<Arc<Capabilities>> {
        self.0.try_lock().ok()?.clone()
    }
    pub async fn prepare(
        &self,
        screen: capture::Screen,
        gpu_compatibility: bool,
        active: impl Fn() -> bool + Send + 'static,
    ) -> Result<Arc<Capabilities>> {
        let mut cached = self.0.lock().await;
        ensure!(active(), "{}", t().features.host_media_cancelled);
        let adapters = tokio::task::spawn_blocking(capture::encoding_adapters).await??;
        ensure!(active(), "{}", t().features.host_media_cancelled);
        if let Some(capabilities) = cached.as_ref()
            && same_probe_source(&capabilities.screen, &screen)
            && capabilities.adapters == adapters
        {
            let capabilities = Arc::new(Capabilities {
                screen,
                codecs: capabilities.codecs.clone(),
                adapters,
            });
            *cached = Some(capabilities.clone());
            tracing::info!("host media capabilities reused");
            return Ok(capabilities);
        }
        // The probe takes seconds (every encoder is opened once). Its result
        // only depends on the graphics stack and the source, so it is kept on
        // disk across restarts and recomputed when any of them changes.
        let key = ProbeKey::new(&screen, &adapters, gpu_compatibility);
        let luids = disk::Luids {
            listed: adapters.iter().map(|adapter| adapter.luid).collect(),
            source: screen.adapter,
        };
        let stored = {
            let (key, luids) = (key.clone(), luids.clone());
            tokio::task::spawn_blocking(move || disk::load(&key, &luids))
                .await
                .ok()
                .flatten()
        };
        if let Some(codecs) = stored {
            tracing::info!(capabilities = codecs.len(), "host media capabilities restored from disk");
            let capabilities = Arc::new(Capabilities {
                screen,
                codecs,
                adapters,
            });
            *cached = Some(capabilities.clone());
            return Ok(capabilities);
        }
        let capabilities = Arc::new(probe(screen, adapters, gpu_compatibility, active).await?);
        let codecs = capabilities.codecs.clone();
        tokio::task::spawn_blocking(move || disk::store(key, codecs, &luids));
        *cached = Some(capabilities.clone());
        Ok(capabilities)
    }
    pub async fn replace(&self, value: Option<Capabilities>) {
        *self.0.lock().await = value.map(Arc::new);
    }
    pub async fn invalidate(&self) {
        self.0.lock().await.take();
    }
}

/// What an encoder probe result depends on. Adapter LUIDs change across
/// reboots, so adapters are identified by PCI identity, name and driver.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct ProbeKey {
    build: String,
    gpu_compatibility: bool,
    adapters: Vec<(Option<crate::media::selection::GpuId>, u32, String, u64)>,
    source_adapter: Option<usize>,
    render_adapter: Option<usize>,
    identity: Option<String>,
    device_name: String,
    width: u32,
    height: u32,
    hdr: bool,
}

impl ProbeKey {
    fn new(screen: &capture::Screen, adapters: &[capture::EncodingAdapter], gpu_compatibility: bool) -> Self {
        let index = |luid: u64| adapters.iter().position(|a| a.luid == luid);
        Self {
            build: env!("CARGO_PKG_VERSION").to_owned(),
            gpu_compatibility,
            adapters: adapters
                .iter()
                .map(|a| (a.id, a.vendor, a.name.clone(), a.driver))
                .collect(),
            source_adapter: index(screen.adapter),
            render_adapter: screen.render_adapter.and_then(index),
            identity: screen.identity.clone(),
            device_name: screen.device_name.clone(),
            width: screen.width,
            height: screen.height,
            hdr: screen.hdr,
        }
    }
}

/// Encoder probe results kept per profile (`cache/encoder-probe.json`).
mod disk {
    use super::{ProbeKey, format};
    use serde::{Deserialize, Serialize};

    const ENTRIES: usize = 8;
    const MAX_AGE_SECONDS: i64 = 30 * 24 * 3600;

    #[derive(Default, Serialize, Deserialize)]
    struct Store {
        #[serde(default)]
        entries: Vec<Entry>,
    }

    #[derive(Serialize, Deserialize)]
    struct Entry {
        key: ProbeKey,
        codecs: Vec<(Adapter, format::Capability)>,
        saved_at: i64,
    }

    /// A codec's adapter by its place in the key: the LUID in the codec is
    /// only valid until the next reboot.
    #[derive(Clone, Copy, Serialize, Deserialize)]
    enum Adapter {
        Listed(usize),
        Source,
    }

    /// The current LUIDs of the key's adapters and source.
    #[derive(Clone)]
    pub(super) struct Luids {
        pub listed: Vec<u64>,
        pub source: u64,
    }

    fn path() -> Option<std::path::PathBuf> {
        Some(crate::product::local_data_dir().ok()?.join("cache").join("encoder-probe.json"))
    }

    fn read() -> Store {
        path()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub(super) fn load(key: &ProbeKey, luids: &Luids) -> Option<Vec<format::Capability>> {
        let now = chrono::Utc::now().timestamp();
        read()
            .entries
            .into_iter()
            .find(|entry| entry.key == *key && now - entry.saved_at < MAX_AGE_SECONDS)?
            .codecs
            .into_iter()
            .map(|(adapter, mut codec)| {
                codec.adapter = match adapter {
                    Adapter::Listed(index) => *luids.listed.get(index)?,
                    Adapter::Source => luids.source,
                };
                Some(codec)
            })
            .collect::<Option<Vec<_>>>()
            .filter(|codecs| !codecs.is_empty())
    }

    pub(super) fn store(key: ProbeKey, codecs: Vec<format::Capability>, luids: &Luids) {
        if codecs.is_empty() {
            return;
        }
        let Some(codecs) = codecs
            .into_iter()
            .map(|codec| {
                let adapter = match luids.listed.iter().position(|&luid| luid == codec.adapter) {
                    Some(index) => Adapter::Listed(index),
                    None if codec.adapter == luids.source => Adapter::Source,
                    None => return None,
                };
                Some((adapter, codec))
            })
            .collect::<Option<Vec<_>>>()
        else {
            return;
        };
        let Some(path) = path() else {
            return;
        };
        let mut store = read();
        store.entries.retain(|entry| entry.key != key);
        store.entries.insert(
            0,
            Entry {
                key,
                codecs,
                saved_at: chrono::Utc::now().timestamp(),
            },
        );
        store.entries.truncate(ENTRIES);
        let result = (|| -> anyhow::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let temporary = path.with_extension("json.tmp");
            std::fs::write(&temporary, serde_json::to_vec(&store)?)?;
            std::fs::rename(&temporary, &path)?;
            Ok(())
        })();
        if let Err(error) = result {
            tracing::debug!(%error, "encoder probe cache not saved");
        }
    }
}

async fn probe(
    screen: capture::Screen,
    adapters: Vec<capture::EncodingAdapter>,
    gpu_compatibility: bool,
    active: impl Fn() -> bool + Send + 'static,
) -> Result<Capabilities> {
    tokio::task::spawn_blocking(move || {
        let started = std::time::Instant::now();
        ensure!(active(), "{}", t().features.host_media_cancelled);
        let codecs = match probe_in_child(&screen, gpu_compatibility, &active) {
            Ok(codecs) => codecs,
            Err(error) => {
                ensure!(active(), "{}", t().features.host_media_cancelled);
                tracing::warn!(error = %format!("{error:#}"), "encoder probe process failed; probing here");
                let _runtime = encoder::Runtime::with_gpu_compatibility(gpu_compatibility)?;
                let mut desktop = capture::Desktop::open_selected(&screen)?;
                encoder::probe(&mut desktop, &active)?
            }
        };
        ensure!(active(), "{}", t().features.host_media_cancelled);
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis(),
            screen_id = screen.id,
            capabilities = codecs.len(),
            "host media capabilities prepared"
        );
        Ok(Capabilities {
            screen,
            codecs,
            adapters,
        })
    })
    .await
    .context(t().features.host_probe_interrupted)?
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ProbeRequest {
    screen: capture::Screen,
    gpu_compatibility: bool,
}

/// How long the probe process may take (every encoder is opened once).
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// The probe in a process of its own: the graphics drivers keep what every
/// encoder opened (hundreds of MB) for as long as the process has a device,
/// and a device centre or host keeps one for its whole life.
fn probe_in_child(
    screen: &capture::Screen,
    gpu_compatibility: bool,
    active: &dyn Fn() -> bool,
) -> Result<Vec<format::Capability>> {
    use std::io::{Read, Write};
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("encoder-probe")
        .creation_flags(0x0800_0000)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::diagnostics::logging::configure_child(&mut command);
    let mut child = command.spawn().context("无法启动编码能力检查进程")?;
    let _job = crate::plugins::process::Job::attach(&child);
    let request = serde_json::to_vec(&ProbeRequest {
        screen: screen.clone(),
        gpu_compatibility,
    })?;
    let mut input = child.stdin.take().context("检查进程没有输入")?;
    let mut output = child.stdout.take().context("检查进程没有输出")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).map(|_| bytes)
    });
    let written = input.write_all(&request);
    drop(input);
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if !active() || std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("编码能力检查进程已停止");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    written.context("无法把检查条件交给检查进程")?;
    ensure!(status.success(), "编码能力检查进程失败：{status}");
    let bytes = reader
        .join()
        .map_err(|_| anyhow::anyhow!("检查结果读取中断"))??;
    serde_json::from_slice(&bytes).context("检查进程的结果无效")
}

/// `encoder-probe`: the child side of [`probe_in_child`].
pub(crate) fn probe_process() -> Result<()> {
    use std::io::{Read, Write};
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let request: ProbeRequest = serde_json::from_slice(&input).context("检查条件无效")?;
    let _runtime = encoder::Runtime::with_gpu_compatibility(request.gpu_compatibility)?;
    let mut desktop = capture::Desktop::open_selected(&request.screen)?;
    let codecs = encoder::probe(&mut desktop, &|| true)?;
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, &codecs)?;
    output.flush()?;
    Ok(())
}

pub(crate) struct Prepared {
    pub screen: capture::Screen,
    pub negotiated: Arc<format::Negotiated>,
    pub config: VideoConfig,
}
impl Prepared {
    pub fn new(
        options: &publisher::ConnectOptions,
        screen: capture::Screen,
        capabilities: &[format::Capability],
        remote: &DeviceCapability,
        settings: super::EncodingSettings,
    ) -> Result<Self> {
        let negotiated = Arc::new(format::Negotiated::new(
            capabilities,
            remote,
            &options.decoders,
            settings,
            screen.render_adapter.unwrap_or(screen.adapter),
        )?);
        let mut config = publisher::config(options.params.as_ref());
        let chroma = options
            .params
            .as_ref()
            .map_or(1, |p| if p.chroma == 3 { 3 } else { 1 });
        let hdr = options.params.as_ref().is_some_and(|p| p.hdr);
        negotiated.apply_requested(&mut config, chroma, hdr, (screen.width, screen.height))?;
        tracing::info!(
            parameters = ?options.params,
            requested_fps = config.requested_fps,
            fps_limit = config.fps_limit,
            maximum_fps = config.maximum_fps,
            effective_fps = config.fps,
            "host initial capture settings negotiated"
        );
        Ok(Self {
            screen,
            negotiated,
            config,
        })
    }
}
