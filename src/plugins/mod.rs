//! Native plugin host. Business/inference/drawing implementations live in DLLs.
mod chain;
mod editor;
pub mod graph;
pub(crate) mod hotkeys;
mod input;
mod manager;
mod metadata;
mod parameters;
mod process;
mod settings;
mod ui;
pub mod video;
mod watch;

// The node-graph video pipeline is driven by the Windows player only; the
// Linux player has no effect chain yet.
#[cfg(windows)]
pub(crate) use chain::Controller;
pub(crate) use manager::Manager;
#[cfg(windows)]
pub(crate) use parameters::capturing as capturing_shortcut;
#[cfg(windows)]
pub(crate) use process::Sample;
pub(crate) use process::Shared;
pub(crate) use ui::paint_plugin_icon;
#[cfg(windows)]
pub(crate) use video::{ChainShared, Graph, Tap};

use anyhow::{Context, Result, ensure};
use openuuyc_plugin_api as sdk;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize)]
pub(crate) struct Manifest {
    pub id: String,
    pub name: String,
    pub abi: u32,
    #[serde(default)]
    pub version: String,
    pub capability: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    pub config: serde_json::Value,
    #[serde(default)]
    pub config_labels: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub config_schema: std::collections::BTreeMap<String, parameters::Field>,
    #[serde(default)]
    pub nodes: Vec<sdk::NodeDefinition>,
    #[serde(skip)]
    pub path: PathBuf,
    #[serde(skip)]
    selected_node: Option<String>,
}
impl Manifest {
    fn for_node(&self, type_id: &str) -> Result<Self> {
        let node = self
            .nodes
            .iter()
            .find(|n| n.type_id == type_id)
            .context("插件未提供此节点")?;
        let mut selected = self.clone();
        selected.capability = match node.implementation {
            sdk::NodeImplementation::FrameAnalysis | sdk::NodeImplementation::SceneSource => {
                "analysis"
            }
            sdk::NodeImplementation::Overlay => "overlay",
            sdk::NodeImplementation::DetectionControl => "control",
            sdk::NodeImplementation::VideoShader => "video",
        }
        .into();
        selected.dependencies.extend(node.dependencies.clone());
        selected.dependencies.sort();
        selected.dependencies.dedup();
        ensure!(selected.dependencies.len() <= 8, "节点依赖过多");
        selected.config = node.config.clone();
        selected.config_labels = node.config_labels.clone();
        selected.config_schema = node.config_schema.clone();
        selected.selected_node = Some(type_id.into());
        Ok(selected)
    }
}
pub(crate) fn root() -> Result<PathBuf> {
    let adjacent = crate::application::bootstrap::resource_origin()
        .unwrap_or(std::env::current_exe()?)
        .parent()
        .context("missing executable directory")?
        .join("plugins");
    if adjacent.is_dir() {
        return Ok(adjacent);
    }
    Ok(crate::platform::paths::require_local_app_data()?.join("OpenUUYC/plugins"))
}
pub(crate) fn open_folder(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    let path = std::fs::canonicalize(path)?;

    // Launch the resolved directory through ShellExecute instead of passing it
    // to explorer.exe as a child-process argument. Explorer may otherwise fall
    // back to its inherited working directory (typically C:\\Users\\<user>).
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
            core::{PCWSTR, w},
        };
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(path.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize <= 32 {
            anyhow::bail!("打开文件夹失败（{}）", result.0 as isize);
        }
    }
    #[cfg(not(windows))]
    {
        let status = std::process::Command::new("xdg-open")
            .arg(&path)
            .status()
            .context("启动 xdg-open")?;
        anyhow::ensure!(status.success(), "打开文件夹失败（{status}）");
    }
    Ok(())
}

