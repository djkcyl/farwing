//! Host-side resume records and the management requests (List, MakeDir,
//! Rename, Remove, Exists). Runs in the user's executor only.
use super::super::{model::FileEntry, service::PartialFile};
use super::*;
use crate::i18n::t;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::os::windows::fs::MetadataExt;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

/// Receiving state of one upload task, keyed by the controller's task key.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Record {
    pub key: String,
    pub remote_key: String,
    pub destination: PathBuf,
    pub root: PathBuf,
    pub folder: String,
    pub policy: i32,
    pub files: Vec<FileInfo>,
    pub partial: Vec<PartialFile>,
}
pub(super) struct Store {
    directory: PathBuf,
    pub busy: Mutex<std::collections::HashSet<String>>,
    temporary: Mutex<HashMap<String, Vec<PathBuf>>>,
}
impl Store {
    pub fn new(scope: &str) -> Result<Self> {
        ensure!(!scope.is_empty() && scope.len() <= 1024, "{}", t().features.transfer_session_scope_invalid);
        let directory = crate::product::local_data_dir()
            .context(t().features.transfer_user_dir_unavailable)?
            .join("file-receive")
            .join(format!("{:x}", Sha256::digest(scope)));
        Ok(Self::at(directory))
    }
    /// The records kept in `directory`.
    pub fn at(directory: PathBuf) -> Self {
        let store = Self {
            directory,
            busy: Mutex::default(),
            temporary: Mutex::default(),
        };
        if let Ok(records) = std::fs::read_dir(&store.directory) {
            for entry in records.take(256).flatten() {
                if entry.path().extension().is_none_or(|s| s != "json") {
                    continue;
                }
                let restore = (|| -> Result<()> {
                    let mut bytes = Vec::new();
                    File::open(entry.path())?
                        .take(32 * 1024 * 1024 + 1)
                        .read_to_end(&mut bytes)?;
                    ensure!(bytes.len() <= 32 * 1024 * 1024, "{}", t().features.transfer_resume_record_too_large);
                    let record: Record = serde_json::from_slice(&bytes)?;
                    ensure!(
                        store.path(&record.remote_key)? == entry.path(),
                        "{}", t().features.transfer_resume_identity_mismatch
                    );
                    store.cache_temporary(&record)
                })();
                if let Err(error) = restore {
                    tracing::debug!(%error,"file listing resume metadata unavailable");
                }
            }
        }
        store
    }
    fn path(&self, key: &str) -> Result<PathBuf> {
        ensure!(
            !key.is_empty() && key.len() <= 1024 && !key.contains('\0'),
            "{}", t().features.transfer_resume_key_invalid
        );
        Ok(self
            .directory
            .join(format!("{:x}.json", Sha256::digest(key))))
    }
    pub fn reserve_record(self: &Arc<Self>, key: &str) -> Result<Reservation> {
        Reservation::acquire(self.clone(), &self.path(key)?)
    }
    pub fn load(&self, key: &str) -> Result<Option<Record>> {
        let f = match File::open(self.path(key)?) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut bytes = Vec::new();
        f.take(32 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 32 * 1024 * 1024, "{}", t().features.transfer_resume_record_too_large);
        let record: Record = serde_json::from_slice(&bytes)?;
        ensure!(
            record.remote_key == key && uuid::Uuid::parse_str(&record.key).is_ok(),
            "{}", t().features.transfer_resume_identity_mismatch
        );
        storage::validate_manifest(&record.files)?;
        let root = local_path(&record.root.to_string_lossy())?;
        ensure!(
            root == record.root && record.partial.len() <= storage::MAX_FILES,
            "{}", t().features.transfer_resume_record_path_invalid
        );
        for p in &record.partial {
            storage::safe_relative(&p.target)?;
            storage::safe_relative(&p.info.rel_path)?;
        }
        Ok(Some(record))
    }
    pub fn save(&self, r: &Record) -> Result<()> {
        let bytes = serde_json::to_vec(r)?;
        ensure!(bytes.len() <= 32 * 1024 * 1024, "{}", t().features.transfer_resume_record_too_large);
        std::fs::create_dir_all(&self.directory)?;
        let final_path = self.path(&r.remote_key)?;
        if !final_path.exists() {
            ensure!(
                std::fs::read_dir(&self.directory)?.take(257).count() < 256,
                "{}", t().features.transfer_too_many_pending_receives
            );
        }
        let temp = final_path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&temp, &final_path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temp);
        }
        result?;
        self.cache_temporary(r)
    }
    fn cache_temporary(&self, r: &Record) -> Result<()> {
        uuid::Uuid::parse_str(&r.key)?;
        ensure!(r.root.is_absolute(), "{}", t().features.transfer_resume_root_invalid);
        let mut entries = Vec::new();
        for p in r
            .partial
            .iter()
            .filter(|p| !p.done && !p.skipped && !p.info.directory)
        {
            ensure!(entries.len() < storage::MAX_FILES, "{}", t().features.transfer_too_many_temporary_files);
            let relative = storage::safe_relative(&p.target)?;
            entries.push(storage::temp_path(&r.root, &relative, &r.key));
        }
        super::super::lock(&self.temporary).insert(r.remote_key.clone(), entries);
        Ok(())
    }
    fn hide_temporary(entries: &mut Vec<FileEntry>, temporary: &[PathBuf]) {
        entries.retain(|entry| {
            !temporary.iter().any(|temp| {
                temp.to_string_lossy()
                    .eq_ignore_ascii_case(&entry.full_path)
            })
        });
    }
    /// Delete the record of `key` and its partial files.
    pub fn clear(&self, key: &str) -> Result<()> {
        if let Some(r) = self.load(key)? {
            storage::cleanup(&r.root, &r.key, &r.partial)?;
            std::fs::remove_file(self.path(key)?)?;
        }
        super::super::lock(&self.temporary).remove(key);
        Ok(())
    }
    /// Forget the record of a completed task (its files are all saved).
    pub fn finish(&self, key: &str) -> Result<()> {
        match std::fs::remove_file(self.path(key)?) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        super::super::lock(&self.temporary).remove(key);
        Ok(())
    }
}
pub(super) fn local_path(value: &str) -> Result<PathBuf> {
    storage::local_path(value)
}

