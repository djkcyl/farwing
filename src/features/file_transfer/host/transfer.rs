//! Host transfer tasks: receive an upload into hidden partial files, or send a
//! download within the controller's credit.
use super::super::{TIMEOUT, service::PartialFile};
use super::filesystem::{self, Record, Reservation, Store};
use super::*;
use crate::i18n::t;
use crate::transport::transfer::{CHUNK, kind};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// The task side of an admitted transfer.
pub(super) struct Run {
    pub id: u32,
    pub events: mpsc::UnboundedReceiver<Event>,
    pub shared: Arc<Shared>,
    /// The entry in progress, for failure and revocation notices.
    pub entry: Arc<AtomicU32>,
}

/// The transfer was stopped by a `Cancel`, a fault or the engine closing.
#[derive(Debug)]
struct Stopped;
impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(t().features.transfer_stopped)
    }
}
impl std::error::Error for Stopped {}

impl Run {
    async fn next(&mut self) -> Result<Event> {
        tokio::select! {
            biased;
            _ = self.shared.stop.cancelled() => Err(Stopped.into()),
            event = tokio::time::timeout(TIMEOUT, self.events.recv()) => event
                .context(t().features.transfer_controller_idle)?
                .context(t().features.transfer_connection_ended),
        }
    }
    /// Wait until the download may send at least one byte.
    async fn credit(&self) -> Result<u64> {
        loop {
            let changed = self.shared.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.shared.stop.is_cancelled() {
                return Err(Stopped.into());
            }
            let available = super::super::lock(&self.shared.credit).available();
            if available > 0 {
                return Ok(available);
            }
            tokio::select! {
                _ = self.shared.stop.cancelled() => {},
                _ = changed => {},
                _ = tokio::time::sleep(TIMEOUT) => anyhow::bail!("{}", t().features.transfer_controller_not_receiving),
            }
        }
    }
    fn at(&self, entry: usize) {
        self.entry.store(entry as u32, Ordering::Release);
    }
}

/// How a transfer task ended.
enum Ending {
    Complete,
    /// Stopped by the controller's `Cancel` (request ID, discard).
    Cancelled(u32, bool),
    /// The engine closed (connection gone or permission revoked).
    Closed,
    Failed(Failure),
}

/// Release the task's event queue, then decide how it ended. Dropping the
/// queue first lets the engine answer a `Cancel` that arrives afterwards.
fn ending(run: Run, result: Result<()>) -> (Ending, u32) {
    let Run {
        events,
        shared,
        entry,
        ..
    } = run;
    drop(events);
    let entry = entry.load(Ordering::Acquire);
    let cancel = super::super::lock(&shared.cancel).take();
    let ending = match (result, cancel) {
        (_, Some((id, discard))) => Ending::Cancelled(id, discard),
        (Ok(()), None) => Ending::Complete,
        (Err(error), None) => match super::super::lock(&shared.fault).clone() {
            Some(fault) => Ending::Failed(Failure::new(code::PROTOCOL, fault)),
            None if shared.stop.is_cancelled() || error.is::<Stopped>() => Ending::Closed,
            None => Ending::Failed(failure(&error)),
        },
    };
    (ending, entry)
}

/// Publish the notice state, report a failure and answer a `Cancel`.
async fn conclude(
    id: u32,
    ending: Ending,
    entry: u32,
    output: &Output,
    progress: &mut notices::Progress,
    succeeded: bool,
) {
    match &ending {
        Ending::Complete => progress.terminal(notices::COMPLETE, String::new()),
        Ending::Cancelled(_, true) => progress.terminal(notices::CANCELLED, String::new()),
        // A pause after the last byte still completed the task.
        Ending::Cancelled(..) if succeeded => progress.terminal(notices::COMPLETE, String::new()),
        Ending::Cancelled(..) => progress.terminal(notices::PAUSED, String::new()),
        Ending::Closed => progress.terminal(notices::INTERRUPTED, String::new()),
        Ending::Failed(failure) => progress.terminal(notices::FAILED, failure.to_string()),
    }
    let sent = match ending {
        Ending::Failed(failure) => {
            tracing::warn!(transfer = id, entry, error = %failure, "host file transfer failed");
            output
                .send(Packet::session(wire::item_done(wire::ItemDone {
                    transfer: id,
                    entry,
                    error_code: failure.code,
                    message: failure.message,
                })))
                .await
        }
        Ending::Cancelled(request, _) => output.respond(request, Body::Done(wire::Done::default())).await,
        Ending::Complete | Ending::Closed => Ok(()),
    };
    if let Err(error) = sent {
        tracing::debug!(%error, "host file transfer ending not sent");
    }
}

