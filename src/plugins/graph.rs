//! Typed persistent graph and compiler. No runtime is started while editing.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

pub const RAW: &str = "host.video_source.v1";
pub const VIDEO: &str = "host.video_output.v1";
pub const OVERLAY: &str = "host.overlay_output.v1";
pub const INPUT: &str = "host.input_output.v1";
const MAX_FILE: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub type_id: String,
    pub type_version: u32,
    pub name: String,
    pub position: [f32; 2],
    pub parameters: serde_json::Value,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub inputs: Vec<sdk::NodePort>,
    #[serde(default)]
    pub outputs: Vec<sdk::NodePort>,
}
fn enabled() -> bool {
    true
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Endpoint {
    pub node: String,
    pub port: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Edge {
    pub id: String,
    pub from: Endpoint,
    pub to: Endpoint,
    #[serde(default)]
    pub order: u32,
    /// Editor-only curve waypoints; not nodes or executable operations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reroutes: Vec<[f32; 2]>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Document {
    pub graph_format_version: u32,
    pub graph_id: String,
    pub name: String,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Publication {
    pub revision: String,
    pub document: Document,
}
#[derive(Clone)]
pub struct NodeType {
    pub definition: sdk::NodeDefinition,
    pub(crate) manifest: Option<Manifest>,
    pub(crate) fields: super::parameters::Fields,
}
#[derive(Clone, Default)]
pub struct Catalog {
    pub types: BTreeMap<String, NodeType>,
    providers: BTreeSet<String>,
}
#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub node: Option<String>,
    pub message: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct InstanceSpec {
    pub id: u64,
    pub path: PathBuf,
    pub type_id: String,
    pub config: serde_json::Value,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RenderSpec {
    pub instance: InstanceSpec,
    pub order: u32,
    #[serde(default)]
    pub source_control: Option<u64>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ControlSpec {
    pub instance: InstanceSpec,
    #[serde(default)]
    pub conditions: Vec<ActivationSpec>,
    pub send_input: bool,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ActivationSpec {
    pub port: String,
    pub name: String,
    pub hotkey: Option<super::hotkeys::Spec>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct AnalysisSpec {
    pub instance: InstanceSpec,
    pub source: u64,
    pub sample_frames: bool,
    pub renderers: Vec<RenderSpec>,
    #[serde(default)]
    pub controls: Vec<ControlSpec>,
}
#[derive(Clone)]
pub(crate) struct Compiled {
    pub document: Document,
    pub videos: Vec<super::video::VideoRequest>,
    pub output_source: u64,
    pub analyses: Vec<AnalysisSpec>,
    pub warnings: Vec<String>,
}
fn port(id: &str, name: &str, data_type: sdk::PortType, many: bool) -> sdk::NodePort {
    sdk::NodePort {
        id: id.into(),
        name: name.into(),
        data_type,
        many,
        optional: false,
    }
}
impl Catalog {
    pub fn load() -> Result<Self> {
        Self::from_manifests(discover()?)
    }
    pub(crate) fn from_manifests(manifests: Vec<Manifest>) -> Result<Self> {
        let mut result = Self {
            providers: manifests
                .iter()
                .filter(|m| m.nodes.is_empty() && m.capability == "inference")
                .map(|m| m.id.clone())
                .collect(),
            ..Self::default()
        };
        for (id, name, inputs, outputs) in [
            (
                RAW,
                "原始画面",
                vec![],
                vec![port("frames", "画面", sdk::PortType::Frame, false)],
            ),
            (
                VIDEO,
                "视频输出",
                vec![port("video", "画面", sdk::PortType::Frame, false)],
                vec![],
            ),
            (
                INPUT,
                "远端输入",
                vec![port(
                    "commands",
                    "输入指令",
                    sdk::PortType::InputCommands,
                    true,
                )],
                vec![],
            ),
            (
                OVERLAY,
                "叠加输出",
                vec![port("layers", "图层", sdk::PortType::Layer, true)],
                vec![],
            ),
        ] {
            result.types.insert(
                id.into(),
                NodeType {
                    definition: sdk::NodeDefinition {
                        type_id: id.into(),
                        schema_version: 1,
                        name: name.into(),
                        description: match id {
                            RAW => "提供远端解码后的原始画面。",
                            VIDEO => "将连接的画面呈现在观看窗口。",
                            INPUT => "将插件指令发送到远端；需单独开启控制。",
                            _ => "将绘制图层叠加到观看画面。",
                        }
                        .into(),
                        category: if id == RAW { "输入源" } else { "输出端" }.into(),
                        layer_renderer: None,
                        dependencies: Vec::new(),
                        config: serde_json::json!({}),
                        config_labels: Default::default(),
                        config_schema: Default::default(),
                        implementation: sdk::NodeImplementation::Overlay,
                        inputs,
                        outputs,
                    },
                    manifest: None,
                    fields: super::parameters::Fields::default(),
                },
            );
        }
        for manifest in manifests {
            for definition in &manifest.nodes {
                let mut definition = definition.clone();
                let (category, description) = match definition.implementation {
                    sdk::NodeImplementation::VideoShader => {
                        ("图像处理", "处理输入画面并输出处理后的画面。")
                    }
                    sdk::NodeImplementation::FrameAnalysis => {
                        ("画面分析", "分析输入画面并输出分析结果。")
                    }
                    sdk::NodeImplementation::Overlay | sdk::NodeImplementation::SceneSource => {
                        ("绘制", "将绘制指令转换为叠加图层。")
                    }
                    sdk::NodeImplementation::DetectionControl => {
                        ("输入控制", "将检测结果转换为远端输入指令。")
                    }
                };
                if definition.description.is_empty() {
                    definition.description = description.into();
                }
                if definition.category.is_empty() {
                    definition.category = category.into();
                }
                ensure!(
                    definition.type_id.starts_with(&format!("{}.", manifest.id))
                        && definition.schema_version == 1
                        && definition.name.len() <= 128
                        && definition.description.len() <= 1024
                        && definition.category.len() <= 128,
                    "节点命名空间或版本无效"
                );
                ensure!(
                    definition.config.is_object()
                        && definition.dependencies.len() <= 8
                        && definition.dependencies.iter().all(|id| valid_id(id)),
                    "节点配置或依赖无效"
                );
                let selected = manifest.for_node(&definition.type_id)?;
                let fields = super::parameters::Fields::new(&selected);
                let expected = match definition.implementation {
                    sdk::NodeImplementation::VideoShader => {
                        ("video", sdk::PortType::Frame, sdk::PortType::Frame)
                    }
                    sdk::NodeImplementation::FrameAnalysis
                    | sdk::NodeImplementation::SceneSource => {
                        ("analysis", sdk::PortType::Frame, sdk::PortType::DrawList)
                    }
                    sdk::NodeImplementation::Overlay => {
                        ("overlay", sdk::PortType::DrawList, sdk::PortType::Layer)
                    }
                    sdk::NodeImplementation::DetectionControl => (
                        "control",
                        sdk::PortType::Detections,
                        sdk::PortType::InputCommands,
                    ),
                };
                ensure!(
                    selected.capability == expected.0
                        && (if definition.implementation == sdk::NodeImplementation::SceneSource {
                            definition.inputs.is_empty()
                        } else {
                            definition.inputs.len() == 1
                                && definition.inputs[0].data_type == expected.1
                                && !definition.inputs[0].many
                        })
                        && (definition.outputs.len() == 1
                            || (definition.implementation
                                == sdk::NodeImplementation::DetectionControl
                                && definition.outputs.len() == 2
                                && definition.outputs[1].data_type == sdk::PortType::Layer)
                            || (definition.implementation
                                == sdk::NodeImplementation::FrameAnalysis
                                && definition.outputs.len() == 2
                                && definition.outputs[1].data_type == sdk::PortType::Detections))
                        && definition.outputs[0].data_type == expected.2,
                    "节点执行契约不受支持"
                );
                ensure!(
                    definition
                        .inputs
                        .iter()
                        .map(|p| &p.id)
                        .collect::<BTreeSet<_>>()
                        .len()
                        == definition.inputs.len()
                        && definition
                            .outputs
                            .iter()
                            .map(|p| &p.id)
                            .collect::<BTreeSet<_>>()
                            .len()
                            == definition.outputs.len(),
                    "节点端口ID重复"
                );
                ensure!(
                    definition
                        .inputs
                        .iter()
                        .chain(&definition.outputs)
                        .all(|p| valid_id(&p.id) && p.name.len() <= 128),
                    "无效端口"
                );
                ensure!(
                    !result.types.contains_key(&definition.type_id),
                    "节点类型重复"
                );
                result.types.insert(
                    definition.type_id.clone(),
                    NodeType {
                        definition: definition.clone(),
                        manifest: Some(selected),
                        fields: fields.clone(),
                    },
                );
            }
        }
        Ok(result)
    }
    pub fn instantiate(&self, type_id: &str, position: [f32; 2]) -> Result<Node> {
        let ty = self.types.get(type_id).context("节点类型不存在")?;
        Ok(Node {
            id: uuid::Uuid::new_v4().to_string(),
            type_id: type_id.into(),
            type_version: ty.definition.schema_version,
            name: ty.definition.name.clone(),
            position,
            parameters: ty.fields.with_defaults(&serde_json::json!({})),
            enabled: true,
            inputs: ty.definition.inputs.clone(),
            outputs: ty.definition.outputs.clone(),
        })
    }
    pub fn ports<'a>(&'a self, node: &'a Node, output: bool) -> &'a [sdk::NodePort] {
        match self.types.get(&node.type_id) {
            Some(t) if t.definition.schema_version == node.type_version => {
                if output {
                    &t.definition.outputs
                } else {
                    &t.definition.inputs
                }
            }
            _ => {
                if output {
                    &node.outputs
                } else {
                    &node.inputs
                }
            }
        }
    }
}
impl Document {
    pub fn new(catalog: &Catalog) -> Result<Self> {
        let source = catalog.instantiate(RAW, [30.0, 80.0])?;
        let output = catalog.instantiate(VIDEO, [580.0, 80.0])?;
        let overlay = catalog.instantiate(OVERLAY, [580.0, 340.0])?;
        let edge = Edge {
            id: uuid::Uuid::new_v4().to_string(),
            from: Endpoint {
                node: source.id.clone(),
                port: "frames".into(),
            },
            to: Endpoint {
                node: output.id.clone(),
                port: "video".into(),
            },
            order: 0,
            reroutes: Vec::new(),
        };
        Ok(Self {
            graph_format_version: 1,
            graph_id: uuid::Uuid::new_v4().to_string(),
            name: "新节点图".into(),
            nodes: vec![source, output, overlay],
            edges: vec![edge],
        })
    }
    pub fn connect(&mut self, catalog: &Catalog, from: Endpoint, to: Endpoint) -> Result<()> {
        ensure!(from.node != to.node, "不能连接到自身");
        let source = self
            .nodes
            .iter()
            .find(|n| n.id == from.node)
            .context("输出节点不存在")?;
        let destination = self
            .nodes
            .iter()
            .find(|n| n.id == to.node)
            .context("输入节点不存在")?;
        let a = catalog
            .ports(source, true)
            .iter()
            .find(|p| p.id == from.port)
            .context("输出端口不存在")?;
        let b = catalog
            .ports(destination, false)
            .iter()
            .find(|p| p.id == to.port)
            .context("输入端口不存在")?;
        ensure!(a.data_type == b.data_type, "端口类型不匹配");
        let many = b.many;
        // Reject a cycle before replacing any existing input connection.
        let mut pending = vec![to.node.clone()];
        let mut seen = BTreeSet::new();
        while let Some(id) = pending.pop() {
            ensure!(id != from.node, "连线会形成环路");
            if seen.insert(id.clone()) {
                pending.extend(
                    self.edges
                        .iter()
                        .filter(|e| e.from.node == id)
                        .map(|e| e.to.node.clone()),
                );
            }
        }
        if self.edges.iter().any(|e| e.from == from && e.to == to) {
            return Ok(());
        }
        if !many {
            self.edges.retain(|e| e.to != to);
        }
        ensure!(self.edges.len() < 256, "连线数量超限");
        let order = self
            .edges
            .iter()
            .filter(|e| e.to == to)
            .map(|e| e.order)
            .max()
            .map_or(0, |n| n.saturating_add(1));
        self.edges.push(Edge {
            id: uuid::Uuid::new_v4().to_string(),
            from,
            to,
            order,
            reroutes: Vec::new(),
        });
        Ok(())
    }
}

pub(crate) fn compile(
    document: &Document,
    catalog: &Catalog,
) -> std::result::Result<Compiled, Vec<Diagnostic>> {
    let run = || -> Result<Compiled> {
        ensure!(
            document.graph_format_version == 1 && uuid::Uuid::parse_str(&document.graph_id).is_ok(),
            "节点图版本或ID无效"
        );
        ensure!(
            document.nodes.len() <= 64 && document.edges.len() <= 256 && document.name.len() <= 128,
            "节点图规模超限"
        );
        let mut nodes = BTreeMap::new();
        for node in &document.nodes {
            ensure!(
                uuid::Uuid::parse_str(&node.id).is_ok()
                    && nodes.insert(node.id.clone(), node).is_none(),
                "重复或无效节点ID"
            );
            ensure!(
                node.position.iter().all(|p| p.is_finite())
                    && serde_json::to_vec(&node.parameters)?.len() <= 65536,
                "节点参数或布局无效"
            );
        }
        let mut edge_ids = BTreeSet::new();
        let mut connections = BTreeSet::new();
        let mut incoming: BTreeMap<(String, String), Vec<&Edge>> = BTreeMap::new();
        for edge in &document.edges {
            ensure!(
                edge.reroutes.len() <= 32 && edge.reroutes.iter().flatten().all(|p| p.is_finite()),
                "中继点数量或坐标无效"
            );
            ensure!(edge_ids.insert(&edge.id), "重复连线ID");
            ensure!(
                connections.insert((
                    &edge.from.node,
                    &edge.from.port,
                    &edge.to.node,
                    &edge.to.port
                )),
                "重复连线"
            );
            let source = nodes.get(&edge.from.node).context("连线来源节点不存在")?;
            let target = nodes.get(&edge.to.node).context("连线目标节点不存在")?;
            let a = catalog
                .ports(source, true)
                .iter()
                .find(|p| p.id == edge.from.port)
                .context("连线输出端口不存在")?;
            let b = catalog
                .ports(target, false)
                .iter()
                .find(|p| p.id == edge.to.port)
                .context("连线输入端口不存在")?;
            ensure!(a.data_type == b.data_type, "端口类型不兼容");
            let list = incoming
                .entry((edge.to.node.clone(), edge.to.port.clone()))
                .or_default();
            list.push(edge);
            ensure!(b.many || list.len() == 1, "单输入端口连接了多个来源");
        }
        let video_sinks = document
            .nodes
            .iter()
            .filter(|n| n.enabled && n.type_id == VIDEO)
            .collect::<Vec<_>>();
        ensure!(video_sinks.len() <= 1, "每个观看窗口只允许一个视频输出");
        let mut roots = video_sinks.clone();
        roots.extend(
            document
                .nodes
                .iter()
                .filter(|n| n.enabled && (n.type_id == OVERLAY || n.type_id == INPUT)),
        );
        let mut order = Vec::new();
        let mut visited = BTreeSet::new();
        let mut visiting = BTreeSet::new();
        fn visit<'a>(
            id: &str,
            nodes: &BTreeMap<String, &'a Node>,
            incoming: &BTreeMap<(String, String), Vec<&Edge>>,
            catalog: &Catalog,
            visiting: &mut BTreeSet<String>,
            visited: &mut BTreeSet<String>,
            order: &mut Vec<&'a Node>,
        ) -> Result<()> {
            if visited.contains(id) {
                return Ok(());
            }
            ensure!(visiting.insert(id.into()), "节点图存在环路");
            let node = nodes.get(id).context("节点不存在")?;
            let ty = catalog
                .types
                .get(&node.type_id)
                .with_context(|| format!("缺少节点类型：{}", node.type_id))?;
            ensure!(
                node.type_version == ty.definition.schema_version,
                "节点版本不兼容"
            );
            if node.enabled
                && let Some(manifest) = &ty.manifest
            {
                ensure!(
                    manifest
                        .dependencies
                        .iter()
                        .all(|id| catalog.providers.contains(id)),
                    "{} 缺少推理依赖",
                    ty.definition.name
                );
            }
            let follows = node.enabled
                || ty.definition.implementation == sdk::NodeImplementation::VideoShader;
            if follows {
                for port in &ty.definition.inputs {
                    let edges = incoming.get(&(id.into(), port.id.clone()));
                    if ![VIDEO, OVERLAY, INPUT].contains(&node.type_id.as_str()) && !port.optional {
                        ensure!(
                            edges.is_some_and(|e| !e.is_empty()),
                            "{} 的 {} 尚未连接",
                            node.name,
                            port.name
                        );
                    }
                    if let Some(edges) = edges {
                        for edge in edges {
                            visit(
                                &edge.from.node,
                                nodes,
                                incoming,
                                catalog,
                                visiting,
                                visited,
                                order,
                            )?;
                        }
                    }
                }
            }
            visiting.remove(id);
            visited.insert(id.into());
            order.push(node);
            Ok(())
        }
        for root in roots {
            visit(
                &root.id,
                &nodes,
                &incoming,
                catalog,
                &mut visiting,
                &mut visited,
                &mut order,
            )?;
        }
        let ids = document
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i as u64 + 1))
            .collect::<BTreeMap<_, _>>();
        let mut frames: BTreeMap<String, u64> = BTreeMap::new();
        let mut scenes: BTreeMap<String, String> = BTreeMap::new();
        let mut layer_sources: BTreeMap<String, (String, InstanceSpec)> = BTreeMap::new();
        let mut control_sources: BTreeMap<String, (String, InstanceSpec)> = BTreeMap::new();
        let mut videos = Vec::new();
        let mut analyses: BTreeMap<String, AnalysisSpec> = BTreeMap::new();
        let upstream = |node: &Node| {
            incoming
                .iter()
                .filter(|((id, _), _)| id == &node.id)
                .flat_map(|(_, e)| e.iter())
                .find(|e| catalog.ports(node, false).iter().any(|p| p.id == e.to.port))
                .map(|e| e.from.node.as_str())
        };
        let instance = |node: &Node, manifest: &Manifest| InstanceSpec {
            id: ids[node.id.as_str()],
            path: manifest.path.clone(),
            type_id: node.type_id.clone(),
            config: node.parameters.clone(),
        };
        for node in order {
            if node.type_id == RAW {
                frames.insert(node.id.clone(), 0);
                continue;
            }
            let ty = &catalog.types[&node.type_id];
            let Some(manifest) = &ty.manifest else {
                continue;
            };
            match ty.definition.implementation {
                sdk::NodeImplementation::VideoShader => {
                    let source = upstream(node)
                        .and_then(|id| frames.get(id))
                        .copied()
                        .context("视频输入没有可用帧")?;
                    if node.enabled {
                        let spec = instance(node, manifest);
                        videos.push(super::video::VideoRequest {
                            id: spec.id,
                            input: source,
                            path: spec.path,
                            config: Some(spec.config),
                            type_id: Some(spec.type_id),
                        });
                        frames.insert(node.id.clone(), spec.id);
                    } else {
                        frames.insert(node.id.clone(), source);
                    }
                }
                sdk::NodeImplementation::FrameAnalysis | sdk::NodeImplementation::SceneSource => {
                    if node.enabled {
                        let sample_frames =
                            ty.definition.implementation == sdk::NodeImplementation::FrameAnalysis;
                        let source = if sample_frames {
                            upstream(node)
                                .and_then(|id| frames.get(id))
                                .copied()
                                .context("分析输入没有可用帧")?
                        } else {
                            0
                        };
                        analyses.insert(
                            node.id.clone(),
                            AnalysisSpec {
                                instance: instance(node, manifest),
                                source,
                                sample_frames,
                                renderers: Vec::new(),
                                controls: Vec::new(),
                            },
                        );
                        scenes.insert(node.id.clone(), node.id.clone());
                    }
                }
                sdk::NodeImplementation::Overlay => {
                    if node.enabled
                        && let Some(source) = upstream(node).and_then(|id| scenes.get(id))
                    {
                        layer_sources
                            .insert(node.id.clone(), (source.clone(), instance(node, manifest)));
                    }
                }
                sdk::NodeImplementation::DetectionControl => {
                    if node.enabled
                        && let Some(source) = upstream(node).and_then(|id| scenes.get(id))
                    {
                        control_sources
                            .insert(node.id.clone(), (source.clone(), instance(node, manifest)));
                    }
                }
            }
        }
        let output_source = video_sinks
            .first()
            .and_then(|n| upstream(n))
            .and_then(|id| frames.get(id))
            .copied()
            .unwrap_or(0);
        let mut layers = document
            .edges
            .iter()
            .filter(|e| nodes[&e.to.node].enabled && nodes[&e.to.node].type_id == OVERLAY)
            .collect::<Vec<_>>();
        layers.sort_by_key(|e| (e.order, e.id.clone()));
        let input_edges = document
            .edges
            .iter()
            .filter(|e| nodes[&e.to.node].enabled && nodes[&e.to.node].type_id == INPUT)
            .collect::<Vec<_>>();
        ensure!(input_edges.len() <= 4, "每个窗口最多允许四个输入来源");
        let control_gates = |node_id: &str| -> Result<Vec<ActivationSpec>> {
            let node = nodes[node_id];
            catalog.types[&node.type_id]
                .fields
                .shortcuts(&node.parameters, ids[node_id])
        };
        for (node_id, (analysis, instance)) in &control_sources {
            let send_input = input_edges.iter().any(|e| e.from.node == *node_id);
            let drawn = layers.iter().any(|e| e.from.node == *node_id);
            if send_input || drawn {
                analyses
                    .get_mut(analysis)
                    .context("控制来源不存在")?
                    .controls
                    .push(ControlSpec {
                        instance: instance.clone(),
                        conditions: control_gates(node_id)?,
                        send_input,
                    });
            }
        }
        for (index, edge) in layers.into_iter().enumerate() {
            if let Some((analysis, renderer)) = layer_sources.get(&edge.from.node) {
                analyses
                    .get_mut(analysis)
                    .context("分析来源不存在")?
                    .renderers
                    .push(RenderSpec {
                        instance: renderer.clone(),
                        order: index as u32,
                        source_control: None,
                    });
            } else if let Some((analysis, control)) = control_sources.get(&edge.from.node) {
                let definition = &catalog.types[&nodes[&edge.from.node].type_id].definition;
                let renderer_type = definition
                    .layer_renderer
                    .as_ref()
                    .context("图层输出缺少外置绘制节点")?;
                let renderer = catalog
                    .types
                    .get(renderer_type)
                    .context("未安装所需绘制节点")?;
                ensure!(
                    renderer.definition.implementation == sdk::NodeImplementation::Overlay,
                    "图层绘制依赖类型错误"
                );
                let manifest = renderer.manifest.as_ref().context("图层绘制依赖必须外置")?;
                ensure!(
                    manifest
                        .dependencies
                        .iter()
                        .all(|id| catalog.providers.contains(id)),
                    "绘制节点缺少推理依赖"
                );
                analyses
                    .get_mut(analysis)
                    .context("分析来源不存在")?
                    .renderers
                    .push(RenderSpec {
                        instance: InstanceSpec {
                            id: control.id + 1024,
                            path: manifest.path.clone(),
                            type_id: renderer_type.clone(),
                            config: manifest.config.clone(),
                        },
                        order: index as u32,
                        source_control: Some(control.id),
                    });
            }
        }
        ensure!(
            analyses.values().all(|a| a.renderers.len() <= 4),
            "每个分析支路最多4个绘制节点"
        );
        let analyses = analyses
            .into_values()
            .filter(|a| !a.renderers.is_empty() || !a.controls.is_empty())
            .collect::<Vec<_>>();
        ensure!(
            analyses.len() <= 4 && videos.len() <= 8,
            "当前后端最多4个分析实例、8个GPU节点"
        );
        let warnings = document
            .nodes
            .iter()
            .filter(|n| !visited.contains(&n.id))
            .map(|n| format!("{} 未连接到输出，不会执行", n.name))
            .collect();
        Ok(Compiled {
            document: document.clone(),
            videos,
            output_source,
            analyses,
            warnings,
        })
    };
    run().map_err(|error| {
        vec![Diagnostic {
            node: None,
            message: format!("{error:#}"),
        }]
    })
}

pub fn directory() -> Result<PathBuf> {
    let base = crate::platform::paths::require_local_app_data()?;
    ensure!(base.is_absolute(), "invalid application directory");
    Ok(base.join("OpenUUYC/graphs"))
}
pub fn path(id: &str, published: bool) -> Result<PathBuf> {
    let id = uuid::Uuid::parse_str(id)?;
    Ok(directory()?.join(format!(
        "{id}.{}",
        if published {
            "active.json"
        } else {
            "graph.json"
        }
    )))
}
pub fn list() -> Result<Vec<Document>> {
    let dir = directory()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for file in std::fs::read_dir(dir)?.take(128) {
        let p = file?.path();
        if p.to_string_lossy().ends_with(".graph.json")
            && let Ok(doc) = load(&p)
        {
            out.push(doc);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}
pub fn load(path: &Path) -> Result<Document> {
    ensure!(
        std::fs::metadata(path)?.len() <= MAX_FILE as u64,
        "图文件过大"
    );
    let bytes = std::fs::read(path)?;
    ensure!(bytes.len() <= MAX_FILE, "图文件过大");
    let document: Document = serde_json::from_slice(&bytes)?;
    ensure!(
        document.graph_format_version == 1
            && document.nodes.len() <= 64
            && document.edges.len() <= 256,
        "图文件版本或规模不受支持"
    );
    ensure!(
        document.nodes.iter().all(|n| n.inputs.len() <= 16
            && n.outputs.len() <= 16
            && n.position
                .iter()
                .all(|x| x.is_finite() && x.abs() <= 1_000_000.0)
            && n.name.len() <= 128),
        "节点布局或端口规模无效"
    );
    Ok(document)
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() <= MAX_FILE, "图文件过大");
    std::fs::create_dir_all(path.parent().context("graph directory")?)?;
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}
pub fn save(document: &Document) -> Result<()> {
    atomic_write(
        &path(&document.graph_id, false)?,
        &serde_json::to_vec_pretty(document)?,
    )
}
pub fn publish(document: &Document, catalog: &Catalog) -> Result<()> {
    compile(document, catalog).map_err(|d| {
        anyhow::anyhow!(
            d.into_iter()
                .map(|d| d.message)
                .collect::<Vec<_>>()
                .join("\n")
        )
    })?;
    save(document)?;
    let publication = Publication {
        revision: uuid::Uuid::new_v4().to_string(),
        document: document.clone(),
    };
    atomic_write(
        &path(&document.graph_id, true)?,
        &serde_json::to_vec(&publication)?,
    )
}
