//! Versioned detection ABI. Callbacks are serial; all borrowed buffers expire on return.
//! The module owns its instance. Panics and exceptions must not cross these functions.
use core::ffi::c_void;

pub const ABI_VERSION: u32 = 1;
pub const MAX_DETECTIONS: usize = 128;
pub const MAX_EDGE: u32 = 640;

#[repr(C)]
pub struct Frame {
    pub struct_size: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub sequence: u64,
    /// Packed RGBA8, upright, full-range RGB. No plugin/UI overlay is included.
    pub rgba: *const u8,
    pub len: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Detection {
    /// Normalized upright input coordinates, xyxy, in [0, 1].
    pub bounds: [f32; 4],
    pub confidence: f32,
    pub class_id: u32,
    /// UTF-8; unused bytes are zero. No pointers in output records.
    pub label: [u8; 32],
}

#[repr(C)]
pub struct PluginApi {
    pub abi_version: u32,
    pub struct_size: u32,
    /// JSON configuration; host-resolved file paths are absolute UTF-8.
    pub create:
        unsafe extern "C" fn(*const u8, usize, *const HostServices, *mut *mut c_void) -> i32,
    /// A null frame requests the initial scene, without inference. Output is UTF-8
    /// JSON `Scene`; the host owns the output buffer. No Rust objects cross FFI.
    pub process:
        Option<unsafe extern "C" fn(*mut c_void, *const Frame, *mut u8, usize, *mut usize) -> i32>,
    /// Optional provider, queried only for a declared inference dependency.
    pub inference: Option<unsafe extern "C" fn(*mut c_void) -> InferenceService>,
    pub render: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *const u8,
            usize,
            *const Viewport,
            *mut u8,
            usize,
            *mut usize,
        ) -> i32,
    >,
    pub destroy: unsafe extern "C" fn(*mut c_void),
}

pub type Query = unsafe extern "C" fn(u32) -> *const PluginApi;

