//! Local implementation preferences. Capabilities and hard limits remain separate.
use serde::{Deserialize, Serialize};
pub(crate) fn codec_rank(codec: i32) -> u8 {
    match codec {
        5 => 3,
        2 => 2,
        1 => 1,
        _ => 0,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn candidate_rank(
    size: (u32, u32),
    hardware: bool,
    format_matches: bool,
    fps: u32,
    adapter: u64,
    preferred: Option<u64>,
    source: u64,
    codec: i32,
    depth: u8,
) -> (u64, bool, bool, u32, bool, bool, u8, u8) {
    (
        u64::from(size.0) * u64::from(size.1),
        hardware,
        format_matches,
        fps,
        Some(adapter) == preferred,
        adapter == source,
        codec_rank(codec),
        depth,
    )
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SelectionReason {
    #[default]
    Automatic,
    Preferred,
    MissingGpu,
    GpuFormat,
    CodecFormat,
    FailedCandidate,
}
impl SelectionReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Automatic => crate::i18n::t().session.selection_automatic,
            Self::Preferred => crate::i18n::t().session.selection_preferred,
            Self::MissingGpu => crate::i18n::t().session.selection_missing_gpu,
            Self::GpuFormat => crate::i18n::t().session.selection_gpu_format,
            Self::CodecFormat => crate::i18n::t().session.selection_codec_format,
            Self::FailedCandidate => crate::i18n::t().session.selection_failed_candidate,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GpuId {
    pub vendor: u32,
    pub device: u32,
    pub subsystem: u32,
    pub revision: u32,
    pub location: [u32; 3],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessingMode {
    #[default]
    Automatic,
    Hardware,
    Software,
}
impl ProcessingMode {
    pub const ALL: [Self; 3] = [Self::Automatic, Self::Hardware, Self::Software];
    pub fn label(self) -> &'static str {
        match self {
            Self::Automatic => crate::i18n::t().session.processing_automatic,
            Self::Hardware => crate::i18n::t().session.processing_hardware,
            Self::Software => crate::i18n::t().session.processing_software,
        }
    }
    pub fn hardware(self) -> bool {
        self != Self::Software
    }
    pub fn software(self) -> bool {
        self != Self::Hardware
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DecoderPreference {
    pub mode: ProcessingMode,
    pub gpu: Option<GpuId>,
}
impl DecoderPreference {
    pub fn from_hardware(hardware: bool) -> Self {
        Self {
            mode: if hardware {
                ProcessingMode::Automatic
            } else {
                ProcessingMode::Software
            },
            gpu: None,
        }
    }
}

pub(crate) fn resolve_gpu(id: Option<GpuId>) -> Option<u64> {
    let id = id?;
    resolve_gpu_from(id, &crate::platform::capture::encoding_adapters().ok()?)
}
pub(crate) fn resolve_gpu_from(
    id: GpuId,
    adapters: &[crate::platform::capture::EncodingAdapter],
) -> Option<u64> {
    let matches: Vec<_> = adapters
        .iter()
        .filter(|adapter| adapter.id == Some(id))
        .collect();
    if matches.len() == 1 {
        Some(matches[0].luid)
    } else {
        tracing::warn!(
            ?id,
            matches = matches.len(),
            "preferred GPU unavailable or ambiguous; using automatic selection"
        );
        None
    }
}