fn upsert(r: &mut Record, p: PartialFile) {
    if let Some(old) = r
        .partial
        .iter_mut()
        .find(|v| v.info.rel_path == p.info.rel_path)
    {
        *old = p;
    } else {
        r.partial.push(p);
    }
}
async fn persist(store: &Arc<Store>, record: &Record) -> Result<()> {
    let store = store.clone();
    let record = record.clone();
    tokio::task::spawn_blocking(move || store.save(&record)).await?
}

/// An upload ready to receive: its record, per-item decisions and the
/// locations it holds.
struct Prepared {
    record: Record,
    items: Vec<FileInfo>,
    holdings: Vec<storage::Holding>,
    _reservations: Vec<Reservation>,
    _record: Reservation,
}

fn prepare_upload(store: &Arc<Store>, upload: &wire::Upload) -> Result<Prepared> {
    ensure!(
        matches!(upload.conflict, 1..=3),
        Failure::new(code::PROTOCOL, t().features.transfer_unknown_conflict_policy)
    );
    let items = upload.items.iter().map(FileInfo::from).collect::<Vec<_>>();
    storage::validate_manifest(&items)?;
    let record_lock = store.reserve_record(&upload.key)?;
    let destination = filesystem::directory(&upload.dir)?;
    let mut record = match store.load(&upload.key)? {
        // A resume continues in the same receiving folder.
        Some(old) => {
            ensure!(
                old.destination == destination && old.folder == upload.folder,
                Failure::new(code::CHANGED, t().features.transfer_resume_target_changed)
            );
            Record {
                policy: upload.conflict,
                ..old
            }
        }
        None => Record {
            key: uuid::Uuid::new_v4().to_string(),
            remote_key: upload.key.clone(),
            root: filesystem::receiving_root(&destination, &upload.folder, upload.conflict)?,
            destination,
            folder: upload.folder.clone(),
            policy: upload.conflict,
            files: Vec::new(),
            partial: Vec::new(),
        },
    };
    for item in &items {
        match record.files.iter().find(|f| f.rel_path == item.rel_path) {
            Some(known) => ensure!(
                known.same(item),
                Failure::new(code::CHANGED, t().features.transfer_source_changed)
            ),
            None => record.files.push(item.clone()),
        }
    }
    let holdings = items
        .iter()
        .map(|item| {
            let old = record.partial.iter().find(|p| p.info.rel_path == item.rel_path);
            storage::plan(&record.root, &record.key, item, record.policy, old)
        })
        .collect::<Result<Vec<_>>>()?;
    for holding in &holdings {
        upsert(&mut record, holding.partial.clone());
    }
    let targets = if !record.folder.is_empty() || items.is_empty() {
        vec![record.root.clone()]
    } else {
        items.iter().map(|f| record.root.join(&f.rel_path)).collect()
    };
    let reservations = targets
        .iter()
        .map(|p| Reservation::acquire(store.clone(), p))
        .collect::<Result<Vec<_>>>()?;
    store.save(&record)?;
    Ok(Prepared {
        record,
        items,
        holdings,
        _reservations: reservations,
        _record: record_lock,
    })
}