pub(super) fn directory(value: &str) -> Result<PathBuf> {
    storage::canonical_dir(&local_path(value)?)
}
fn parent_locks(path: &Path) -> Result<Vec<File>> {
    let parent = path.parent().context(t().features.transfer_cannot_modify_drive_root)?;
    let root = PathBuf::from(format!(
        "{}:\\",
        path.to_string_lossy().as_bytes()[0] as char
    ));
    let rel = path.strip_prefix(&root)?;
    ensure!(
        parent != path && rel.components().count() > 0,
        "{}", t().features.transfer_cannot_modify_drive_root
    );
    storage::parents(&root, rel, false)
}
/// The directory an upload writes into: `destination`, or its top-level
/// `folder` (a free "folder (n)" under the rename policy).
pub(super) fn receiving_root(destination: &Path, folder: &str, policy: i32) -> Result<PathBuf> {
    if folder.is_empty() {
        return storage::canonical_dir(destination);
    }
    let rel = storage::safe_relative(folder)?;
    ensure!(rel.components().count() == 1, "{}", t().features.transfer_receive_folder_name_invalid);
    let _locks = storage::parents(destination, &rel, false)?;
    let mut path = destination.join(&rel);
    if policy == wire::conflict::RENAME {
        for i in 0..10000 {
            if i > 0 {
                path = destination.join(format!("{folder} ({i})"));
            }
            match std::fs::create_dir(&path) {
                Ok(()) => return storage::canonical_dir(&path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        }
        anyhow::bail!("{}", t().features.transfer_receive_folder_unavailable)
    }
    if !path.exists() {
        std::fs::create_dir(&path)?;
    }
    storage::canonical_dir(&path)
}
/// Marks a path as in use by one file operation; nested paths conflict.
pub(super) struct Reservation {
    store: Arc<Store>,
    key: String,
}
impl Reservation {
    pub fn acquire(store: Arc<Store>, root: &Path) -> Result<Self> {
        let key = root
            .to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase();
        let mut active = super::super::lock(&store.busy);
        ensure!(
            !active.iter().any(|p| p == &key
                || key.starts_with(&format!("{p}\\"))
                || p.starts_with(&format!("{key}\\"))),
            Failure::new(code::BUSY, t().features.transfer_location_in_use)
        );
        active.insert(key.clone());
        drop(active);
        Ok(Self { store, key })
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        super::super::lock(&self.store.busy).remove(&self.key);
    }
}

fn single_name(name: &str) -> Result<PathBuf> {
    let relative = storage::safe_relative(name)?;
    ensure!(
        relative.components().count() == 1,
        Failure::new(code::PROTOCOL, t().features.transfer_name_has_path)
    );
    Ok(relative)
}

/// Run one management request and build its response.
pub(super) fn operation(body: Body, store: &Arc<Store>, stop: &CancellationToken) -> Result<Body> {
    ensure!(!stop.is_cancelled(), "{}", t().features.transfer_operation_revoked);
    Ok(match body {
        Body::List(v) => {
            let roots = v.path.is_empty();
            let path = if roots {
                ":/".to_owned()
            } else {
                directory(&v.path)?.to_string_lossy().into_owned()
            };
            let temporary: Vec<_> = super::super::lock(&store.temporary)
                .values()
                .flatten()
                .cloned()
                .collect();
            let mut entries = super::super::service::local_list(&path)?;
            Store::hide_temporary(&mut entries, &temporary);
            Body::Listing(wire::Listing {
                path: if roots { String::new() } else { path },
                entries: entries.iter().map(|e| wire::entry_of(e, roots)).collect(),
            })
        }
        Body::Exists(v) => {
            ensure!(v.names.len() <= storage::MAX_FILES, "{}", t().features.transfer_too_many_name_checks);
            let root = directory(&v.dir)?;
            let mut existing = wire::Existing::default();
            for name in v.names {
                let p = root.join(single_name(&name)?);
                let key = p.to_string_lossy().to_lowercase();
                let busy = super::super::lock(&store.busy)
                    .iter()
                    .any(|s| s == &key || key.starts_with(&format!("{s}\\")));
                if p.try_exists()? {
                    existing.present.push(name.clone());
                }
                if busy {
                    existing.writing.push(name);
                }
            }
            Body::Existing(existing)
        }
        Body::MakeDir(v) => {
            let parent = directory(&v.parent)?;
            let path = parent.join(single_name(&v.name)?);
            let _lock = Reservation::acquire(store.clone(), &path)?;
            let _parents = parent_locks(&path)?;
            match std::fs::create_dir(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(Failure::new(code::EXISTS, t().features.transfer_name_exists).into());
                }
                Err(e) => return Err(e.into()),
            }
            Body::Done(wire::Done {
                path: path.to_string_lossy().into_owned(),
            })
        }
        Body::Rename(v) => {
            let from = local_path(&v.path)?;
            let parent = from.parent().context(t().features.transfer_cannot_rename_root)?;
            let to = parent.join(single_name(&v.name)?);
            let _from = Reservation::acquire(store.clone(), &from)?;
            let same = from
                .to_string_lossy()
                .eq_ignore_ascii_case(&to.to_string_lossy());
            let _to = (!same)
                .then(|| Reservation::acquire(store.clone(), &to))
                .transpose()?;
            let _parents = parent_locks(&from)?;
            std::fs::symlink_metadata(&from)?;
            // A case-only rename names the same entry.
            ensure!(
                same || !to.try_exists()?,
                Failure::new(code::EXISTS, t().features.transfer_name_exists)
            );
            std::fs::rename(&from, &to)?;
            Body::Done(wire::Done {
                path: to.to_string_lossy().into_owned(),
            })
        }
        Body::Remove(v) => {
            let p = local_path(&v.path)?;
            let _lock = Reservation::acquire(store.clone(), &p)?;
            let _parents = parent_locks(&p)?;
            let m = std::fs::symlink_metadata(&p)?;
            ensure!(
                m.file_attributes() & 0x400 == 0,
                Failure::new(code::DENIED, t().features.transfer_cannot_delete_link)
            );
            if m.is_dir() {
                if v.recursive {
                    remove_tree(&p, stop, 0)?;
                } else {
                    std::fs::remove_dir(&p)?;
                }
            } else {
                std::fs::remove_file(&p)?;
            }
            Body::Done(wire::Done { path: v.path })
        }
        other => anyhow::bail!((t().features.transfer_not_management_operation)(other.name())),
    })
}
fn remove_tree(path: &Path, stop: &CancellationToken, depth: usize) -> Result<()> {
    ensure!(
        !stop.is_cancelled() && depth < 128,
        "{}", t().features.transfer_delete_cancelled_or_too_deep
    );
    let m = std::fs::symlink_metadata(path)?;
    ensure!(
        m.is_dir() && m.file_attributes() & 0x400 == 0,
        "{}", t().features.transfer_delete_target_not_directory
    );
    let held = storage::parents(path, Path::new(".farwing-directory-lock"), false)?;
    for child in std::fs::read_dir(path)? {
        let p = child?.path();
        let m = std::fs::symlink_metadata(&p)?;
        ensure!(m.file_attributes() & 0x400 == 0, "{}", t().features.transfer_cannot_delete_reparse_recursively);
        if m.is_dir() {
            remove_tree(&p, stop, depth + 1)?
        } else {
            ensure!(!stop.is_cancelled(), "{}", t().features.transfer_delete_cancelled);
            std::fs::remove_file(p)?;
        }
    }
    drop(held);
    std::fs::remove_dir(path)?;
    Ok(())
}
