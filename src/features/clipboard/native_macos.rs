//! macOS clipboard adapter stub, signature-faithful to `native_linux.rs`.
//! The real arboard/NSPasteboard implementation is the next phase; every
//! entry point either succeeds as a no-op or reports "not yet wired".
use super::*;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) use super::formats::safe_name;

pub(super) enum Command {
    Activate(Weak<Inner>),
    DropFiles(
        Arc<super::drag::Submission>,
        PreparedFiles,
        ClipboardFormatListRequestKind,
    ),
    Remove(u64),
    Offer(
        Weak<Inner>,
        u64,
        Vec<ClipboardFormat>,
        Option<ClipboardFormatListRequestKind>,
    ),
    Request(Weak<Inner>, u64, i64, ClipboardRequestKind),
    Text(Weak<Inner>, u64, i64, String),
    PublishFiles(
        Weak<Inner>,
        u64,
        PreparedFiles,
        tokio::sync::oneshot::Sender<Result<(), String>>,
    ),
}

/// Local files picked for a drag or a drop. Never produced here yet.
pub(super) struct PreparedFiles {
    summary: FileSummary,
}
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileSummary {
    pub count: u32,
    pub bytes: u64,
    pub first_name: String,
}
impl PreparedFiles {
    pub fn summary(&self) -> FileSummary {
        self.summary.clone()
    }
}
pub(super) fn prepare_files(
    _paths: Vec<PathBuf>,
    _allowed: impl Fn() -> bool,
) -> Result<PreparedFiles> {
    bail!("macOS clipboard files: not yet wired")
}

/// Files a peer offers; on macOS none are ever received, so the inner state
/// only carries what `drag.rs` reads directly (`session`, `epoch`).
pub(super) struct RemoteOffer {
    pub(super) session: Arc<Inner>,
    pub(super) epoch: u64,
}
#[derive(Clone)]
pub(crate) struct FileOffer(pub(super) Arc<RemoteOffer>);
pub(crate) struct FileOfferStatus {
    pub bytes_read: u64,
    pub completed: Option<Result<(), String>>,
}
impl FileOffer {
    pub fn prepare(&self) -> Result<FileSummary> {
        bail!("macOS clipboard files: not yet wired")
    }
    pub fn object(&self) -> Result<crate::platform::drag_drop::Files> {
        bail!("macOS clipboard files: not yet wired")
    }
    pub fn dragging(&self, _value: bool) {}
    pub fn cancel(&self) {
        self.0.session.cancel_pending();
    }
    pub(super) fn valid(&self) -> bool {
        false
    }
    pub(super) fn manifest(&self) -> Result<Vec<ClipboardFileDescriptor>> {
        bail!("macOS clipboard files: not yet wired")
    }
    pub(super) fn read(&self, _index: usize, _offset: u64, _length: usize) -> Result<Vec<u8>> {
        bail!("macOS clipboard files: not yet wired")
    }
    pub fn status(&self) -> FileOfferStatus {
        FileOfferStatus {
            bytes_read: 0,
            completed: Some(Err("macOS clipboard files: not yet wired".into())),
        }
    }
}

pub(super) fn start() -> Result<()> {
    Ok(())
}
pub(super) fn post(_command: Command) -> Result<()> {
    Ok(())
}
pub(super) fn pump() {}
pub(super) fn shutdown() {}
