use super::process::lock;
use super::*;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::{
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct VideoRequest {
    pub id: u64,
    pub input: u64,
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    #[serde(default)]
    pub type_id: Option<String>,
    pub path: PathBuf,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct VideoNode {
    pub id: u64,
    pub input: u64,
    pub name: String,
    pub program: sdk::VideoProgram,
}
#[derive(Clone, Default)]
pub(crate) struct Graph {
    pub revision: u64,
    pub output_source: u64,
    pub nodes: Vec<VideoNode>,
}
#[derive(Clone)]
pub(crate) struct Tap {
    pub id: u64,
    pub revision: u64,
    pub source: u64,
    pub state: Arc<Shared>,
}
pub(crate) struct ChainShared {
    pub graph: Mutex<Graph>,
    pub taps: Mutex<Vec<Tap>>,
    pub revision: AtomicU64,
    pub applied_revision: AtomicU64,
    pub failed_revision: AtomicU64,
    pub video_failed: AtomicU64,
    pub wake: Mutex<Option<std::thread::Thread>>,
    pub error: Mutex<Option<String>>,
    pub generated: AtomicU64,
    pub skipped: AtomicU64,
}
impl ChainShared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            graph: Mutex::new(Graph::default()),
            taps: Mutex::new(Vec::new()),
            revision: AtomicU64::new(0),
            applied_revision: AtomicU64::new(0),
            failed_revision: AtomicU64::new(0),
            video_failed: AtomicU64::new(0),
            wake: Mutex::new(None),
            error: Mutex::new(None),
            generated: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
        })
    }
    pub fn publish(&self, nodes: Vec<VideoNode>, output_source: u64) -> u64 {
        let revision = self.revision.fetch_add(1, Ordering::AcqRel) + 1;
        *lock(&self.graph) = Graph {
            revision,
            output_source,
            nodes,
        };
        *lock(&self.error) = None;
        if let Some(wake) = lock(&self.wake).as_ref() {
            wake.unpark();
        }
        revision
    }
    pub fn fail(&self, error: String) {
        self.video_failed.store(
            self.applied_revision.load(Ordering::Acquire),
            Ordering::Release,
        );
        *lock(&self.error) = Some(error);
    }
}
pub(crate) fn validate_programs(nodes: &[VideoNode]) -> Result<()> {
    ensure!(nodes.len() <= 8, "视频节点最多8个");
    let mut count = 0;
    let mut rates = std::collections::BTreeMap::from([(0, 1usize)]);
    let mut delays = std::collections::BTreeMap::from([(0, 0u32)]);
    for node in nodes {
        ensure!(
            node.id != 0 && !rates.contains_key(&node.id),
            "重复视频节点ID"
        );
        let mut rate = *rates
            .get(&node.input)
            .context("视频输入节点不存在或顺序无效")?;
        let delay = delays[&node.input] + node.program.delay_frames;
        let program = &node.program;
        ensure!(
            program.backend == "d3d11-rgba16f"
                && !program.passes.is_empty()
                && program.delay_frames <= 2,
            "视频插件后端或延迟不兼容"
        );
        ensure!(delay <= 2, "视频路径延迟预算超限");
        for pass in &program.passes {
            count += 1;
            ensure!(
                pass.shader.len() >= 4
                    && pass.shader.len() <= 1024 * 1024
                    && pass.shader.starts_with(b"DXBC"),
                "无效的视频Shader"
            );
            ensure!(
                (1..=400).contains(&pass.scale_numerator)
                    && (1..=400).contains(&pass.scale_denominator)
                    && pass.params.iter().all(|v| v.is_finite()),
                "无效的视频尺寸或参数"
            );
            ensure!(
                !pass.phases.is_empty()
                    && pass.phases.len() <= 4
                    && pass.phases.last() == Some(&1.0)
                    && pass
                        .phases
                        .iter()
                        .all(|p| p.is_finite() && *p > 0.0 && *p <= 1.0)
                    && pass.phases.windows(2).all(|p| p[0] < p[1]),
                "无效的输出时间分数"
            );
            ensure!(
                pass.phases.len() == 1 || program.delay_frames > 0,
                "多输出节点必须声明呈现延迟"
            );
            rate *= pass.phases.len();
            ensure!(rate <= 4, "链的补帧倍率不能超过4倍");
        }
        rates.insert(node.id, rate);
        delays.insert(node.id, delay);
    }
    ensure!(count <= 16, "视频图pass预算超限");
    Ok(())
}

