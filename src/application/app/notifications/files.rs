use super::{Card, TransferCard, clean};
use crate::features::file_transfer::host::notices::*;
pub(super) fn cards(notices: Vec<Notice>, now: i64) -> Vec<(String, Card)> {
    notices
        .into_iter()
        .filter(|n| n.state == RUNNING || n.updated.saturating_add(25) > now)
        .map(|n| {
            let title = match (n.receiving, n.state) {
                (true, RUNNING) => "正在接收文件",
                (false, RUNNING) => "正在发送文件",
                (true, COMPLETE) => "文件接收完成",
                (false, COMPLETE) => "文件发送完成",
                (_, FAILED) => "文件传输失败",
                (_, PAUSED) => "文件传输已暂停",
                (_, CANCELLED) => "文件传输已取消",
                _ => "文件传输已中断",
            };
            let detail = if !n.error.is_empty() {
                clean(&n.error)
            } else {
                match n.state {
                    RUNNING => format!("{} / {}", bytes(n.done), bytes(n.total)),
                    COMPLETE if n.saved == 0 => "已处理，未新增文件".into(),
                    COMPLETE => format!("{} · {} 个文件", bytes(n.done), n.saved),
                    PAUSED => "可从传输端继续任务".into(),
                    CANCELLED => "传输已取消".into(),
                    _ => "连接或接收权限已中断".into(),
                }
            };
            let progress = if n.state == RUNNING {
                Some(if n.total == 0 {
                    0
                } else {
                    ((n.done.min(n.total) as u128 * 1000) / n.total as u128) as u16
                })
            } else {
                None
            };
            let folder = (!n.folder.is_empty() && (n.state == COMPLETE || n.saved > 0))
                .then(|| std::path::PathBuf::from(&n.folder));
            (
                format!("file:{}:{}", n.id, n.state),
                Card {
                    disconnect: None,
                    ticket: String::new(),
                    title: title.into(),
                    body: clean(&n.name),
                    detail,
                    expires_at: if n.state == RUNNING {
                        0
                    } else {
                        n.updated.saturating_add(25)
                    },
                    confirmation: false,
                    busy: false,
                    connected: false,
                    transfer: Some(TransferCard { progress, folder }),
                },
            )
        })
        .collect()
}
fn bytes(n: u64) -> String {
    if n >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", n as f64 / (1024. * 1024. * 1024.))
    } else if n >= 1024 * 1024 {
        format!("{:.1} MiB", n as f64 / (1024. * 1024.))
    } else if n >= 1024 {
        format!("{:.1} KiB", n as f64 / 1024.)
    } else {
        format!("{n} B")
    }
}
