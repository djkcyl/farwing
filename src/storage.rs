//! Settings and caches on disk: JSON values in the profile directory, each
//! written atomically (a new temporary file, flushed, then renamed over the
//! old one). Secrets stay in the credential store.
use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};
use std::io::Write;
use std::path::{Path, PathBuf};

/// `relative` inside this profile's data directory.
pub(crate) fn path(relative: impl AsRef<Path>) -> Result<PathBuf> {
    Ok(crate::product::local_data_dir()?.join(relative))
}

/// The bytes of `path`; `None` when there is no such file.
pub(crate) fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// The value saved at `path`; `None` when there is no such file.
pub(crate) fn read<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    read_bytes(path)?
        .map(|bytes| {
            serde_json::from_slice(&bytes).with_context(|| {
                (crate::i18n::t().system.settings_file_invalid)(&path.display().to_string())
            })
        })
        .transpose()
}

/// Replace `path` with `bytes`; a reader sees the old or the new file, never
/// a part of it.
pub(crate) fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context(crate::i18n::t().system.settings_folder_invalid)?;
    std::fs::create_dir_all(parent)?;
    let name = path.file_name().map_or_else(Default::default, |n| n.to_string_lossy());
    let temp = parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Save `value` at `path` (pretty JSON).
pub(crate) fn write<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    write_bytes(path, &serde_json::to_vec_pretty(value)?)
}

/// Delete `path`; a missing file is fine.
pub(crate) fn remove(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn values_round_trip_and_replace_atomically() {
        let dir = std::env::temp_dir().join(format!("farwing-storage-{}", uuid::Uuid::new_v4().simple()));
        let path = dir.join("nested").join("value.json");
        assert_eq!(super::read::<Vec<u32>>(&path).unwrap(), None);
        super::write(&path, &vec![1u32, 2]).unwrap();
        super::write(&path, &vec![3u32]).unwrap();
        assert_eq!(super::read::<Vec<u32>>(&path).unwrap(), Some(vec![3]));
        // Only the file itself is left: no temporaries.
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        std::fs::write(&path, b"{").unwrap();
        assert!(super::read::<Vec<u32>>(&path).is_err());
        super::remove(&path).unwrap();
        super::remove(&path).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
