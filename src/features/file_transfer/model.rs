//! File-transfer model shared by the browser, the persisted task records and
//! both roles. The FILE v2 wire messages live in [`super::wire`].
use serde::{Deserialize, Serialize};

/// [`FileEntry::entry_type`] values. Values below [`entry::FILE`] open as folders.
pub(crate) mod entry {
    pub const DIRECTORY: i32 = 0;
    /// A directory junction or symbolic link (local pane only).
    pub const DIRECTORY_LINK: i32 = 2;
    pub const DRIVE: i32 = 3;
    pub const FILE: i32 = 4;
    /// A file link, or any link of the remote host (never followed).
    pub const FILE_LINK: i32 = 5;
}

/// One row of a directory listing.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct FileEntry {
    /// One of [`entry`].
    pub entry_type: i32,
    pub name: String,
    pub size: u64,
    /// Nanoseconds since the Unix epoch; 0 when unknown.
    pub modified_ns: u64,
    pub full_path: String,
    /// Named location icon (`disk`, `desktop`, …) or the extension with its dot.
    pub icon_type: String,
    pub hidden: bool,
}

/// One manifest item of a transfer. Persisted in task records.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileInfo {
    /// `/`-separated path relative to the transfer root.
    pub rel_path: String,
    pub size: u64,
    /// Nanoseconds since the Unix epoch.
    pub modified_ns: u64,
    /// An empty directory rather than a file.
    pub directory: bool,
}

impl FileInfo {
    pub(crate) fn file(rel_path: String, size: u64, modified_ns: u64) -> Self {
        Self {
            rel_path,
            size,
            modified_ns,
            directory: false,
        }
    }
    pub(crate) fn directory(rel_path: String) -> Self {
        Self {
            rel_path,
            directory: true,
            ..Default::default()
        }
    }
    /// Modification time in nanoseconds.
    pub(crate) fn mtime_ns(&self) -> u64 {
        self.modified_ns
    }
    /// Whether a file modified at `ns` still matches this item.
    pub(crate) fn same_time(&self, ns: u64) -> bool {
        self.modified_ns == ns
    }
    /// The same item of the same source.
    pub(crate) fn same(&self, other: &Self) -> bool {
        self == other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_compare_by_nanoseconds() {
        let item = FileInfo::file("a/b.txt".into(), 3, 1_700_000_000_123_456_700);
        assert!(item.same(&item.clone()));
        assert!(item.same_time(1_700_000_000_123_456_700));
        assert!(!item.same_time(1_700_000_000_123_456_800));
        let json = serde_json::to_string(&item).unwrap();
        assert_eq!(serde_json::from_str::<FileInfo>(&json).unwrap(), item);
    }
}