pub(super) async fn upload(
    request: u32,
    upload: wire::Upload,
    mut run: Run,
    output: Output,
    store: Arc<Store>,
    journal: notices::Journal,
) {
    let mut progress = journal.begin(true);
    tracing::debug!(
        transfer = run.id,
        items = upload.items.len(),
        conflict = upload.conflict,
        "host file upload requested"
    );
    let prepared = {
        let store = store.clone();
        tokio::task::spawn_blocking(move || prepare_upload(&store, &upload))
            .await
            .unwrap_or_else(|e| Err(e.into()))
    };
    let mut prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::warn!(transfer = run.id, error = %format!("{error:#}"), "host file upload refused");
            progress.terminal(notices::FAILED, error.to_string());
            let _ = output.fail(request, &error).await;
            return;
        }
    };
    progress.describe(&prepared.items, &prepared.record.root);
    let plan = wire::Plan {
        transfer: run.id,
        items: prepared.holdings.iter().map(|h| h.plan.clone()).collect(),
    };
    let mut open = None;
    let result = match output.respond(request, Body::Plan(plan)).await {
        Ok(()) => receive_items(&mut run, &output, &store, &mut prepared, &mut open, &mut progress).await,
        Err(error) => Err(error),
    };
    // Keep every received byte for a resume.
    if let Some(mut receiving) = open.take() {
        let kept = async {
            receiving.file.flush().await?;
            receiving.file.sync_all().await
        }
        .await;
        if let Err(error) = kept {
            tracing::warn!(%error, "host partial file not flushed");
        }
    }
    let succeeded = result.is_ok();
    let saved = if succeeded {
        let (store, key) = (store.clone(), prepared.record.remote_key.clone());
        tokio::task::spawn_blocking(move || store.finish(&key))
            .await
            .unwrap_or_else(|e| Err(e.into()))
    } else {
        persist(&store, &prepared.record).await
    };
    if let Err(error) = saved {
        tracing::warn!(%error, "host file receive record not saved");
    }
    let id = run.id;
    let (ending, entry) = ending(run, result);
    if let Ending::Cancelled(_, true) = ending {
        let (store, key) = (store.clone(), prepared.record.remote_key.clone());
        let cleared = tokio::task::spawn_blocking(move || store.clear(&key)).await;
        if !matches!(cleared, Ok(Ok(()))) {
            tracing::warn!(?cleared, "cancelled upload cleanup failed");
        }
    }
    // Release the locations before answering, so a resume can take them.
    drop(prepared);
    conclude(id, ending, entry, &output, &mut progress, succeeded).await;
}

async fn receive_items(
    run: &mut Run,
    output: &Output,
    store: &Arc<Store>,
    prepared: &mut Prepared,
    open: &mut Option<storage::Receiving>,
    progress: &mut notices::Progress,
) -> Result<()> {
    let (start, offsets) = match run.next().await? {
        Event::Start(id, Body::UploadStart(start)) => (id, start.offsets),
        Event::Start(id, _) => {
            let error = Failure::new(code::PROTOCOL, t().features.transfer_upload_got_download_start).into();
            output.fail(id, &error).await?;
            return Err(error);
        }
        Event::Chunk(_) => {
            return Err(Failure::new(code::PROTOCOL, t().features.transfer_upload_data_before_start).into());
        }
    };
    let holdings = &prepared.holdings;
    if offsets.len() != holdings.len()
        || holdings
            .iter()
            .zip(&offsets)
            .any(|(h, &offset)| !h.plan.skip && offset != 0 && offset != h.plan.offset)
    {
        let error = Failure::new(code::PROTOCOL, t().features.transfer_upload_offsets_mismatch).into();
        output.fail(start, &error).await?;
        return Err(error);
    }
    output.respond(start, Body::Done(wire::Done::default())).await?;
    tracing::info!(
        transfer = run.id,
        items = holdings.len(),
        skipped = holdings.iter().filter(|h| h.plan.skip).count(),
        resumed = offsets.iter().sum::<u64>(),
        root = %prepared.record.root.display(),
        "host file upload receiving"
    );
    let items = &prepared.items;
    let expected = (0..items.len())
        .filter(|&i| !holdings[i].plan.skip)
        .collect::<Vec<_>>();
    progress.advance(
        (0..items.len())
            .map(|i| {
                if holdings[i].plan.skip {
                    items[i].size
                } else {
                    offsets[i]
                }
            })
            .sum(),
    );
    let mut saved = Instant::now();
    let mut next = 0;
    while next < expected.len() {
        let index = expected[next];
        run.at(index);
        let chunk = match run.next().await? {
            Event::Chunk(chunk) => chunk,
            Event::Start(id, _) => {
                let _ = output
                    .fail(id, &Failure::new(code::PROTOCOL, t().features.transfer_already_started).into())
                    .await;
                continue;
            }
        };
        ensure!(
            chunk.entry as usize == index,
            Failure::new(code::PROTOCOL, t().features.transfer_entry_order_mismatch)
        );
        let item = &items[index];
        let record = &mut prepared.record;
        if item.directory {
            ensure!(
                chunk.data.is_empty() && chunk.last,
                Failure::new(code::PROTOCOL, t().features.transfer_directory_entry_has_data)
            );
            let (root, directory) = (record.root.clone(), item.clone());
            tokio::task::spawn_blocking(move || storage::make_directory(&root, &directory))
                .await??;
            upsert(
                record,
                PartialFile {
                    info: item.clone(),
                    target: item.rel_path.clone(),
                    done: true,
                    skipped: false,
                },
            );
        } else {
            if open.is_none() {
                let (root, key) = (record.root.clone(), record.key.clone());
                let (partial, offset) = (holdings[index].partial.clone(), offsets[index]);
                *open = Some(
                    tokio::task::spawn_blocking(move || storage::open(&root, &key, &partial, offset))
                        .await??,
                );
            }
            let receiving = open.as_mut().expect("opened above");
            ensure!(
                chunk.offset == receiving.position,
                Failure::new(code::PROTOCOL, t().features.transfer_data_not_contiguous)
            );
            let len = chunk.data.len();
            let end = receiving
                .position
                .checked_add(len as u64)
                .filter(|end| *end <= item.size)
                .ok_or_else(|| Failure::new(code::PROTOCOL, t().features.transfer_data_exceeds_size))?;
            receiving.file.write_all(&chunk.data).await?;
            receiving.position = end;
            progress.advance(len as u64);
            // The data is written: the controller may send more.
            let credit = super::super::lock(&run.shared.window).deliver(len);
            if let Some(credit) = credit {
                output.send(Packet::bulk(wire::flow(run.id, credit))).await?;
            }
            if !chunk.last {
                continue;
            }
            let mut receiving = open.take().expect("opened above");
            receiving.file.flush().await?;
            let partial = storage::finish(receiving, &record.root, &record.key, record.policy).await?;
            upsert(record, partial);
        }
        if saved.elapsed() >= Duration::from_secs(1) {
            persist(store, record).await?;
            saved = Instant::now();
        }
        progress.saved();
        output
            .send(Packet::session(wire::item_done(wire::ItemDone {
                transfer: run.id,
                entry: index as u32,
                error_code: code::OK,
                message: String::new(),
            })))
            .await?;
        next += 1;
    }
    Ok(())
}

