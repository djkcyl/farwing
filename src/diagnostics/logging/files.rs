use super::*;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

pub const MAX_FILE_MIB: u64 = 16;
pub const MAX_TOTAL_MIB: u64 = 256;
pub const RETENTION_DAYS: u64 = 14;

pub(super) fn directories() -> Result<(PathBuf, PathBuf)> {
    if crate::platform::host_service::vault::applies()? {
        let base = crate::platform::host_service::vault::root()?;
        return Ok((base.clone(), base.join("logs")));
    }
    {
        let base = crate::platform::paths::require_local_app_data()?.join("OpenUUYC");
        Ok((base.clone(), base.join("logs")))
    }
}

fn create(path: &Path, append: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true);
    if append {
        options.append(true).create(true);
    } else {
        options.create_new(true);
    }

    options.open(path)
}

fn open_lease(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

fn state_directory(directory: &Path) -> io::Result<PathBuf> {
    let state = directory.join(".state");
    fs::create_dir_all(&state)?;

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW},
            core::PCWSTR,
        };
        let path: Vec<u16> = state.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe { SetFileAttributesW(PCWSTR(path.as_ptr()), FILE_ATTRIBUTE_HIDDEN) }
            .map_err(io::Error::other)?;
    }
    // The leading dot already hides the directory on Linux desktops.
    Ok(state)
}

fn rotation_lock(directory: &Path) -> io::Result<File> {
    let file = open_lease(&directory.join(".state/rotation.lock"))?;
    file.lock()?;
    Ok(file)
}

pub(super) fn save_config(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = create(&temporary, false)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);

        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows::{
                Win32::Storage::FileSystem::{
                    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
                },
                core::PCWSTR,
            };
            let from: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
            let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            unsafe {
                MoveFileExW(
                    PCWSTR(from.as_ptr()),
                    PCWSTR(to.as_ptr()),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            }?;
        }
        #[cfg(not(windows))]
        {
            // rename(2) replaces the target atomically within one filesystem.
            fs::rename(&temporary, path)?;
        }

        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.context("保存日志设置")
}

pub(super) fn open_directory(directory: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
            core::{PCWSTR, w},
        };
        // Runtime.directory has already been resolved to the physical directory.
        let path: Vec<u16> = directory.as_os_str().encode_wide().chain(Some(0)).collect();
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(path.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize <= 32 {
            bail!("打开日志文件夹失败（{}）", result.0 as isize);
        }
    }
    #[cfg(not(windows))]
    {
        let status = std::process::Command::new("xdg-open")
            .arg(directory)
            .status()
            .context("启动 xdg-open")?;
        if !status.success() {
            bail!("打开日志文件夹失败（{status}）");
        }
    }

    Ok(())
}

// Sync outside the producer and writer locks, at most once per watcher tick.
// Windows directory enumeration can otherwise show stale size/time until close.
struct PendingSync {
    file: File,
    written: AtomicU64,
    synced: AtomicU64,
}
impl PendingSync {
    fn new(file: &File) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            file: file.try_clone()?,
            written: AtomicU64::new(0),
            synced: AtomicU64::new(0),
        }))
    }
    fn flush(&self) -> io::Result<()> {
        let written = self.written.load(Ordering::Acquire);
        if written != self.synced.load(Ordering::Acquire) {
            self.file.sync_data()?;
            self.synced.store(written, Ordering::Release);
        }
        Ok(())
    }
}
pub(super) fn sync_pending(status: &Arc<Mutex<Status>>) {
    let pending = status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pending
        .clone();
    if let Some(pending) = pending
        && let Err(error) = pending.flush()
    {
        let mut state = status.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .pending
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &pending))
        {
            state.error = Some(format!("日志刷新失败：{error}"));
        }
    }
}

