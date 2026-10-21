//! Controller side of FILE v2: management requests and the upload/download
//! state machines of one persisted task record.
use super::{
    model::{FileInfo, entry},
    service::{Direction, PartialFile, Record, Repository, TaskState},
    storage,
    wire::{self, Body, code, conflict},
    *,
};
use crate::transport::transfer::{CHUNK, kind};
use std::{path::PathBuf, time::Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// The remote browser's name for the drive and common-location list.
const ROOTS: &str = ":/";

fn extension(name: &str) -> String {
    std::path::Path::new(name)
        .extension()
        .filter(|v| !v.is_empty())
        .map(|v| format!(".{}", v.to_string_lossy()))
        .unwrap_or_default()
}

pub(super) async fn directory(
    wire: Arc<Transport>,
    path: String,
    stop: CancellationToken,
) -> Result<Vec<FileEntry>> {
    let roots = path.is_empty() || path == ROOTS;
    let request = wire::List {
        path: if roots { String::new() } else { path.clone() },
    };
    let Body::Listing(listing) = wire.request(Body::List(request), &stop).await? else {
        anyhow::bail!("目录响应类型不匹配");
    };
    ensure!(
        listing.entries.len() <= storage::MAX_FILES,
        "远端目录项目过多"
    );
    let base = if listing.path.is_empty() {
        path
    } else {
        listing.path
    };
    let mut entries = listing
        .entries
        .into_iter()
        .map(|e| -> Result<FileEntry> {
            ensure!(
                !e.name.is_empty()
                    && e.name.len() < 65536
                    && e.path.len() < 65536
                    && !e.name.contains('\0')
                    && !e.path.contains('\0'),
                "远端目录路径无效"
            );
            let full_path = if roots {
                ensure!(!e.path.is_empty(), "远端目录路径无效");
                e.path
            } else {
                ensure!(!e.name.contains(['\\', '/']), "远端目录路径无效");
                service::join_remote(&base, &e.name)
            };
            let entry_type = match e.kind {
                wire::kind::DIR => entry::DIRECTORY,
                wire::kind::DRIVE => entry::DRIVE,
                wire::kind::FILE => entry::FILE,
                _ => entry::FILE_LINK,
            };
            let icon_type = if !e.icon.is_empty() {
                e.icon
            } else if entry_type == entry::FILE {
                extension(&e.name)
            } else {
                String::new()
            };
            Ok(FileEntry {
                entry_type,
                name: e.name,
                size: e.size,
                modified_ns: e.mtime_ns,
                full_path,
                icon_type,
                hidden: e.hidden,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    entries.sort_by(|a, b| {
        (a.entry_type >= entry::FILE, a.name.to_lowercase())
            .cmp(&(b.entry_type >= entry::FILE, b.name.to_lowercase()))
    });
    Ok(entries)
}

pub(super) async fn make_dir(
    wire: Arc<Transport>,
    parent: String,
    name: String,
    stop: CancellationToken,
) -> Result<String> {
    ensure!(
        storage::safe_relative(&name)?.components().count() == 1,
        "文件夹名称无效"
    );
    match wire
        .request(Body::MakeDir(wire::MakeDir { parent, name }), &stop)
        .await?
    {
        Body::Done(done) => Ok(done.path),
        _ => anyhow::bail!("文件操作响应类型不匹配"),
    }
}

pub(super) async fn rename(
    wire: Arc<Transport>,
    path: String,
    name: String,
    stop: CancellationToken,
) -> Result<String> {
    match wire
        .request(Body::Rename(wire::Rename { path, name }), &stop)
        .await?
    {
        Body::Done(done) => Ok(done.path),
        _ => anyhow::bail!("文件操作响应类型不匹配"),
    }
}

pub(super) async fn remove(
    wire: Arc<Transport>,
    path: String,
    recursive: bool,
    stop: CancellationToken,
) -> Result<()> {
    match wire
        .request(Body::Remove(wire::Remove { path, recursive }), &stop)
        .await?
    {
        Body::Done(_) => Ok(()),
        _ => anyhow::bail!("文件操作响应类型不匹配"),
    }
}

/// Delete the host's partial files of a cancelled upload.
pub(super) async fn discard(wire: Arc<Transport>, key: String, stop: CancellationToken) -> Result<()> {
    let cancel = wire::Cancel {
        transfer: 0,
        key,
        discard: true,
    };
    match wire.request(Body::Cancel(cancel), &stop).await? {
        Body::Done(_) => Ok(()),
        _ => anyhow::bail!("文件操作响应类型不匹配"),
    }
}

async fn exists(
    wire: &Transport,
    dir: String,
    name: String,
    stop: &CancellationToken,
) -> Result<(bool, bool)> {
    let request = wire::Exists {
        dir,
        names: vec![name.clone()],
    };
    match wire.request(Body::Exists(request), stop).await? {
        Body::Existing(v) => Ok((v.present.contains(&name), v.writing.contains(&name))),
        _ => anyhow::bail!("同名检查响应类型不符"),
    }
}

/// Stop the host side of `transfer`; it keeps its partial files. Waiting for
/// the answer keeps a quick resume from racing the stopping transfer.
async fn stop_remote(wire: &Transport, transfer: u32) {
    let cancel = wire::Cancel {
        transfer,
        ..Default::default()
    };
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        wire.request(Body::Cancel(cancel), &CancellationToken::new()),
    )
    .await;
}

pub(super) async fn run(
    wire: Arc<Transport>,
    mut record: Record,
    repo: Arc<Repository>,
    stop: CancellationToken,
) {
    let outcome = async {
        record.state = TaskState::Running;
        record.error = None;
        repo.save(&record)?;
        match record.direction {
            Direction::Upload => upload(&wire, &stop, &mut record, &repo).await,
            Direction::Download => download(&wire, &stop, &mut record, &repo).await,
        }
    }
    .await;
    record.state = if outcome.is_ok() {
        if record.state == TaskState::Skipped {
            TaskState::Skipped
        } else {
            TaskState::Done
        }
    } else if stop.is_cancelled() {
        TaskState::Paused
    } else {
        TaskState::Failed
    };
    record.error = if stop.is_cancelled() && wire.supported() && wire.allowed() {
        None
    } else {
        outcome.err().map(|e| format!("{e:#}"))
    };
    record.confirmed = record
        .confirmed
        .max(repo.transferred(&record.key))
        .max(record.completed_bytes());
    if let Err(e) = repo.save(&record) {
        repo.report(format!("无法保存续传记录：{e:#}"));
    }
    tracing::info!(
        key = %record.key,
        state = record.state.label(),
        total = record.total,
        error = record.error.as_deref().unwrap_or(""),
        "file transfer task ended"
    );
    repo.progress(&record.key, record.confirmed, true);
}

fn finished(r: &Record, f: &FileInfo) -> bool {
    r.partial
        .iter()
        .any(|p| p.info.rel_path == f.rel_path && (p.done || p.skipped))
}

fn upsert(r: &mut Record, p: PartialFile) {
    if let Some(old) = r
        .partial
        .iter_mut()
        .find(|o| o.info.rel_path == p.info.rel_path)
    {
        *old = p;
    } else {
        r.partial.push(p);
    }
}

/// Saves per-item progress at most once a second; the task saves at its end.
struct Saver(Instant);
impl Saver {
    fn new() -> Self {
        Self(Instant::now())
    }
    fn save(&mut self, repo: &Repository, r: &Record) -> Result<()> {
        if self.0.elapsed() >= Duration::from_secs(1) {
            repo.save(r)?;
            self.0 = Instant::now();
        }
        Ok(())
    }
}

/// The next message of a transfer, or why it stopped.
async fn next(transfer: &mut Registration) -> Result<Inbound> {
    let stop = transfer.stream.stop.clone();
    let message = tokio::select! {
        biased;
        _ = stop.cancelled() => None,
        message = tokio::time::timeout(TIMEOUT, transfer.inbound.recv()) => Some(message),
    };
    message
        .with_context(|| transfer.failure())?
        .context("被控端长时间没有传输数据")?
        .context("文件连接已结束")
}

/// A failed `ItemDone` as an error naming the item.
fn item_failure(done: &wire::ItemDone, items: &[FileInfo]) -> anyhow::Error {
    let failure = wire::Failure::new(done.error_code, done.message.clone());
    match items.get(done.entry as usize) {
        Some(item) => anyhow::Error::new(failure).context(format!("{} 传输失败", item.rel_path)),
        None => failure.into(),
    }
}

async fn upload(
    wire: &Arc<Transport>,
    stop: &CancellationToken,
    r: &mut Record,
    repo: &Repository,
) -> Result<()> {
    if !r.initialized {
        let source = PathBuf::from(&r.source);
        let scan_stop = stop.clone();
        let (root, folder, files) =
            tokio::task::spawn_blocking(move || storage::scan(&source, &scan_stop)).await??;
        r.local_root = Some(root);
        r.folder = folder;
        r.files = files;
        r.initialized = true;
        r.total = storage::validate_manifest(&r.files)?;
        repo.save(r)?;
    }
    let root = r.local_root.clone().context("本地源目录无效")?;
    if !r.started {
        let name = if r.folder.is_empty() {
            r.files
                .first()
                .context("没有要发送的文件")?
                .rel_path
                .clone()
        } else {
            r.folder.clone()
        };
        let (present, writing) = exists(wire, r.destination.clone(), name, stop).await?;
        ensure!(!writing, "另一个任务正在写入同名文件");
        if present && r.policy == conflict::SKIP {
            r.state = TaskState::Skipped;
            return Ok(());
        }
    }
    let pending = r
        .files
        .iter()
        .filter(|f| !finished(r, f))
        .cloned()
        .collect::<Vec<_>>();
    if r.started && pending.is_empty() {
        return Ok(());
    }
    let mut transfer = wire.register(stop.clone())?;
    let request = wire::Upload {
        transfer: transfer.id,
        dir: r.destination.clone(),
        folder: r.folder.clone(),
        items: pending.iter().map(wire::Item::from).collect(),
        conflict: r.policy,
        key: r.key.clone(),
    };
    let Body::Plan(plan) = wire.request(Body::Upload(request), stop).await? else {
        anyhow::bail!("上传计划响应类型不匹配");
    };
    r.started = true;
    repo.save(r)?;
    let result = send_items(wire, stop, &mut transfer, r, repo, &root, &pending, plan).await;
    if result.is_err() {
        stop_remote(wire, transfer.id).await;
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn send_items(
    wire: &Arc<Transport>,
    stop: &CancellationToken,
    transfer: &mut Registration,
    r: &mut Record,
    repo: &Repository,
    root: &std::path::Path,
    pending: &[FileInfo],
    plan: wire::Plan,
) -> Result<()> {
    ensure!(
        plan.transfer == transfer.id && plan.items.len() == pending.len(),
        "上传计划与清单不符"
    );
    // The host's partial files count only when their tails match ours.
    let offsets = {
        let (root, items, plans) = (root.to_owned(), pending.to_vec(), plan.items.clone());
        tokio::task::spawn_blocking(move || {
            items
                .iter()
                .zip(&plans)
                .map(|(item, plan)| storage::confirm(&root, item, plan))
                .collect::<Result<Vec<_>>>()
        })
        .await??
    };
    for (item, plan) in pending.iter().zip(&plan.items) {
        if plan.skip {
            upsert(
                r,
                PartialFile {
                    target: item.rel_path.clone(),
                    info: item.clone(),
                    done: false,
                    skipped: true,
                },
            );
        }
    }
    repo.save(r)?;
    tracing::info!(
        transfer = transfer.id,
        items = pending.len(),
        skipped = plan.items.iter().filter(|p| p.skip).count(),
        offered = plan.items.iter().map(|p| p.offset).sum::<u64>(),
        resumed = offsets.iter().sum::<u64>(),
        "file upload starting"
    );
    let start = wire::UploadStart {
        transfer: transfer.id,
        offsets: offsets.clone(),
    };
    let Body::Done(_) = wire.request(Body::UploadStart(start), stop).await? else {
        anyhow::bail!("上传开始响应类型不匹配");
    };
    let mut saver = Saver::new();
    let mut sent = r.completed_bytes();
    let mut unconfirmed = 0usize;
    let confirm = |message: Inbound, r: &mut Record, unconfirmed: &mut usize| -> Result<()> {
        let Inbound::ItemDone(done) = message else {
            anyhow::bail!("上传任务收到了意外的数据");
        };
        if done.error_code != code::OK {
            return Err(item_failure(&done, pending));
        }
        let item = pending
            .get(done.entry as usize)
            .context("被控端确认了未知的文件")?;
        upsert(
            r,
            PartialFile {
                target: item.rel_path.clone(),
                info: item.clone(),
                done: true,
                skipped: false,
            },
        );
        *unconfirmed = unconfirmed.checked_sub(1).context("被控端重复确认文件")?;
        Ok(())
    };
    for (index, item) in pending.iter().enumerate() {
        if plan.items[index].skip {
            continue;
        }
        let entry = index as u32;
        let offset = offsets[index];
        let mut file = if item.directory {
            None
        } else {
            let (root, item) = (root.to_owned(), item.clone());
            let file =
                tokio::task::spawn_blocking(move || storage::open_source(&root, &item)).await??;
            let mut file = tokio::fs::File::from_std(file);
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            Some(file)
        };
        let mut position = offset;
        sent += offset;
        loop {
            while let Ok(message) = transfer.inbound.try_recv() {
                confirm(message, r, &mut unconfirmed)?;
                saver.save(repo, r)?;
            }
            let remaining = item.size - position;
            let len = if remaining == 0 {
                0
            } else {
                wire.credit(transfer)
                    .await?
                    .min(remaining)
                    .min(CHUNK as u64) as usize
            };
            let mut data = vec![0; len];
            if let Some(file) = &mut file {
                tokio::select! {
                    biased;
                    _ = transfer.cancelled() => anyhow::bail!(transfer.failure()),
                    read = file.read_exact(&mut data) => { read.context("源文件读取失败")?; }
                }
            }
            let last = position + len as u64 == item.size;
            let chunk = Chunk {
                stream: transfer.id,
                kind: kind::FILE,
                offset: position,
                data: Bytes::from(data),
                last,
                entry,
            };
            wire.send_chunk(transfer, chunk).await?;
            position += len as u64;
            sent += len as u64;
            repo.progress(&r.key, sent, false);
            if last {
                break;
            }
        }
        unconfirmed += 1;
    }
    // Every byte is out; the host confirms each saved item.
    while unconfirmed > 0 {
        let message = next(transfer).await.context("等待被控端保存文件")?;
        confirm(message, r, &mut unconfirmed)?;
        saver.save(repo, r)?;
    }
    Ok(())
}

async fn download(
    wire: &Arc<Transport>,
    stop: &CancellationToken,
    r: &mut Record,
    repo: &Repository,
) -> Result<()> {
    if !r.started {
        let name = r
            .source
            .trim_end_matches(['\\', '/'])
            .rsplit(['\\', '/'])
            .next()
            .context("远端源文件无效")?;
        let relative = storage::safe_relative(name)?;
        if r.policy == conflict::SKIP && PathBuf::from(&r.destination).join(relative).exists() {
            r.state = TaskState::Skipped;
            return Ok(());
        }
    }
    let mut transfer = wire.register(stop.clone())?;
    let request = wire::Download {
        transfer: transfer.id,
        paths: vec![r.source.clone()],
        key: r.key.clone(),
    };
    let Body::Manifest(manifest) = wire.request(Body::Download(request), stop).await? else {
        anyhow::bail!("下载清单响应类型不匹配");
    };
    let result = receive_items(wire, stop, &mut transfer, r, repo, manifest).await;
    if result.is_err() {
        stop_remote(wire, transfer.id).await;
    }
    result
}

async fn receive_items(
    wire: &Arc<Transport>,
    stop: &CancellationToken,
    transfer: &mut Registration,
    r: &mut Record,
    repo: &Repository,
    manifest: wire::Manifest,
) -> Result<()> {
    ensure!(manifest.transfer == transfer.id, "下载清单与任务不符");
    let files = manifest.items.iter().map(FileInfo::from).collect::<Vec<_>>();
    let total = storage::validate_manifest(&files)?;
    if r.initialized {
        ensure!(
            r.folder == manifest.folder
                && r.files.len() == files.len()
                && r.files.iter().zip(&files).all(|(a, b)| a.same(b)),
            wire::Failure::new(code::CHANGED, "源文件已变化，无法续传")
        );
    } else {
        r.files = files;
        r.folder = manifest.folder;
        r.total = total;
        r.initialized = true;
    }
    if r.local_root.is_none() {
        let dest = PathBuf::from(&r.destination);
        let folder = r.folder.clone();
        let policy = r.policy;
        r.local_root = Some(
            tokio::task::spawn_blocking(move || -> Result<PathBuf> {
                let root = storage::canonical_dir(&dest)?;
                if folder.is_empty() {
                    return Ok(root);
                }
                let relative = storage::safe_relative(&folder)?;
                ensure!(relative.components().count() == 1, "远端文件夹名包含路径");
                let mut target = root.join(&relative);
                if target.exists() && policy == conflict::RENAME {
                    for i in 1..10000 {
                        target = root.join(format!("{folder} ({i})"));
                        if !target.exists() {
                            break;
                        }
                    }
                    ensure!(!target.exists(), "无法分配目标目录");
                }
                if !target.exists() {
                    std::fs::create_dir(&target)?;
                }
                storage::canonical_dir(&target)
            })
            .await??,
        );
    }
    let root = r.local_root.clone().context("下载目录无效")?;
    r.started = true;
    repo.save(r)?;
    // Offer what is already here: partial files with their tail digests.
    let holdings = {
        let (root, key, policy) = (root.clone(), r.key.clone(), r.policy);
        let (files, partial) = (r.files.clone(), r.partial.clone());
        tokio::task::spawn_blocking(move || {
            files
                .iter()
                .map(|item| {
                    let old = partial.iter().find(|p| p.info.rel_path == item.rel_path);
                    storage::plan(&root, &key, item, policy, old)
                })
                .collect::<Result<Vec<_>>>()
        })
        .await??
    };
    for holding in &holdings {
        upsert(r, holding.partial.clone());
    }
    repo.save(r)?;
    let start = wire::DownloadStart {
        transfer: transfer.id,
        items: holdings.iter().map(|h| h.plan.clone()).collect(),
    };
    let Body::Accepted(accepted) = wire.request(Body::DownloadStart(start), stop).await? else {
        anyhow::bail!("下载开始响应类型不匹配");
    };
    ensure!(
        accepted.offsets.len() == holdings.len()
            && holdings
                .iter()
                .zip(&accepted.offsets)
                .all(|(h, &offset)| h.plan.skip || offset == 0 || offset == h.plan.offset),
        "被控端续传位置无效"
    );
    let expected = (0..holdings.len())
        .filter(|&i| !holdings[i].plan.skip)
        .collect::<Vec<_>>();
    tracing::info!(
        transfer = transfer.id,
        items = holdings.len(),
        skipped = holdings.len() - expected.len(),
        offered = holdings.iter().map(|h| h.plan.offset).sum::<u64>(),
        resumed = expected.iter().map(|&i| accepted.offsets[i]).sum::<u64>(),
        "file download starting"
    );
    let mut received = r.completed_bytes()
        + expected
            .iter()
            .map(|&i| accepted.offsets[i])
            .sum::<u64>();
    repo.progress(&r.key, received, false);
    let mut saver = Saver::new();
    let mut open: Option<storage::Receiving> = None;
    let mut next_item = 0;
    while next_item < expected.len() {
        let chunk = match next(transfer).await? {
            Inbound::Chunk(chunk) => chunk,
            Inbound::ItemDone(done) => {
                ensure!(done.error_code != code::OK, "下载任务收到了意外的确认");
                return Err(item_failure(&done, &r.files));
            }
        };
        let index = expected[next_item];
        ensure!(
            chunk.entry as usize == index,
            "被控端发送的文件顺序不符"
        );
        let item = r.files[index].clone();
        if item.directory {
            ensure!(
                chunk.data.is_empty() && chunk.last,
                "被控端发送了目录内容"
            );
            let (root, directory) = (root.clone(), item.clone());
            tokio::task::spawn_blocking(move || storage::make_directory(&root, &directory))
                .await??;
            upsert(
                r,
                PartialFile {
                    target: item.rel_path.clone(),
                    info: item,
                    done: true,
                    skipped: false,
                },
            );
            saver.save(repo, r)?;
            next_item += 1;
            continue;
        }
        if open.is_none() {
            let partial = holdings[index].partial.clone();
            let (root, key, offset) = (root.clone(), r.key.clone(), accepted.offsets[index]);
            open = Some(
                tokio::task::spawn_blocking(move || storage::open(&root, &key, &partial, offset))
                    .await??,
            );
        }
        let receiving = open.as_mut().expect("opened above");
        ensure!(
            chunk.offset == receiving.position,
            "被控端文件数据不连续"
        );
        let len = chunk.data.len();
        let end = receiving
            .position
            .checked_add(len as u64)
            .filter(|end| *end <= item.size)
            .context("文件块超过声明长度")?;
        receiving
            .file
            .write_all(&chunk.data)
            .await
            .context("写入本地文件失败")?;
        receiving.position = end;
        wire.delivered(transfer, len).await?;
        received += len as u64;
        repo.progress(&r.key, received, false);
        if chunk.last {
            let receiving = open.take().expect("opened above");
            receiving_done(r, repo, &root, receiving, &mut saver).await?;
            next_item += 1;
        }
    }
    Ok(())
}

async fn receiving_done(
    r: &mut Record,
    repo: &Repository,
    root: &std::path::Path,
    mut receiving: storage::Receiving,
    saver: &mut Saver,
) -> Result<()> {
    receiving.file.flush().await?;
    let partial = storage::finish(receiving, root, &r.key, r.policy).await?;
    upsert(r, partial);
    saver.save(repo, r)
}
