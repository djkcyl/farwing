use super::{Card, TransferCard, clean};
use crate::features::file_transfer::host::notices::*;
use crate::i18n::t;
pub(super) fn cards(notices: Vec<Notice>, now: i64) -> Vec<(String, Card)> {
    let text = &t().app;
    notices
        .into_iter()
        .filter(|n| n.state == RUNNING || n.updated.saturating_add(25) > now)
        .map(|n| {
            let title = match (n.receiving, n.state) {
                (true, RUNNING) => text.receiving_files,
                (false, RUNNING) => text.sending_files,
                (true, COMPLETE) => text.files_received,
                (false, COMPLETE) => text.files_sent,
                (_, FAILED) => text.transfer_failed,
                (_, PAUSED) => text.transfer_paused,
                (_, CANCELLED) => text.transfer_cancelled_title,
                _ => text.transfer_interrupted,
            };
            let detail = if !n.error.is_empty() {
                clean(&n.error)
            } else {
                match n.state {
                    RUNNING => format!("{} / {}", bytes(n.done), bytes(n.total)),
                    COMPLETE if n.saved == 0 => text.no_new_files.into(),
                    COMPLETE => (text.files_saved)(&bytes(n.done), n.saved),
                    PAUSED => String::new(),
                    CANCELLED => text.transfer_cancelled.into(),
                    _ => text.transfer_access_lost.into(),
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
                    pairing: false,
                    browser: false,
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
