//! Read descriptor data from a native library without executing its entry point.
use super::*;
use crate::i18n::t;
use object::{
    Object, ObjectSection,
    read::{ReadCache, ReadRef},
};
use std::{cell::Cell, fs::File, ops::Range};

#[derive(Clone, Copy)]
struct Reader<'a> {
    cache: &'a ReadCache<File>,
    budget: &'a Cell<u64>,
}
impl<'a> ReadRef<'a> for Reader<'a> {
    fn len(self) -> std::result::Result<u64, ()> {
        self.cache.len()
    }
    fn read_bytes_at(self, offset: u64, size: u64) -> std::result::Result<&'a [u8], ()> {
        self.budget
            .set(self.budget.get().checked_sub(size).ok_or(())?);
        self.cache.read_bytes_at(offset, size)
    }
    fn read_bytes_at_until(
        self,
        range: Range<u64>,
        delimiter: u8,
    ) -> std::result::Result<&'a [u8], ()> {
        let size = range.end.checked_sub(range.start).ok_or(())?.min(4096);
        let bytes = self.read_bytes_at(range.start, size)?;
        bytes
            .iter()
            .position(|b| *b == delimiter)
            .map(|end| &bytes[..end])
            .ok_or(())
    }
}

pub fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    let cache = ReadCache::new(File::open(path)?);
    let budget = Cell::new(8 * 1024 * 1024);
    let object = object::File::parse(Reader {
        cache: &cache,
        budget: &budget,
    })
    .context(t().features.plugin_library_unreadable)?;
    let Some(section) = object.section_by_name(".fwmeta") else {
        return Ok(None);
    };
    ensure!(
        section.size() <= 128 * 1024,
        "{}",
        t().features.plugin_manifest_section_too_large
    );
    let data = section
        .data()
        .context(t().features.plugin_manifest_unreadable_embedded)?;
    ensure!(
        data.len() >= sdk::MANIFEST_HEADER && &data[..8] == sdk::MANIFEST_MAGIC,
        "{}",
        t().features.plugin_manifest_header_invalid
    );
    ensure!(
        u32::from_le_bytes(data[8..12].try_into()?) == 1,
        "{}",
        t().features.plugin_manifest_version_unsupported
    );
    let len = u32::from_le_bytes(data[12..16].try_into()?) as usize;
    ensure!(
        len <= sdk::MAX_MANIFEST_BYTES && len <= data.len() - sdk::MANIFEST_HEADER,
        "{}",
        t().features.plugin_manifest_length_invalid
    );
    Ok(Some(
        data[sdk::MANIFEST_HEADER..sdk::MANIFEST_HEADER + len].to_vec(),
    ))
}

pub fn candidates(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut paths = BTreeSet::new();
    for entry in std::fs::read_dir(root)?.take(128) {
        let directory = entry?.path();
        if !directory.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&directory)?.take(256) {
            let path = file?.path();
            if !path.is_file()
                || !path.extension().is_some_and(|e| {
                    let e = e.to_string_lossy();
                    e.eq_ignore_ascii_case("dll")
                })
            {
                continue;
            }
            if !matches!(read(&path), Ok(None)) {
                paths.insert(path);
            }
        }
    }
    Ok(paths.into_iter().collect())
}