pub fn host() -> Result<()> {
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    ensure!(
        read_packet(&mut input, 64)? == b"OpenUUYC video protocol 1",
        "invalid video handshake"
    );
    let requests: Vec<VideoRequest> = serde_json::from_slice(&read_packet(&mut input, 65536)?)?;
    ensure!(requests.len() <= 8, "too many video nodes");
    let mut nodes = Vec::new();
    for request in requests {
        let type_id = request.type_id.as_deref().context("缺少视频节点类型")?;
        let manifest = read_manifest(&request.path)?.for_node(type_id)?;
        ensure!(
            manifest.capability == "video" && manifest.dependencies.is_empty(),
            "视频Shader节点不能声明原生服务依赖"
        );
        let dir = manifest.path.parent().context("plugin directory")?;
        let path = manifest.path.clone();
        ensure!(path.starts_with(dir), "video library outside module");
        let library: libloading::Library = unsafe { super::load_plugin_library(&path) }?;
        let query: libloading::Symbol<sdk::VideoNodeQuery> =
            unsafe { library.get(b"openuuyc_video_node_query_v1\0") }?;
        let api = unsafe { query(sdk::ABI_VERSION, type_id.as_ptr(), type_id.len()) };
        ensure!(!api.is_null(), "video ABI rejected");
        let header = unsafe { std::slice::from_raw_parts(api.cast::<u32>(), 2) };
        ensure!(
            header[0] == sdk::ABI_VERSION && header[1] as usize == size_of::<sdk::VideoApi>(),
            "video ABI mismatch"
        );
        if let Some(type_id) = &request.type_id {
            ensure!(
                manifest.nodes.iter().any(|n| &n.type_id == type_id
                    && n.implementation == sdk::NodeImplementation::VideoShader),
                "视频节点类型不匹配"
            );
        }
        let mut config = request.config.unwrap_or(manifest.config);
        if let Some(type_id) = request.type_id {
            config["$node_type"] = serde_json::Value::String(type_id);
        }
        let config = serde_json::to_vec(&config)?;
        let mut data = vec![0; sdk::MAX_RENDER_BYTES];
        let mut len = 0;
        ensure!(
            unsafe {
                ((*api).describe)(
                    config.as_ptr(),
                    config.len(),
                    data.as_mut_ptr(),
                    data.len(),
                    &mut len,
                )
            } == 0
                && len <= data.len(),
            "{} 配置失败",
            manifest.name
        );
        nodes.push(VideoNode {
            id: request.id,
            input: request.input,
            name: manifest.name,
            program: serde_json::from_slice(&data[..len])?,
        });
    }
    validate_programs(&nodes)?;
    write_packet(&mut output, &serde_json::to_vec(&nodes)?)?;
    Ok(())
}

pub(crate) struct Loader {
    receiver: mpsc::Receiver<Result<Vec<VideoNode>>>,
    cancel: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    worker: Option<JoinHandle<()>>,
}
impl Loader {
    pub fn start(requests: Vec<VideoRequest>, ctx: egui::Context) -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None::<Child>));
        let state = cancel.clone();
        let process = child.clone();
        let worker = std::thread::Builder::new()
            .name("Video plugin loader".into())
            .spawn(move || {
                let result = (|| -> Result<Vec<VideoNode>> {
                    let mut command = Command::new(std::env::current_exe()?);
                    command
                        .arg("plugin-video-host")
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null());
                    #[cfg(windows)]
                    command.creation_flags(0x08000000);
                    crate::diagnostics::logging::configure_child(&mut command);
                    let mut spawned = command.spawn()?;
                    let job = match super::process::Job::attach(&spawned) {
                        Ok(job) => job,
                        Err(e) => {
                            let _ = spawned.kill();
                            let _ = spawned.wait();
                            return Err(e);
                        }
                    };
                    let mut input = spawned.stdin.take().context("video stdin")?;
                    let mut output = spawned.stdout.take().context("video stdout")?;
                    *lock(&process) = Some(spawned);
                    let timer_state = state.clone();
                    let timer_child = process.clone();
                    let timer = std::thread::spawn(move || {
                        let deadline = Instant::now() + Duration::from_secs(10);
                        while !timer_state.load(Ordering::Acquire) && Instant::now() < deadline {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        if let Some(child) = lock(&timer_child).as_mut() {
                            let _ = child.kill();
                        }
                    });
                    let result = (|| {
                        ensure!(!state.load(Ordering::Acquire), "video load cancelled");
                        write_packet(&mut input, b"OpenUUYC video protocol 1")?;
                        write_packet(&mut input, &serde_json::to_vec(&requests)?)?;
                        let nodes: Vec<VideoNode> = serde_json::from_slice(&read_packet(
                            &mut output,
                            sdk::MAX_RENDER_BYTES,
                        )?)?;
                        validate_programs(&nodes)?;
                        Ok(nodes)
                    })();
                    state.store(true, Ordering::Release);
                    drop(input);
                    drop(output);
                    drop(job);
                    let _ = timer.join();
                    if let Some(mut child) = lock(&process).take() {
                        let _ = child.kill();
                        let _ = child.wait();
                    }
                    result
                })();
                let _ = sender.send(result);
                ctx.request_repaint();
            })?;
        Ok(Self {
            receiver,
            cancel,
            child,
            worker: Some(worker),
        })
    }
    pub fn poll(&self) -> Option<Result<Vec<VideoNode>>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(_) => Some(Err(anyhow::anyhow!("视频插件加载线程退出"))),
        }
    }
}
impl Drop for Loader {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(child) = lock(&self.child).as_mut() {
            let _ = child.kill();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
