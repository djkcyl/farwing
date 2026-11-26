//! Session-owned draft queue. Dropping only stages paths; Send is the only
//! command that publishes a file offer. Queues never survive permission epochs.
use crate::{
    features::clipboard::{
        Clipboard,
        drag::{Submission, SubmissionState},
    },
    platform::drag_drop::send_target::{Action, ActionKind, Model, Row, SendTarget},
};
use std::{
    path::PathBuf,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

#[derive(Clone)]
struct Item {
    id: u64,
    path: PathBuf,
}
#[derive(Default)]
struct Draft {
    items: Vec<Item>,
    revision: u64,
    next: u64,
}
impl Draft {
    fn add(&mut self, paths: Vec<PathBuf>) -> Result<(), String> {
        if paths.len() > 256 {
            return Err("队列过大，请先发送一批".into());
        }
        let mut incoming = Vec::new();
        for path in paths {
            if !path.is_absolute() || path.file_name().is_none() {
                return Err("不能加入此路径".into());
            }
            if !self.items.iter().any(|i| i.path == path) && !incoming.contains(&path) {
                incoming.push(path);
            }
        }
        if self.items.len() + incoming.len() > 256
            || self
                .items
                .iter()
                .map(|i| i.path.as_os_str().len())
                .sum::<usize>()
                + incoming.iter().map(|p| p.as_os_str().len()).sum::<usize>()
                > 1024 * 1024
        {
            return Err("队列过大，请先发送一批".into());
        }
        if !incoming.is_empty() {
            for path in incoming {
                self.next = self.next.wrapping_add(1);
                self.items.push(Item {
                    id: self.next,
                    path,
                });
            }
            self.revision = self.revision.wrapping_add(1);
        }
        Ok(())
    }
    fn clear(&mut self) {
        if !self.items.is_empty() {
            self.items.clear();
            self.revision = self.revision.wrapping_add(1);
        }
    }
    fn remove(&mut self, id: u64) {
        let old = self.items.len();
        self.items.retain(|i| i.id != id);
        if self.items.len() != old {
            self.revision = self.revision.wrapping_add(1);
        }
    }
    fn selection(&self) -> Result<Vec<PathBuf>, String> {
        let mut names = std::collections::HashSet::new();
        for item in &self.items {
            let name = item
                .path
                .file_name()
                .ok_or("路径无效")?
                .to_string_lossy()
                .to_lowercase();
            if !names.insert(name) {
                return Err("队列里有同名项目，请分批发送".into());
            }
        }
        Ok(self.items.iter().map(|i| i.path.clone()).collect())
    }
}
struct Active {
    ticket: Arc<Submission>,
    paths: Vec<PathBuf>,
    settled: bool,
}
pub(super) struct Sender {
    target: SendTarget,
    incoming: mpsc::Receiver<Action>,
    draft: Draft,
    epoch: u64,
    acknowledged: u64,
    enabled: bool,
    active: Option<Active>,
    result_at: Option<Instant>,
    error: Option<String>,
}
impl Sender {
    pub fn new() -> anyhow::Result<Self> {
        let (send, incoming) = mpsc::sync_channel(8);
        Ok(Self {
            target: SendTarget::start(send)?,
            incoming,
            draft: Draft::default(),
            epoch: 0,
            acknowledged: 0,
            enabled: false,
            active: None,
            result_at: None,
            error: None,
        })
    }
    fn status(&mut self) -> Option<String> {
        let mut restored = None;
        let status = self
            .active
            .as_mut()
            .map(|active| match active.ticket.state() {
                SubmissionState::Preparing => "正在准备发送…".to_owned(),
                SubmissionState::Waiting => "等待主控接收…".to_owned(),
                SubmissionState::HandedOff => "正在发送 · 可继续添加下一批".to_owned(),
                SubmissionState::Complete { total, saved } => {
                    self.result_at.get_or_insert_with(Instant::now);
                    active.settled = true;
                    if total == saved && total > 0 {
                        format!("已发送 · {saved} 项")
                    } else {
                        format!("已保存 {saved}/{total} 项，请检查主控")
                    }
                }
                SubmissionState::Failed(error) => {
                    self.result_at.get_or_insert_with(Instant::now);
                    if !active.settled {
                        restored = Some(active.paths.clone());
                        active.settled = true;
                    }
                    error
                }
            });
        if let Some(paths) = restored {
            if let Err(error) = self.draft.add(paths) {
                self.error = Some(error);
            }
        }
        self.error.clone().or(status)
    }
    pub fn update(&mut self, clipboard: &Clipboard, enabled: bool) {
        let epoch = clipboard.epoch();
        if epoch != self.epoch || (self.enabled && !enabled) {
            self.draft.clear();
            self.active = None;
            self.error = None;
            self.result_at = None;
            self.acknowledged = 0;
        }
        self.epoch = epoch;
        self.enabled = enabled;
        let _ = self.status();
        while let Ok(action) = self.incoming.try_recv() {
            if !enabled || action.epoch != epoch || action.sequence <= self.acknowledged {
                continue;
            }
            self.acknowledged = action.sequence;
            let result = match action.kind {
                ActionKind::Add(paths) => self.draft.add(paths),
                ActionKind::Remove(id) => {
                    self.draft.remove(id);
                    Ok(())
                }
                ActionKind::Clear if action.revision == self.draft.revision => {
                    self.draft.clear();
                    Ok(())
                }
                ActionKind::Send if action.revision == self.draft.revision => {
                    if self.draft.items.is_empty() {
                        Ok(())
                    } else if !clipboard.drop_available() {
                        Err("正在等待上一批发送结果".into())
                    } else {
                        match self.draft.selection() {
                            Err(error) => Err(error),
                            Ok(paths) => match clipboard.send_files(paths.clone()) {
                                Ok(ticket) => {
                                    self.draft.clear();
                                    self.active = Some(Active {
                                        ticket,
                                        paths,
                                        settled: false,
                                    });
                                    self.result_at = None;
                                    Ok(())
                                }
                                Err(error) => Err(error.to_string()),
                            },
                        }
                    }
                }
                _ => Err("队列已变化，请重新点击".into()),
            };
            match result {
                Ok(()) => self.error = None,
                Err(error) => {
                    self.error = Some(error);
                    self.result_at = Some(Instant::now());
                }
            }
        }
        let mut status = self.status();
        if self
            .result_at
            .is_some_and(|at| at.elapsed() > Duration::from_secs(8))
        {
            if self.active.as_ref().is_some_and(|a| a.settled) {
                self.active = None;
            }
            self.error = None;
            self.result_at = None;
            status = None;
        }
        self.target.update(Model {
            epoch,
            revision: self.draft.revision,
            acknowledged: self.acknowledged,
            enabled,
            can_send: enabled && !self.draft.items.is_empty() && clipboard.drop_available(),
            status,
            items: self
                .draft
                .items
                .iter()
                .map(|i| Row {
                    id: i.id,
                    name: i
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                })
                .collect(),
        });
    }
}