#[repr(C)]
pub struct HostServices {
    pub struct_size: u32,
    pub inference: *const InferenceService,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct InferenceService {
    pub abi_version: u32,
    pub context: *mut c_void,
    pub open: unsafe extern "C" fn(*mut c_void, *const u8, usize, *mut *mut c_void) -> i32,
    pub run: unsafe extern "C" fn(*mut c_void, *const TensorInput, *mut TensorOutput) -> i32,
    pub close: unsafe extern "C" fn(*mut c_void),
}

#[repr(C)]
pub struct TensorInput {
    pub data: *const f32,
    pub len: usize,
    pub shape: [u32; 4],
}

#[repr(C)]
pub struct TensorOutput {
    pub data: *mut f32,
    pub capacity: usize,
    pub len: usize,
    pub shape: [u32; 4],
    pub rank: u32,
}

pub const MAX_SCENE_BYTES: usize = 1024 * 1024;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Space {
    /// Upright visible video coordinates, normalized independently on each axis.
    Video,
    /// Device-independent local pixels relative to a normalized video anchor.
    View { anchor: [f32; 2] },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Shape {
    Line {
        from: [f32; 2],
        to: [f32; 2],
    },
    Rect {
        min: [f32; 2],
        max: [f32; 2],
        fill: bool,
    },
    Ellipse {
        center: [f32; 2],
        radii: [f32; 2],
        fill: bool,
    },
    Polyline {
        points: Vec<[f32; 2]>,
        closed: bool,
    },
    Text {
        position: [f32; 2],
        text: String,
        size: f32,
    },
    Image {
        resource: u32,
        min: [f32; 2],
        max: [f32; 2],
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Draw {
    pub space: Space,
    pub color: [u8; 4],
    /// Width in device-independent local pixels, independent of video scaling.
    pub width: f32,
    pub shape: Shape,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Layer {
    pub id: String,
    pub name: String,
    /// Static HUD survives inference gaps. Frame-linked annotations must be false.
    pub persistent: bool,
    pub default_enabled: bool,
    pub commands: Vec<Draw>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageResource {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    /// Straight-alpha RGBA8. Initial response only; IDs immutable until restart.
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Scene {
    #[serde(default)]
    pub resources: Vec<ImageResource>,
    pub layers: Vec<Layer>,
    #[serde(default)]
    pub detections: Vec<Detection>,
    #[serde(default)]
    pub input: Vec<InputCommand>,
    /// Supplied by the host to control callbacks, never trusted from analysis output.
    #[serde(default)]
    pub control_active: bool,
    #[serde(default)]
    pub control_epoch: u64,
    /// Host observation identity and local input telemetry. Not a remote injection ACK.
    #[serde(default)]
    pub observation: Option<Observation>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Observation {
    pub generation: u64,
    pub sequence: u64,
    pub physical_motion: [i64; 2],
    pub current_physical: [i64; 2],
    pub submitted_motion: [i64; 2],
    pub motion_pending: bool,
    pub assist_pending: bool,
}

/// Declarative remote input. The host enforces focus, an explicit lease, age,
/// bounds and button-release cleanup. This interface never injects local OS input.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputCommand {
    Relative {
        x: i32,
        y: i32,
    },
    /// A bounded correction; compensate this fraction of physical motion since sampling.
    Correction {
        x: i32,
        y: i32,
        weight: [f32; 2],
        /// Use physical-motion totals at request dispatch instead of frame capture.
        #[serde(default)]
        at_dispatch: bool,
    },
    Click {
        hold_ms: u32,
    },
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    /// Dimensions and video rectangle in logical local pixels (points).
    pub width: f32,
    pub height: f32,
    pub scale: f32,
    pub video: [f32; 4],
    pub revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Vertex {
    pub position: [f32; 2],
    pub uv: [f32; 2],
    pub color: [u8; 4],
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mesh {
    pub texture: u64,
    pub clip: [f32; 4],
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MeshLayer {
    pub id: String,
    pub name: String,
    pub persistent: bool,
    pub default_enabled: bool,
    pub meshes: Vec<Mesh>,
    #[serde(default)]
    pub composite_order: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextureUpload {
    pub id: u64,
    pub position: Option<[usize; 2]>,
    pub size: [usize; 2],
    pub pixels: Vec<[u8; 4]>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Rendered {
    pub textures: Vec<TextureUpload>,
    pub layers: Vec<MeshLayer>,
}
pub const MAX_RENDER_BYTES: usize = 8 * 1024 * 1024;

/// Separate extension: the existing analysis ABI remains binary-compatible.
#[repr(C)]
pub struct VideoApi {
    pub abi_version: u32,
    pub struct_size: u32,
    pub describe: unsafe extern "C" fn(*const u8, usize, *mut u8, usize, *mut usize) -> i32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoProgram {
    pub backend: String,
    pub passes: Vec<VideoPass>,
    /// Explicit presentation delay, in nominal source-frame periods.
    pub delay_frames: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoPass {
    /// D3D11 ps_5_0 bytecode. t0=current, t2=previous, s0=linear sampler;
    /// b1=[phase,input_width,input_height,history_valid], b2=params.
    pub shader: Vec<u8>,
    pub scale_numerator: u32,
    pub scale_denominator: u32,
    pub params: [f32; 4],
    /// Sorted interpolation fractions in (0,1], ending at 1.0. Missing history
    /// emits only phase 1.0. Intermediate outputs are always synthetic.
    pub phases: Vec<f32>,
}

/// Declarative node catalog. Runtime port IDs never depend on UI names.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PortType {
    Frame,
    DrawList,
    Layer,
    Detections,
    InputCommands,
    Activation,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodePort {
    pub id: String,
    pub name: String,
    pub data_type: PortType,
    #[serde(default)]
    pub many: bool,
    #[serde(default)]
    pub optional: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeImplementation {
    VideoShader,
    FrameAnalysis,
    SceneSource,
    Overlay,
    DetectionControl,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeDefinition {
    pub type_id: String,
    pub schema_version: u32,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: String,
    /// External overlay node used to materialize this node's Layer output.
    #[serde(default)]
    pub layer_renderer: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub config: serde_json::Value,
    #[serde(default)]
    pub config_labels: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub config_schema: std::collections::BTreeMap<String, ParameterField>,
    pub implementation: NodeImplementation,
    pub inputs: Vec<NodePort>,
    pub outputs: Vec<NodePort>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ParameterChoice {
    pub label: String,
    pub value: serde_json::Value,
    #[serde(default)]
    pub updates: std::collections::BTreeMap<String, serde_json::Value>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ParameterField {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub order: u32,
    #[serde(default)]
    pub options: Vec<ParameterChoice>,
    #[serde(default)]
    pub extension: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub source_choice: String,
    #[serde(default)]
    pub reset: std::collections::BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub step: Option<f64>,
}
/// A read-only descriptor section. Hosts inspect it without loading native code.
pub const MANIFEST_MAGIC: &[u8; 8] = b"FWPLMETA";
pub const MANIFEST_HEADER: usize = 16;
pub const MAX_MANIFEST_BYTES: usize = 65536;

#[doc(hidden)]
pub const fn manifest_section<const N: usize>(json: &[u8]) -> [u8; N] {
    assert!(json.len() <= MAX_MANIFEST_BYTES && N == MANIFEST_HEADER + json.len());
    let mut data = [0; N];
    let mut i = 0;
    while i < 8 {
        data[i] = MANIFEST_MAGIC[i];
        i += 1;
    }
    data[8] = 1; // Descriptor container version, independent of the execution ABI.
    let length = (json.len() as u32).to_le_bytes();
    i = 0;
    while i < 4 {
        data[12 + i] = length[i];
        i += 1;
    }
    i = 0;
    while i < json.len() {
        data[MANIFEST_HEADER + i] = json[i];
        i += 1;
    }
    data
}

/// Embed exactly one manifest in a plugin library. The JSON is build input only.
#[macro_export]
macro_rules! embed_manifest {
    ($json:expr) => {
        const _: () = {
            const JSON: &[u8] = $json;
            #[used]
            #[unsafe(no_mangle)]
            #[unsafe(link_section = ".fwmeta")]
            pub static farwing_plugin_manifest_v1: [u8; $crate::MANIFEST_HEADER + JSON.len()] =
                $crate::manifest_section::<{ $crate::MANIFEST_HEADER + JSON.len() }>(JSON);
        };
    };
}

/// Query a specific node's vtable. The UTF-8 type ID is borrowed for this call only.
pub type NodeQuery = unsafe extern "C" fn(u32, *const u8, usize) -> *const PluginApi;
pub type VideoNodeQuery = unsafe extern "C" fn(u32, *const u8, usize) -> *const VideoApi;