/// The manifest of `paths`: one file or folder, or several entries of one
/// directory (relative to it, without a top-level folder).
fn scan_sources(
    paths: &[String],
    stop: &CancellationToken,
) -> Result<(PathBuf, String, Vec<FileInfo>)> {
    ensure!(
        !paths.is_empty() && paths.len() <= storage::MAX_FILES,
        Failure::new(code::PROTOCOL, t().features.transfer_download_path_invalid)
    );
    if let [path] = paths {
        return storage::scan(&filesystem::local_path(path)?, stop);
    }
    let mut root: Option<PathBuf> = None;
    let mut files = Vec::new();
    for path in paths {
        let (base, folder, items) = storage::scan(&filesystem::local_path(path)?, stop)?;
        let parent = if folder.is_empty() {
            base
        } else {
            base.parent().context(t().features.transfer_cannot_download_drive)?.to_owned()
        };
        ensure!(
            *root.get_or_insert_with(|| parent.clone()) == parent,
            Failure::new(code::PROTOCOL, t().features.transfer_download_items_same_folder)
        );
        if folder.is_empty() {
            files.extend(items);
        } else if items.is_empty() {
            files.push(FileInfo::directory(folder));
        } else {
            files.extend(items.into_iter().map(|mut item| {
                item.rel_path = format!("{folder}/{}", item.rel_path);
                item
            }));
        }
    }
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    storage::validate_manifest(&files)?;
    Ok((root.context(t().features.transfer_download_path_invalid)?, String::new(), files))
}

pub(super) async fn download(
    request: u32,
    download: wire::Download,
    mut run: Run,
    output: Output,
    journal: notices::Journal,
) {
    let mut progress = journal.begin(false);
    let stop = run.shared.stop.clone();
    let scanned = tokio::task::spawn_blocking(move || scan_sources(&download.paths, &stop))
        .await
        .unwrap_or_else(|e| Err(e.into()));
    let (root, folder, items) = match scanned {
        Ok(scanned) => scanned,
        Err(error) => {
            tracing::warn!(transfer = run.id, error = %format!("{error:#}"), "host file download refused");
            progress.terminal(notices::FAILED, error.to_string());
            let _ = output.fail(request, &error).await;
            return;
        }
    };
    progress.describe(&items, &root);
    let manifest = wire::Manifest {
        transfer: run.id,
        folder,
        items: items.iter().map(wire::Item::from).collect(),
    };
    let result = match output.respond(request, Body::Manifest(manifest)).await {
        Ok(()) => send_items(&mut run, &output, &root, &items, &mut progress).await,
        Err(error) => Err(error),
    };
    let succeeded = result.is_ok();
    let id = run.id;
    let (ending, entry) = ending(run, result);
    conclude(id, ending, entry, &output, &mut progress, succeeded).await;
}