#[derive(Default)]
pub(super) struct Status {
    pub path: PathBuf,
    pub error: Option<String>,
    pending: Option<Arc<PendingSync>>,
}
pub(super) struct Writer {
    file: File,
    pending: Arc<PendingSync>,
    // A separate lease keeps logs readable in editors while they are being written.
    _lease: Option<File>,
    directory: PathBuf,
    stem: Option<String>,
    sequence: u32,
    size: u64,
    day: chrono::NaiveDate,
    status: Arc<Mutex<Status>>,
}
// All file mutations are serialized on the log directory lock, exclusively on
// background log-writer threads. The producer remains the bounded nonblocking queue.
impl Writer {
    pub fn new(
        directory: &Path,
        explicit: Option<&Path>,
        status: Arc<Mutex<Status>>,
        launch_stem: &str,
    ) -> Result<Self> {
        state_directory(directory)?;
        let creation_lock = rotation_lock(directory)?;
        let stem = explicit.is_none().then(|| launch_stem.to_owned());
        let sequence = stem
            .as_deref()
            .map(|stem| current_sequence(directory, stem))
            .transpose()?
            .unwrap_or(0);
        let path = explicit
            .map(Path::to_owned)
            .unwrap_or_else(|| directory.join(format!("{launch_stem}.{sequence:04}.log")));
        let file = create(&path, true).context("打开日志文件")?;
        let lease = if stem.is_some() {
            let lease = open_lease(&directory.join(".state").join(format!("{launch_stem}.lock")))?;
            lease.lock_shared().context("锁定当前日志租约")?;
            Some(lease)
        } else {
            None
        };
        let metadata = file.metadata()?;
        let size = metadata.len();
        let day = chrono::DateTime::<chrono::Utc>::from(
            metadata.created().or_else(|_| metadata.modified())?,
        )
        .date_naive();
        if let Some(stem) = &stem {
            save_sequence(directory, stem, sequence)?;
        }
        let pending = PendingSync::new(&file)?;
        {
            let mut state = status.lock().unwrap_or_else(|e| e.into_inner());
            state.path = path;
            state.pending = Some(pending.clone());
        }
        let writer = Self {
            file,
            pending,
            _lease: lease,
            directory: directory.to_owned(),
            stem,
            sequence,
            size,
            day,
            status,
        };
        drop(creation_lock);
        if writer.stem.is_some() {
            writer.prune();
        }
        Ok(writer)
    }
    fn prune(&self) {
        if let Err(e) = prune(&self.directory) {
            self.status.lock().unwrap_or_else(|e| e.into_inner()).error =
                Some(format!("清理历史日志失败：{e}"));
        }
    }
    fn select_segment(&mut self, sequence: u32) -> io::Result<()> {
        let stem = self.stem.as_ref().unwrap();
        let path = self.directory.join(format!("{stem}.{sequence:04}.log"));
        let file = create(&path, true)?;
        let metadata = file.metadata()?;
        self.size = metadata.len();
        self.day = chrono::DateTime::<chrono::Utc>::from(
            metadata.created().or_else(|_| metadata.modified())?,
        )
        .date_naive();
        self.pending.flush()?;
        self.pending = PendingSync::new(&file)?;
        self.file = file;
        self.sequence = sequence;
        let mut state = self.status.lock().unwrap_or_else(|e| e.into_inner());
        state.path = path;
        state.pending = Some(self.pending.clone());
        Ok(())
    }
    fn write_inner(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let write_lock = rotation_lock(&self.directory)?;
        let mut rotated = false;
        if let Some(stem) = self.stem.clone() {
            let sequence = current_sequence(&self.directory, &stem)?;
            if sequence != self.sequence {
                self.select_segment(sequence)?;
            }
            self.size = self.file.metadata()?.len();
            let now = chrono::Utc::now().date_naive();
            if self.size != 0
                && (self.size.saturating_add(bytes.len() as u64) > MAX_FILE_MIB * 1048576
                    || now != self.day)
            {
                let sequence = self
                    .sequence
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("log rotation exhausted"))?;
                self.select_segment(sequence)?;
                save_sequence(&self.directory, &stem, sequence)?;
                rotated = true;
            }
        }
        self.file.write_all(bytes)?;
        self.pending.written.fetch_add(1, Ordering::Release);
        self.size = self.size.saturating_add(bytes.len() as u64);
        drop(write_lock);
        if rotated {
            self.prune();
        }
        Ok(bytes.len())
    }
}

