//! Task-owned progress snapshots; no GUI or notification I/O in transfer loops.
use super::*;
use crate::i18n::t;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) const RUNNING: i32 = 0;
pub(crate) const COMPLETE: i32 = 1;
pub(crate) const FAILED: i32 = 2;
pub(crate) const PAUSED: i32 = 3;
pub(crate) const CANCELLED: i32 = 4;
pub(crate) const INTERRUPTED: i32 = 5;
#[derive(Clone, PartialEq, prost::Message, serde::Serialize, serde::Deserialize)]
pub(crate) struct Notice {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(bool, tag = "2")]
    pub receiving: bool,
    #[prost(string, tag = "3")]
    pub name: String,
    #[prost(uint64, tag = "4")]
    pub total: u64,
    #[prost(uint64, tag = "5")]
    pub done: u64,
    #[prost(int32, tag = "6")]
    pub state: i32,
    #[prost(string, tag = "7")]
    pub folder: String,
    #[prost(string, tag = "8")]
    pub error: String,
    #[prost(int64, tag = "9")]
    pub updated: i64,
    #[prost(uint32, tag = "10")]
    pub saved: u32,
}
impl Notice {
    pub fn interrupt(&mut self) {
        if self.state == RUNNING {
            self.state = INTERRUPTED;
            self.updated = chrono::Utc::now().timestamp();
        }
    }
}
#[derive(Clone, Default)]
pub(crate) struct Journal(Arc<Mutex<Vec<Notice>>>);
impl Journal {
    pub fn publish(&self, value: Notice) {
        let mut values = super::super::lock(&self.0);
        if let Some(old) = values.iter_mut().find(|v| v.id == value.id) {
            *old = value;
        } else {
            if values.len() >= 32 {
                values.remove(0);
            }
            values.push(value);
        }
    }
    pub fn take(&self) -> Vec<Notice> {
        std::mem::take(&mut *super::super::lock(&self.0))
    }
    pub fn begin(&self, receiving: bool) -> Progress {
        let notice = Notice {
            id: uuid::Uuid::new_v4().simple().to_string(),
            receiving,
            name: t().features.transfer_notice_files.into(),
            updated: chrono::Utc::now().timestamp(),
            ..Default::default()
        };
        self.publish(notice.clone());
        Progress {
            journal: self.clone(),
            notice,
            last: Instant::now(),
        }
    }
}
pub(crate) struct Progress {
    journal: Journal,
    notice: Notice,
    last: Instant,
}
impl Progress {
    pub fn describe(&mut self, items: &[FileInfo], folder: &std::path::Path) {
        let files = items.iter().filter(|f| !f.directory).collect::<Vec<_>>();
        self.notice.total = files.iter().fold(0u64, |n, f| n.saturating_add(f.size));
        self.notice.name = files
            .first()
            .map(|f| {
                std::path::Path::new(&f.rel_path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_else(|| t().features.transfer_empty_folder.into());
        if files.len() > 1 {
            self.notice.name = (t().features.transfer_files_and_more)(&self.notice.name, files.len());
        }
        self.notice.folder = folder.to_string_lossy().into_owned();
        self.emit();
    }
    pub fn advance(&mut self, bytes: u64) {
        self.notice.done = self
            .notice
            .done
            .saturating_add(bytes)
            .min(self.notice.total);
        if self.last.elapsed() >= Duration::from_millis(250) {
            self.emit();
        }
    }
    pub fn saved(&mut self) {
        self.notice.saved = self.notice.saved.saturating_add(1);
        self.emit();
    }
    pub fn terminal(&mut self, state: i32, error: String) {
        if self.notice.state != RUNNING {
            return;
        }
        self.notice.state = state;
        self.notice.error = error.chars().take(180).collect();
        self.emit();
    }
    fn emit(&mut self) {
        self.notice.updated = chrono::Utc::now().timestamp();
        self.last = Instant::now();
        self.journal.publish(self.notice.clone());
    }
}
impl Drop for Progress {
    fn drop(&mut self) {
        if self.notice.state == RUNNING {
            self.terminal(INTERRUPTED, String::new());
        }
    }
}