async fn send_items(
    run: &mut Run,
    output: &Output,
    root: &std::path::Path,
    items: &[FileInfo],
    progress: &mut notices::Progress,
) -> Result<()> {
    let (start, plans) = match run.next().await? {
        Event::Start(id, Body::DownloadStart(start)) => (id, start.items),
        Event::Start(id, _) => {
            let error = Failure::new(code::PROTOCOL, t().features.transfer_download_got_upload_start).into();
            output.fail(id, &error).await?;
            return Err(error);
        }
        Event::Chunk(_) => {
            return Err(Failure::new(code::PROTOCOL, t().features.transfer_download_got_data).into());
        }
    };
    // Resume where the controller's tail matches our bytes, else from 0.
    let offsets = if plans.len() == items.len() {
        let (root, items, plans) = (root.to_owned(), items.to_vec(), plans.clone());
        tokio::task::spawn_blocking(move || {
            items
                .iter()
                .zip(&plans)
                .map(|(item, plan)| storage::confirm(&root, item, plan))
                .collect::<Result<Vec<_>>>()
        })
        .await
        .unwrap_or_else(|e| Err(e.into()))
    } else {
        Err(Failure::new(code::PROTOCOL, t().features.transfer_download_plan_mismatch).into())
    };
    let offsets = match offsets {
        Ok(offsets) => offsets,
        Err(error) => {
            output.fail(start, &error).await?;
            return Err(error);
        }
    };
    output
        .respond(
            start,
            Body::Accepted(wire::Accepted {
                offsets: offsets.clone(),
            }),
        )
        .await?;
    tracing::info!(
        transfer = run.id,
        items = items.len(),
        skipped = plans.iter().filter(|p| p.skip).count(),
        offered = plans.iter().map(|p| p.offset).sum::<u64>(),
        resumed = offsets.iter().sum::<u64>(),
        "host file download sending"
    );
    progress.advance(
        (0..items.len())
            .map(|i| if plans[i].skip { items[i].size } else { offsets[i] })
            .sum(),
    );
    for (index, item) in items.iter().enumerate() {
        if plans[index].skip {
            continue;
        }
        run.at(index);
        let entry = index as u32;
        let mut file = if item.directory {
            None
        } else {
            let (root, source) = (root.to_owned(), item.clone());
            let file = tokio::task::spawn_blocking(move || storage::open_source(&root, &source))
                .await??;
            let mut file = tokio::fs::File::from_std(file);
            file.seek(std::io::SeekFrom::Start(offsets[index])).await?;
            Some(file)
        };
        let mut position = offsets[index];
        loop {
            let remaining = item.size - position;
            let len = if remaining == 0 {
                0
            } else {
                run.credit().await?.min(remaining).min(CHUNK as u64) as usize
            };
            let mut data = vec![0; len];
            if let Some(file) = &mut file {
                tokio::select! {
                    biased;
                    _ = run.shared.stop.cancelled() => return Err(Stopped.into()),
                    read = file.read_exact(&mut data) => { read.context(t().features.transfer_source_read_failed)?; }
                }
            }
            let last = position + len as u64 == item.size;
            super::super::lock(&run.shared.credit).spend(len)?;
            let packet = Packet::bulk(wire::chunk(&Chunk {
                stream: run.id,
                kind: kind::FILE,
                offset: position,
                data: bytes::Bytes::from(data),
                last,
                entry,
            }));
            tokio::select! {
                biased;
                _ = run.shared.stop.cancelled() => return Err(Stopped.into()),
                sent = output.send(packet) => sent?,
            }
            position += len as u64;
            progress.advance(len as u64);
            if last {
                break;
            }
        }
        progress.saved();
    }
    Ok(())
}
