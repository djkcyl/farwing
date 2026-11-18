//! Explicit clipboard-file imports share the transfer receiver's path, temporary
//! file and atomic finalization rules. No separate unrestricted disk writer.
use super::{protocol::FileInfo, storage};
use anyhow::{Context, Result, ensure};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
pub(crate) struct Item {
    pub info: FileInfo,
    pub directory: bool,
}
pub(crate) struct Report {
    pub total: u32,
    pub saved: u32,
    pub first: String,
}
pub(crate) fn destination(path: &str) -> Result<PathBuf> {
    ensure!(path.len() <= 32768 && !path.contains('\0'), "接收目录无效");
    if path.is_empty() {
        storage::known_folder(storage::KnownFolder::Downloads).context("用户下载目录不可用")
    } else {
        storage::canonical_dir(&storage::local_path(path)?)
    }
}
pub(crate) async fn receive(
    root: PathBuf,
    items: Vec<Item>,
    permitted: impl Fn() -> bool,
    mut read: impl FnMut(usize, u64, usize) -> Result<Vec<u8>>,
) -> Result<Report> {
    storage::validate_manifest(&items.iter().map(|i| i.info.clone()).collect::<Vec<_>>())?;
    let root = storage::canonical_dir(&root)?;
    let mut report = Report {
        total: items.len().try_into()?,
        saved: 0,
        first: String::new(),
    };
    let key = uuid::Uuid::new_v4().to_string();
    for (index, item) in items.iter().enumerate() {
        if !permitted() {
            break;
        }
        let result = async {
            let relative = storage::safe_relative(&item.info.rel_path)?;
            if item.directory {
                let _locks = storage::parents(&root, &relative, true)?;
                let path = root.join(&relative);
                match std::fs::create_dir(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e.into()),
                }
                storage::canonical_dir(&path)?;
            } else {
                let mut receiving = storage::prepare(&root, &key, &item.info, 2, None)?
                    .context("文件接收未创建")?;
                let partial = receiving.partial.clone();
                let transfer = async {
                    while receiving.position < item.info.size {
                        ensure!(permitted(), "文件接收已取消");
                        let wanted = (item.info.size - receiving.position).min(512000) as usize;
                        let bytes = read(index, receiving.position, wanted)?;
                        ensure!(
                            !bytes.is_empty() && bytes.len() <= wanted,
                            "文件内容提前结束或超出清单"
                        );
                        receiving.file.write_all(&bytes).await?;
                        receiving.position += bytes.len() as u64;
                    }
                    ensure!(permitted(), "文件接收已取消");
                    storage::finish(receiving, &root, &key, 2).await?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                if transfer.is_err() {
                    let _ = storage::cleanup(&root, &key, &[partial]);
                }
                transfer?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        match result {
            Ok(()) => {
                report.saved += 1;
                if report.first.is_empty() {
                    report.first = item.info.rel_path.clone();
                }
            }
            Err(error) => tracing::warn!(%error,"drag file import failed"),
        }
    }
    Ok(report)
}
