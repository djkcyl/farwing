//! Shared diagnostic archive limits, paths and progress.
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{Exported, Input, Task};

use super::logging;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub(crate) const ARCHIVE_LIMIT: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct Progress {
    pub stage: String,
    /// What `stage` says, for sending to another device (which words it).
    #[serde(skip)]
    pub step: Step,
    pub completed: u64,
    pub total: u64,
}

/// The steps of preparing a bundle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Step {
    #[default]
    Other,
    Collecting,
    ReadingHardware,
    CheckingComponents,
    /// Log `index` (from 1) of `count`.
    PackingLogs { index: u32, count: u32 },
    Finishing,
}

impl Step {
    /// The key sent for this step, and its log numbers.
    #[cfg(windows)]
    pub(crate) fn key(self) -> (&'static str, u32, u32) {
        match self {
            Self::Other => ("", 0, 0),
            Self::Collecting => ("collecting", 0, 0),
            Self::ReadingHardware => ("reading_hardware", 0, 0),
            Self::CheckingComponents => ("checking_components", 0, 0),
            Self::PackingLogs { index, count } => ("packing_logs", index, count),
            Self::Finishing => ("finishing", 0, 0),
        }
    }

    /// A received step in the interface language.
    pub(crate) fn text_of(key: &str, index: u32, count: u32) -> String {
        let text = &crate::i18n::t().session;
        match key {
            "collecting" => text.diag_collecting.into(),
            "reading_hardware" => text.diag_reading_hardware.into(),
            "checking_components" => text.diag_checking_components.into(),
            "packing_logs" => (text.diag_packing_logs)(index as usize, count as usize),
            "finishing" => text.diag_finishing.into(),
            _ => text.rdiag_waiting_remote.into(),
        }
    }
}
impl Default for Progress {
    fn default() -> Self {
        Self {
            stage: crate::i18n::t().session.diag_collecting.into(),
            step: Step::Collecting,
            completed: 0,
            total: 0,
        }
    }
}
impl Progress {
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.completed as f64 / self.total as f64).clamp(0.0, 1.0) as f32
        }
    }
}

pub(crate) fn open_folder(path: &Path) -> Result<()> {
    let parent = path.parent().context(crate::i18n::t().session.diag_path_invalid)?;
    logging::open_folder(parent)
}

pub(crate) fn output_directory() -> Result<PathBuf> {
    let snapshot = logging::snapshot().context(crate::i18n::t().session.logging_not_ready)?;
    Ok(snapshot
        .directory
        .parent()
        .context(crate::i18n::t().session.diag_output_folder_invalid)?
        .join("diagnostics"))
}
