//! The "send to controller" floating drop target, viewer-only: macOS has no
//! desktop-wide drag watcher in this port, so the target never starts.
use anyhow::{Result, bail};
use std::{path::PathBuf, sync::mpsc};

#[derive(Clone, Default, PartialEq)]
pub(crate) struct Model {
    pub epoch: u64,
    pub revision: u64,
    pub acknowledged: u64,
    pub enabled: bool,
    pub can_send: bool,
    pub status: Option<String>,
    pub items: Vec<Row>,
}

#[derive(Clone, PartialEq)]
pub(crate) struct Row {
    pub id: u64,
    pub name: String,
}

#[derive(Clone)]
pub(crate) struct Action {
    pub epoch: u64,
    pub revision: u64,
    pub sequence: u64,
    pub kind: ActionKind,
}
pub(crate) enum ActionKind {
    Add(Vec<PathBuf>),
    Remove(u64),
    Clear,
    Send,
}

pub(crate) struct SendTarget;
impl SendTarget {
    pub fn start(_send: mpsc::SyncSender<Action>) -> Result<Self> {
        bail!("macOS 暂不支持文件发送浮窗")
    }
    pub fn update(&self, _model: Model) {}
}
impl Drop for SendTarget {
    fn drop(&mut self) {}
}