fn save_sequence(directory: &Path, stem: &str, sequence: u32) -> io::Result<()> {
    fs::write(
        directory.join(".state").join(format!("{stem}.cursor")),
        sequence.to_le_bytes(),
    )
}

fn current_sequence(directory: &Path, stem: &str) -> io::Result<u32> {
    let cursor = fs::read(directory.join(".state").join(format!("{stem}.cursor")))
        .ok()
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
        .map(u32::from_le_bytes);
    let mut sequence = if let Some(sequence) = cursor {
        sequence
    } else {
        // Recover a cursor interrupted by process termination from committed segments.
        let prefix = format!("{stem}.");
        let mut latest = 0;
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            if let Some(segment) = name
                .to_str()
                .and_then(|name| name.strip_prefix(&prefix))
                .and_then(|name| name.strip_suffix(".log"))
                .and_then(|value| value.parse::<u32>().ok())
            {
                latest = latest.max(segment);
            }
        }
        latest
    };
    // A segment may have been created just before its writer exited while updating the cursor.
    while let Some(next) = sequence.checked_add(1) {
        if !directory
            .join(format!("{stem}.{next:04}.log"))
            .try_exists()?
        {
            break;
        }
        sequence = next;
    }
    Ok(sequence)
}
impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let result = self.write_inner(bytes);
        if let Err(e) = &result {
            self.status.lock().unwrap_or_else(|e| e.into_inner()).error =
                Some(format!("日志写盘失败：{e}"));
        }
        result
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if let Err(error) = self.pending.flush() {
            self.status.lock().unwrap_or_else(|e| e.into_inner()).error =
                Some(format!("日志刷新失败：{error}"));
        }
    }
}

// Only our generated files are eligible; never recurse, follow links or remove
// explicit --log-file outputs. Active processes hold a separate lease until close.
pub(super) fn managed_name(name: &str) -> bool {
    let Some((stem, segment)) = name.strip_suffix(".log").and_then(|s| s.rsplit_once('.')) else {
        return false;
    };
    let parts: Vec<_> = stem.split('-').collect();
    parts.len() == 4
        && parts[0] == "openuuyc"
        && chrono::NaiveDateTime::parse_from_str(parts[1], "%Y%m%dT%H%M%SZ").is_ok()
        && parts[2].parse::<u32>().is_ok()
        && parts[3].len() == 32
        && uuid::Uuid::parse_str(parts[3]).is_ok()
        && segment.len() >= 4
        && segment.parse::<u32>().is_ok()
}
fn prune(directory: &Path) -> io::Result<()> {
    let _rotation = rotation_lock(directory)?;
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() || !managed_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let metadata = entry.metadata()?;
        entries.push((metadata.modified()?, metadata.len(), entry.path()));
    }
    entries.sort_by_key(|e| e.0);
    let mut total: u64 = entries.iter().map(|e| e.1).sum();
    for (modified, size, path) in entries {
        let expired = SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default()
            > Duration::from_secs(RETENTION_DAYS * 86400);
        if !expired && total <= MAX_TOTAL_MIB * 1048576 {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy();
        let (stem, segment) = name.strip_suffix(".log").unwrap().rsplit_once('.').unwrap();
        let lease_path = directory.join(".state").join(format!("{stem}.lock"));
        let Ok(lease) = OpenOptions::new().read(true).write(true).open(&lease_path) else {
            continue;
        };
        let inactive = lease.try_lock().is_ok();
        if !inactive && segment.parse::<u32>().unwrap() == current_sequence(directory, stem)? {
            continue;
        }
        // Retain the lease through deletion so cleanup cannot retire a live writer.
        match fs::remove_file(&path) {
            Ok(()) => {
                total = total.saturating_sub(size);
                if inactive {
                    let prefix = format!("{stem}.");
                    let remaining = fs::read_dir(directory)?
                        .filter_map(Result::ok)
                        .any(|entry| {
                            let name = entry.file_name();
                            let name = name.to_string_lossy();
                            name.starts_with(&prefix) && name.ends_with(".log")
                        });
                    if !remaining {
                        let _ = fs::remove_file(lease_path);
                        let _ = fs::remove_file(
                            directory.join(".state").join(format!("{stem}.cursor")),
                        );
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