/// Load a plugin library without letting it pull dependencies from elsewhere:
/// Windows restricts the search path, Linux resolves eagerly into a local scope.
pub(super) unsafe fn load_plugin_library(path: &Path) -> Result<libloading::Library> {
    #[cfg(windows)]
    {
        // LOAD_WITH_ALTERED_SEARCH_PATH | LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR
        Ok(unsafe {
            libloading::os::windows::Library::load_with_flags(path, 0x00000100 | 0x00000800)
        }?
        .into())
    }
    #[cfg(not(windows))]
    {
        Ok(unsafe {
            libloading::os::unix::Library::open(
                Some(path),
                libloading::os::unix::RTLD_NOW | libloading::os::unix::RTLD_LOCAL,
            )
        }?
        .into())
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
fn read_manifest(path: &Path) -> Result<Manifest> {
    let library = std::fs::canonicalize(path)?;
    let directory = std::fs::canonicalize(path.parent().context("插件目录无效")?)?;
    ensure!(
        library.parent() == Some(directory.as_path()),
        "插件动态库超出自身目录"
    );
    let bytes = metadata::read(&library)?.context("动态库没有插件内嵌清单")?;
    let mut m: Manifest = serde_json::from_slice(&bytes)?;
    ensure!(
        valid_id(&m.id)
            && m.name.len() <= 128
            && m.abi == sdk::ABI_VERSION
            && m.dependencies.len() <= 8
            && m.dependencies.iter().all(|id| valid_id(id))
            && m.config.is_object(),
        "插件清单或 ABI 不兼容"
    );
    ensure!(
        matches!(m.capability.as_str(), "nodes" | "inference"),
        "未知插件能力"
    );
    ensure!(
        (m.capability == "nodes") != m.nodes.is_empty(),
        "插件能力与节点声明不一致"
    );
    m.path = library;
    Ok(m)
}
pub(crate) fn discover() -> Result<Vec<Manifest>> {
    let root = root()?;
    if !root.exists() {
        return Ok(Vec::new());
    }
    let entries = metadata::candidates(&root)?
        .iter()
        .map(|p| read_manifest(p))
        .collect::<Result<Vec<_>>>()?;
    let mut ids = BTreeSet::new();
    ensure!(
        entries.iter().all(|m| ids.insert(m.id.clone())),
        "存在重复插件 ID"
    );
    Ok(entries)
}
fn plan(path: &Path, node: Option<&str>) -> Result<Vec<Manifest>> {
    let root = path
        .parent()
        .and_then(Path::parent)
        .context("invalid plugin location")?;
    let mut all = metadata::candidates(root)?
        .iter()
        .map(|p| read_manifest(p))
        .collect::<Result<Vec<_>>>()?;
    let mut ids = BTreeSet::new();
    ensure!(
        all.iter().all(|m| ids.insert(m.id.clone())),
        "duplicate plugin ID"
    );
    let requested = read_manifest(path)?;
    let requested = if let Some(node) = node {
        requested.for_node(node)?
    } else {
        ensure!(requested.nodes.is_empty(), "必须指定插件节点");
        requested
    };
    *all.iter_mut()
        .find(|m| m.id == requested.id)
        .context("插件不在目录中")? = requested.clone();
    fn visit(
        id: &str,
        all: &[Manifest],
        pending: &mut BTreeSet<String>,
        done: &mut BTreeSet<String>,
        out: &mut Vec<Manifest>,
    ) -> Result<()> {
        if done.contains(id) {
            return Ok(());
        }
        ensure!(
            pending.len() < 8 && pending.insert(id.into()),
            "插件依赖循环或层级过深"
        );
        let m = all
            .iter()
            .find(|m| m.id == id)
            .with_context(|| format!("缺少依赖：{id}"))?;
        for dependency in &m.dependencies {
            ensure!(
                all.iter().any(|provider| provider.id == *dependency
                    && provider.nodes.is_empty()
                    && provider.capability == "inference"),
                "缺少推理依赖：{dependency}"
            );
            visit(dependency, all, pending, done, out)?;
        }
        pending.remove(id);
        done.insert(id.into());
        let mut effective = m.clone();
        settings::apply(&mut effective)?;
        out.push(effective);
        Ok(())
    }
    let mut out = Vec::new();
    visit(
        &requested.id,
        &all,
        &mut BTreeSet::new(),
        &mut BTreeSet::new(),
        &mut out,
    )?;
    ensure!(out.len() <= 8, "插件不是分析入口或依赖过多");
    ensure!(
        out.iter().filter(|m| m.capability == "inference").count() <= 1,
        "推理依赖冲突"
    );
    Ok(out)
}

pub(crate) fn write_packet(writer: &mut impl Write, bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() <= sdk::MAX_RENDER_BYTES, "plugin packet limit");
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()?;
    Ok(())
}
pub(crate) fn read_packet(reader: &mut impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut len = [0; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    ensure!(len <= limit, "plugin packet limit");
    let mut data = vec![0; len];
    reader.read_exact(&mut data)?;
    Ok(data)
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Request {
    #[serde(default)]
    pub observation: Option<sdk::Observation>,
    #[serde(default)]
    pub signals: Vec<ControlSignal>,
    pub generation: u64,
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub view: sdk::Viewport,
}
#[derive(Serialize, Deserialize)]
pub(crate) struct Reply {
    pub generation: u64,
    pub sequence: u64,
    pub revision: u64,
    pub rendered: sdk::Rendered,
    #[serde(default)]
    pub input: Vec<ControlOutput>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ControlOutput {
    pub id: u64,
    pub epoch: u64,
    pub trigger: bool,
    pub commands: Vec<sdk::InputCommand>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ControlSignal {
    #[serde(default)]
    pub trigger: bool,
    pub id: u64,
    pub active: bool,
    pub epoch: u64,
}

struct Loaded {
    api: *const sdk::PluginApi,
    instance: *mut std::ffi::c_void,
    _library: libloading::Library,
    id: String,
}
impl Drop for Loaded {
    fn drop(&mut self) {
        unsafe {
            ((*self.api).destroy)(self.instance);
        }
    }
}
struct Group(Vec<Loaded>);
impl Drop for Group {
    fn drop(&mut self) {
        while self.0.pop().is_some() {}
    }
}

fn load_module(group: &mut Group, m: Manifest) -> Result<usize> {
    let directory = m.path.parent().context("plugin directory")?;
    let library_path = m.path.clone();
    ensure!(
        library_path.starts_with(directory),
        "插件动态库超出自身目录"
    );
    let library: libloading::Library = unsafe { load_plugin_library(&library_path) }?;
    let api = if let Some(node) = &m.selected_node {
        let query: libloading::Symbol<sdk::NodeQuery> =
            unsafe { library.get(b"openuuyc_node_query_v1\0") }?;
        unsafe { query(sdk::ABI_VERSION, node.as_ptr(), node.len()) }
    } else {
        let query: libloading::Symbol<sdk::Query> =
            unsafe { library.get(b"openuuyc_plugin_query_v1\0") }?;
        unsafe { query(sdk::ABI_VERSION) }
    };
    ensure!(!api.is_null(), "plugin rejected ABI");
    let header = unsafe { std::slice::from_raw_parts(api.cast::<u32>(), 2) };
    ensure!(
        header[0] == sdk::ABI_VERSION && header[1] as usize == size_of::<sdk::PluginApi>(),
        "plugin ABI mismatch"
    );
    let api_ref = unsafe { &*api };
    ensure!(
        match m.capability.as_str() {
            "analysis" => api_ref.process.is_some(),
            "inference" => api_ref.inference.is_some(),
            "overlay" | "control" => api_ref.render.is_some(),
            _ => false,
        },
        "plugin capability mismatch"
    );
    let inference = group
        .0
        .iter()
        .filter(|d| m.dependencies.contains(&d.id))
        .find_map(|d| unsafe { (*d.api).inference.map(|f| f(d.instance)) });
    let services = sdk::HostServices {
        struct_size: size_of::<sdk::HostServices>() as u32,
        inference: inference.as_ref().map_or(std::ptr::null(), |s| s),
    };
    let mut config = m.config;
    // File-valued settings are resolved relative to each module, not cwd.
    for key in ["model", "runtime"] {
        if let Some(value) = config.get(key).and_then(|v| v.as_str()) {
            let resolved = std::fs::canonicalize(directory.join(value))?;
            ensure!(resolved.starts_with(directory), "插件资源超出自身目录");
            config[key] = serde_json::Value::String(
                resolved
                    .to_str()
                    .context("plugin path is not UTF-8")?
                    .into(),
            );
        }
    }
    let config = serde_json::to_vec(&config)?;
    let mut instance = std::ptr::null_mut();
    ensure!(
        unsafe { (api_ref.create)(config.as_ptr(), config.len(), &services, &mut instance) } == 0
            && !instance.is_null(),
        "{} 初始化失败",
        m.name
    );
    tracing::info!(target:"openuuyc::plugins",plugin=%m.id,"plugin module loaded");
    group.0.push(Loaded {
        api,
        instance,
        _library: library,
        id: m.id,
    });
    Ok(group.0.len() - 1)
}

/// Internal child entry, private inherited pipes only; no login or device API.
pub fn host(path: &Path) -> Result<()> {
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    ensure!(
        read_packet(&mut input, 64)? == b"OpenUUYC plugin protocol 1",
        "invalid plugin handshake"
    );
    let spec: graph::AnalysisSpec = serde_json::from_slice(&read_packet(&mut input, 65536)?)?;
    ensure!(
        std::fs::canonicalize(path)? == std::fs::canonicalize(&spec.instance.path)?
            && (!spec.renderers.is_empty() || !spec.controls.is_empty())
            && spec.renderers.len() <= 4
            && spec.controls.len() <= 4,
        "invalid graph analysis binding"
    );
    let mut manifests = plan(path, Some(&spec.instance.type_id))?;
    let entry = manifests.last_mut().context("missing analysis module")?;
    ensure!(
        entry.capability == "analysis"
            && entry
                .nodes
                .iter()
                .any(|n| n.type_id == spec.instance.type_id
                    && matches!(
                        n.implementation,
                        sdk::NodeImplementation::FrameAnalysis
                            | sdk::NodeImplementation::SceneSource
                    )
                    && spec.sample_frames
                        == (n.implementation == sdk::NodeImplementation::FrameAnalysis)),
        "分析节点类型不匹配"
    );
    entry.config = spec.instance.config.clone();
    entry.config["$node_type"] = serde_json::Value::String(spec.instance.type_id.clone());
    // Limit accidental concurrent model workloads across viewer processes.
    let _slot = if spec.sample_frames {
        Some(process::AnalysisSlot::acquire()?)
    } else {
        None
    };
    let mut group = Group(Vec::new());
    for m in manifests {
        load_module(&mut group, m)?;
    }
    let entry_index = group.0.len() - 1;
    let mut renderer_indices = Vec::new();
    for renderer in &spec.renderers {
        let mut modules = plan(&renderer.instance.path, Some(&renderer.instance.type_id))?;
        let mut manifest = modules.pop().context("缺少绘制模块")?;
        ensure!(
            manifest.capability == "overlay"
                && manifest
                    .nodes
                    .iter()
                    .any(|n| n.type_id == renderer.instance.type_id
                        && n.implementation == sdk::NodeImplementation::Overlay),
            "绘制节点类型不匹配"
        );
        for dependency in modules {
            if !group.0.iter().any(|m| m.id == dependency.id) {
                load_module(&mut group, dependency)?;
            }
        }
        manifest.config = renderer.instance.config.clone();
        manifest.config["$node_type"] =
            serde_json::Value::String(renderer.instance.type_id.clone());
        let index = load_module(&mut group, manifest)?;
        renderer_indices.push((renderer.clone(), index));
    }
    let mut control_indices = Vec::new();
    for control in &spec.controls {
        let instance = &control.instance;
        let mut modules = plan(&instance.path, Some(&instance.type_id))?;
        let mut manifest = modules.pop().context("缺少控制模块")?;
        ensure!(
            manifest.capability == "control"
                && manifest.nodes.iter().any(|n| n.type_id == instance.type_id
                    && n.implementation == sdk::NodeImplementation::DetectionControl),
            "控制节点类型不匹配"
        );
        for dependency in modules {
            if !group.0.iter().any(|m| m.id == dependency.id) {
                load_module(&mut group, dependency)?;
            }
        }
        manifest.config = instance.config.clone();
        manifest.config["$node_type"] = instance.type_id.clone().into();
        control_indices.push((control.clone(), load_module(&mut group, manifest)?));
    }
    let entry = &group.0[entry_index];
    let process = unsafe { (*entry.api).process }.context("missing analysis processor")?;
    let mut texture_ids = std::collections::BTreeMap::new();
    let mut next_texture = 1u64;
    let mut scene = vec![0; sdk::MAX_SCENE_BYTES];
    let mut scene_len = 0;
    ensure!(
        unsafe {
            process(
                entry.instance,
                std::ptr::null(),
                scene.as_mut_ptr(),
                scene.len(),
                &mut scene_len,
            )
        } == 0
            && scene_len <= scene.len(),
        "initial scene failed"
    );
    let mut scene_generation = None;
    let mut consumed_triggers = std::collections::BTreeMap::new();
    write_packet(&mut output, b"ready")?;
    loop {
        let packet = match read_packet(&mut input, 4096) {
            Ok(p) => p,
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::UnexpectedEof) =>
            {
                break;
            }
            Err(e) => return Err(e),
        };
        let request: Request = serde_json::from_slice(&packet)?;
        if request.width > 0 {
            ensure!(spec.sample_frames, "静态绘制节点不接收画面采样");
            ensure!(
                request.height > 0
                    && request.width <= sdk::MAX_EDGE
                    && request.height <= sdk::MAX_EDGE,
                "frame dimensions invalid"
            );
            let rgba = read_packet(&mut input, (sdk::MAX_EDGE * sdk::MAX_EDGE * 4) as usize)?;
            ensure!(
                rgba.len() == (request.width * request.height * 4) as usize,
                "frame size mismatch"
            );
            let frame = sdk::Frame {
                struct_size: size_of::<sdk::Frame>() as u32,
                width: request.width,
                height: request.height,
                stride: request.width * 4,
                sequence: request.sequence,
                rgba: rgba.as_ptr(),
                len: rgba.len(),
            };
            ensure!(
                unsafe {
                    process(
                        entry.instance,
                        &frame,
                        scene.as_mut_ptr(),
                        scene.len(),
                        &mut scene_len,
                    )
                } == 0
                    && scene_len <= scene.len(),
                "analysis callback failed"
            );
        }
        if request.width > 0 {
            scene_generation = Some(request.generation);
        }
        if scene_generation != Some(request.generation) {
            let mut initial: sdk::Scene = serde_json::from_slice(&scene[..scene_len])?;
            initial.layers.retain(|layer| layer.persistent);
            let bytes = serde_json::to_vec(&initial)?;
            scene[..bytes.len()].copy_from_slice(&bytes);
            scene_len = bytes.len();
        }
        let mut control_outputs = std::collections::BTreeMap::new();
        let mut input_commands = Vec::new();
        for (control, index) in &control_indices {
            let mut control_scene: sdk::Scene = serde_json::from_slice(&scene[..scene_len])?;
            let signal = request.signals.iter().find(|s| s.id == control.instance.id);
            control_scene.control_active = signal.is_some_and(|s| {
                s.active
                    && (!s.trigger
                        || (request.width > 0 && consumed_triggers.get(&s.id) != Some(&s.epoch)))
            });
            if control_scene.control_active
                && let Some(s) = signal.filter(|s| s.trigger)
            {
                consumed_triggers.insert(s.id, s.epoch);
            }
            control_scene.control_epoch = signal.map_or(0, |s| s.epoch);
            control_scene.input.clear();
            control_scene.observation = request.observation.clone().filter(|o| {
                request.width > 0
                    && o.generation == request.generation
                    && o.sequence == request.sequence
            });
            if request.width == 0 {
                control_scene.detections.clear();
            }
            let bytes = serde_json::to_vec(&control_scene)?;
            let module = &group.0[*index];
            let callback = unsafe { (*module.api).render }.context("missing control callback")?;
            let mut result = vec![0; sdk::MAX_SCENE_BYTES];
            let mut len = 0;
            ensure!(
                unsafe {
                    callback(
                        module.instance,
                        bytes.as_ptr(),
                        bytes.len(),
                        &request.view,
                        result.as_mut_ptr(),
                        result.len(),
                        &mut len,
                    )
                } == 0
                    && len <= result.len(),
                "control callback failed"
            );
            let mut output: sdk::Scene = serde_json::from_slice(&result[..len])?;
            if control.send_input {
                input_commands.push(ControlOutput {
                    id: control.instance.id,
                    epoch: control_scene.control_epoch,
                    trigger: control_scene.control_active
                        && signal.is_some_and(|s| s.trigger)
                        && request.width > 0,
                    commands: if request.width > 0 && control_scene.control_active {
                        std::mem::take(&mut output.input)
                    } else {
                        Vec::new()
                    },
                });
            }
            output.input.clear();
            control_outputs.insert(control.instance.id, serde_json::to_vec(&output)?);
        }
        let mut rendered = sdk::Rendered::default();
        for (renderer, index) in &renderer_indices {
            let module = &group.0[*index];
            let render = unsafe { (*module.api).render }.context("missing renderer")?;
            let render_scene = if let Some(id) = renderer.source_control {
                control_outputs
                    .get(&id)
                    .context("missing control layer")?
                    .as_slice()
            } else {
                &scene[..scene_len]
            };
            let mut result = vec![0; sdk::MAX_RENDER_BYTES];
            let mut len = 0;
            ensure!(
                unsafe {
                    render(
                        module.instance,
                        render_scene.as_ptr(),
                        render_scene.len(),
                        &request.view,
                        result.as_mut_ptr(),
                        result.len(),
                        &mut len,
                    )
                } == 0
                    && len <= result.len(),
                "overlay callback failed"
            );
            let mut output: sdk::Rendered = serde_json::from_slice(&result[..len])?;
            let remap =
                |id: u64, map: &mut std::collections::BTreeMap<(u64, u64), u64>, next: &mut u64| {
                    *map.entry((renderer.instance.id, id)).or_insert_with(|| {
                        let value = *next;
                        *next += 1;
                        value
                    })
                };
            for texture in &mut output.textures {
                texture.id = remap(texture.id, &mut texture_ids, &mut next_texture);
            }
            for (position, layer) in output.layers.iter_mut().enumerate() {
                layer.id = format!("{:x}.{}", renderer.instance.id, layer.id);
                layer.composite_order = renderer
                    .order
                    .saturating_mul(65536)
                    .saturating_add(position as u32);
                for mesh in &mut layer.meshes {
                    mesh.texture = remap(mesh.texture, &mut texture_ids, &mut next_texture);
                }
            }
            rendered.textures.extend(output.textures);
            rendered.layers.extend(output.layers);
        }
        let reply = Reply {
            generation: request.generation,
            sequence: request.sequence,
            revision: request.view.revision,
            rendered,
            input: input_commands,
        };
        write_packet(&mut output, &serde_json::to_vec(&reply)?)?;
    }
    Ok(())
}
