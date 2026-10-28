//! Typed persistent graph and compiler. No runtime is started while editing.
use super::*;
use crate::i18n::t;
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
        // Built-in labels are in the language current when the catalog loads.
        let text = &t().features;
        for (id, name, inputs, outputs) in [
            (
                RAW,
                text.plugin_node_source,
                vec![],
                vec![port(
                    "frames",
                    text.plugin_port_frames,
                    sdk::PortType::Frame,
                    false,
                )],
            ),
            (
                VIDEO,
                text.plugin_node_video_output,
                vec![port(
                    "video",
                    text.plugin_port_frames,
                    sdk::PortType::Frame,
                    false,
                )],
                vec![],
            ),
            (
                INPUT,
                text.plugin_node_remote_input,
                vec![port(
                    "commands",
                    text.plugin_port_input_commands,
                    sdk::PortType::InputCommands,
                    true,
                )],
                vec![],
            ),
            (
                OVERLAY,
                text.plugin_node_overlay_output,
                vec![port(
                    "layers",
                    text.plugin_port_layers,
                    sdk::PortType::Layer,
                    true,
                )],
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
                            RAW => text.plugin_node_source_description,
                            VIDEO => text.plugin_node_video_output_description,
                            INPUT => text.plugin_node_remote_input_description,
                            _ => text.plugin_node_overlay_output_description,
                        }
                        .into(),
                        category: if id == RAW {
                            text.plugin_category_sources
                        } else {
                            text.plugin_category_outputs
                        }
                        .into(),
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
                    sdk::NodeImplementation::VideoShader => (
                        text.plugin_category_image_processing,
                        text.plugin_description_image_processing,
                    ),
                    sdk::NodeImplementation::FrameAnalysis => (
                        text.plugin_category_video_analysis,
                        text.plugin_description_video_analysis,
                    ),
                    sdk::NodeImplementation::Overlay | sdk::NodeImplementation::SceneSource => (
                        text.plugin_category_drawing,
                        text.plugin_description_drawing,
                    ),
                    sdk::NodeImplementation::DetectionControl => (
                        text.plugin_category_input_control,
                        text.plugin_description_input_control,
                    ),
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
                    "{}",
                    text.plugin_node_namespace_invalid
                );
                ensure!(
                    definition.config.is_object()
                        && definition.dependencies.len() <= 8
                        && definition.dependencies.iter().all(|id| valid_id(id)),
                    "{}",
                    text.plugin_node_config_invalid
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
                    "{}",
                    text.plugin_node_contract_unsupported
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
                    "{}",
                    text.plugin_node_port_id_duplicate
                );
                ensure!(
                    definition
                        .inputs
                        .iter()
                        .chain(&definition.outputs)
                        .all(|p| valid_id(&p.id) && p.name.len() <= 128),
                    "{}",
                    text.plugin_port_invalid
                );
                ensure!(
                    !result.types.contains_key(&definition.type_id),
                    "{}",
                    text.plugin_node_type_duplicate
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
        let ty = self
            .types
            .get(type_id)
            .context(t().features.plugin_node_type_missing)?;
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
            name: t().features.plugin_graph_default_name.into(),
            nodes: vec![source, output, overlay],
            edges: vec![edge],
        })
    }
    pub fn connect(&mut self, catalog: &Catalog, from: Endpoint, to: Endpoint) -> Result<()> {
        ensure!(from.node != to.node, "{}", t().features.plugin_connect_self);
        let source = self
            .nodes
            .iter()
            .find(|n| n.id == from.node)
            .context(t().features.plugin_output_node_missing)?;
        let destination = self
            .nodes
            .iter()
            .find(|n| n.id == to.node)
            .context(t().features.plugin_input_node_missing)?;
        let a = catalog
            .ports(source, true)
            .iter()
            .find(|p| p.id == from.port)
            .context(t().features.plugin_output_port_missing)?;
        let b = catalog
            .ports(destination, false)
            .iter()
            .find(|p| p.id == to.port)
            .context(t().features.plugin_input_port_missing)?;
        ensure!(
            a.data_type == b.data_type,
            "{}",
            t().features.plugin_port_type_mismatch
        );
        let many = b.many;
        // Reject a cycle before replacing any existing input connection.
        let mut pending = vec![to.node.clone()];
        let mut seen = BTreeSet::new();
        while let Some(id) = pending.pop() {
            ensure!(id != from.node, "{}", t().features.plugin_connect_cycle);
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
        ensure!(self.edges.len() < 256, "{}", t().features.plugin_edge_limit);
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
            "{}",
            t().features.plugin_graph_version_invalid
        );
        ensure!(
            document.nodes.len() <= 64 && document.edges.len() <= 256 && document.name.len() <= 128,
            "{}",
            t().features.plugin_graph_too_large
        );
        let mut nodes = BTreeMap::new();
        for node in &document.nodes {
            ensure!(
                uuid::Uuid::parse_str(&node.id).is_ok()
                    && nodes.insert(node.id.clone(), node).is_none(),
                "{}",
                t().features.plugin_node_id_invalid
            );
            ensure!(
                node.position.iter().all(|p| p.is_finite())
                    && serde_json::to_vec(&node.parameters)?.len() <= 65536,
                "{}",
                t().features.plugin_node_layout_invalid
            );
        }
        let mut edge_ids = BTreeSet::new();
        let mut connections = BTreeSet::new();
        let mut incoming: BTreeMap<(String, String), Vec<&Edge>> = BTreeMap::new();
        for edge in &document.edges {
            ensure!(
                edge.reroutes.len() <= 32 && edge.reroutes.iter().flatten().all(|p| p.is_finite()),
                "{}",
                t().features.plugin_reroute_invalid
            );
            ensure!(
                edge_ids.insert(&edge.id),
                "{}",
                t().features.plugin_edge_id_duplicate
            );
            ensure!(
                connections.insert((
                    &edge.from.node,
                    &edge.from.port,
                    &edge.to.node,
                    &edge.to.port
                )),
                "{}",
                t().features.plugin_edge_duplicate
            );
            let source = nodes
                .get(&edge.from.node)
                .context(t().features.plugin_edge_source_missing)?;
            let target = nodes
                .get(&edge.to.node)
                .context(t().features.plugin_edge_target_missing)?;
            let a = catalog
                .ports(source, true)
                .iter()
                .find(|p| p.id == edge.from.port)
                .context(t().features.plugin_edge_output_port_missing)?;
            let b = catalog
                .ports(target, false)
                .iter()
                .find(|p| p.id == edge.to.port)
                .context(t().features.plugin_edge_input_port_missing)?;
            ensure!(
                a.data_type == b.data_type,
                "{}",
                t().features.plugin_port_type_mismatch
            );
            let list = incoming
                .entry((edge.to.node.clone(), edge.to.port.clone()))
                .or_default();
            list.push(edge);
            ensure!(
                b.many || list.len() == 1,
                "{}",
                t().features.plugin_input_port_many_sources
            );
        }
        let video_sinks = document
            .nodes
            .iter()
            .filter(|n| n.enabled && n.type_id == VIDEO)
            .collect::<Vec<_>>();
        ensure!(
            video_sinks.len() <= 1,
            "{}",
            t().features.plugin_video_output_limit
        );
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
            ensure!(
                visiting.insert(id.into()),
                "{}",
                t().features.plugin_graph_cycle
            );
            let node = nodes.get(id).context(t().features.plugin_node_missing)?;
            let ty = catalog
                .types
                .get(&node.type_id)
                .with_context(|| (t().features.plugin_node_type_unavailable)(&node.type_id))?;
            ensure!(
                node.type_version == ty.definition.schema_version,
                "{}",
                t().features.plugin_node_version_incompatible
            );
            if node.enabled
                && let Some(manifest) = &ty.manifest
            {
                ensure!(
                    manifest
                        .dependencies
                        .iter()
                        .all(|id| catalog.providers.contains(id)),
                    "{}",
                    (t().features.plugin_inference_missing)(&ty.definition.name)
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
                            "{}",
                            (t().features.plugin_port_not_connected)(&node.name, &port.name)
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
                        .context(t().features.plugin_video_input_no_frames)?;
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
                                .context(t().features.plugin_analysis_input_no_frames)?
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
        ensure!(
            input_edges.len() <= 4,
            "{}",
            t().features.plugin_input_sources_limit
        );
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
                    .context(t().features.plugin_control_source_missing)?
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
                    .context(t().features.plugin_analysis_source_missing)?
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
                    .context(t().features.plugin_layer_renderer_missing)?;
                let renderer = catalog
                    .types
                    .get(renderer_type)
                    .context(t().features.plugin_renderer_not_installed)?;
                ensure!(
                    renderer.definition.implementation == sdk::NodeImplementation::Overlay,
                    "{}",
                    t().features.plugin_renderer_type_invalid
                );
                let manifest = renderer
                    .manifest
                    .as_ref()
                    .context(t().features.plugin_renderer_not_external)?;
                ensure!(
                    manifest
                        .dependencies
                        .iter()
                        .all(|id| catalog.providers.contains(id)),
                    "{}",
                    t().features.plugin_renderer_inference_missing
                );
                analyses
                    .get_mut(analysis)
                    .context(t().features.plugin_analysis_source_missing)?
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
            "{}",
            t().features.plugin_renderers_limit
        );
        let analyses = analyses
            .into_values()
            .filter(|a| !a.renderers.is_empty() || !a.controls.is_empty())
            .collect::<Vec<_>>();
        ensure!(
            analyses.len() <= 4 && videos.len() <= 8,
            "{}",
            t().features.plugin_backend_limit
        );
        let warnings = document
            .nodes
            .iter()
            .filter(|n| !visited.contains(&n.id))
            .map(|n| (t().features.plugin_node_unconnected)(&n.name))
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
    let base = crate::product::local_data_dir().context("LOCALAPPDATA unavailable")?;
    Ok(base.join("graphs"))
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
        "{}",
        t().features.plugin_graph_file_too_large
    );
    let bytes = std::fs::read(path)?;
    ensure!(
        bytes.len() <= MAX_FILE,
        "{}",
        t().features.plugin_graph_file_too_large
    );
    let document: Document = serde_json::from_slice(&bytes)?;
    ensure!(
        document.graph_format_version == 1
            && document.nodes.len() <= 64
            && document.edges.len() <= 256,
        "{}",
        t().features.plugin_graph_file_unsupported
    );
    ensure!(
        document.nodes.iter().all(|n| n.inputs.len() <= 16
            && n.outputs.len() <= 16
            && n.position
                .iter()
                .all(|x| x.is_finite() && x.abs() <= 1_000_000.0)
            && n.name.len() <= 128),
        "{}",
        t().features.plugin_graph_layout_invalid
    );
    Ok(document)
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() <= MAX_FILE,
        "{}",
        t().features.plugin_graph_file_too_large
    );
    crate::storage::write_bytes(path, bytes)
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
