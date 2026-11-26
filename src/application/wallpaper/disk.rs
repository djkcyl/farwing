//! Disposable image cache. URLs and account credentials are never persisted.
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime},
};
use tokio_util::sync::CancellationToken;

pub(super) const FRESH_FOR: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const MAX_BYTES: u64 = 128 * 1024 * 1024;
const MAX_FILES: usize = 256;
const MAX_IMAGE_BYTES: u64 = 4 * 1024 * 1024;
static IO: Mutex<()> = Mutex::new(());

pub(super) fn path(device: &str, url: &str) -> Option<PathBuf> {
    let root = crate::platform::paths::local_app_data()?;
    let mut hash = Sha256::new();
    hash.update((device.len() as u64).to_le_bytes());
    hash.update(device.as_bytes());
    hash.update(url.as_bytes());
    Some(
        root.join("OpenUUYC/cache/wallpapers/v1")
            .join(format!("{:x}.png", hash.finalize())),
    )
}

pub(super) fn read(path: &Path) -> Result<(Vec<u8>, bool)> {
    let _guard = IO.lock().unwrap_or_else(|e| e.into_inner());
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_IMAGE_BYTES {
        bail!("invalid cached image");
    }
    let age = metadata.modified()?.elapsed().unwrap_or_default();
    if age > MAX_AGE {
        bail!("expired cached image");
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        bail!("cached image too large");
    }
    Ok((bytes, age < FRESH_FOR))
}

pub(super) fn write(path: &Path, bytes: &[u8], cancel: &CancellationToken) -> Result<()> {
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        bail!("cached image too large");
    }
    let _guard = IO.lock().unwrap_or_else(|e| e.into_inner());
    if cancel.is_cancelled() {
        bail!("cancelled");
    }
    let root = path.parent().context("cache directory")?;
    fs::create_dir_all(root)?;
    let temporary = root.join(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        drop(file);
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    prune(root, MAX_FILES, MAX_BYTES);
    result
}

fn prune(root: &Path, max_files: usize, max_bytes: u64) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let mut images = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let image = name
            .strip_suffix(".png")
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));
        let temporary = name
            .strip_suffix(".tmp")
            .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok());
        if (!image && !temporary) || !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let age = modified.elapsed().unwrap_or_default();
        if age > MAX_AGE || (temporary && age > Duration::from_secs(3600)) {
            let _ = fs::remove_file(entry.path());
        } else if image {
            images.push((modified, metadata.len(), entry.path()));
        }
    }
    images.sort_by_key(|(modified, _, _)| *modified);
    let mut bytes: u64 = images.iter().map(|(_, size, _)| size).sum();
    let mut count = images.len();
    for (_, size, path) in images {
        if count <= max_files && bytes <= max_bytes {
            break;
        }
        if fs::remove_file(path).is_ok() {
            bytes = bytes.saturating_sub(size);
            count -= 1;
        }
    }
}
