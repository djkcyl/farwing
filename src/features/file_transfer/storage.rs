use super::{
    model::FileInfo,
    service::{PartialFile, Record},
    wire::{self, conflict},
};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};
use tokio_util::sync::CancellationToken;

pub(super) const MAX_FILES: usize = 100_000;
/// Bytes before a resume point whose SHA-256 both sides compare (§8).
pub(super) const TAIL: u64 = 1024 * 1024;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

pub(super) fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    let value = unsafe {
        windows::Win32::UI::Shell::SHGetKnownFolderPath(
            id,
            windows::Win32::UI::Shell::KF_FLAG_DEFAULT,
            None,
        )
    }
    .ok()?;
    let path = unsafe { value.to_string() }.ok().map(PathBuf::from);
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(value.0.cast())) };
    path.filter(|p| p.is_dir())
}
pub(super) fn safe_relative(name: &str) -> Result<PathBuf> {
    ensure!(
        !name.is_empty() && name.encode_utf16().count() < 30_000,
        "文件名为空或过长"
    );
    let mut result = PathBuf::new();
    for part in name.split(['\\', '/']) {
        ensure!(
            !part.is_empty()
                && !matches!(part, "." | "..")
                && !part.ends_with(['.', ' '])
                && !part.chars().any(|c| c < ' ' || ":<>\"|?*".contains(c)),
            "远端返回了不安全的文件路径"
        );
        let stem = part.split('.').next().unwrap_or("").to_uppercase();
        ensure!(
            !matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
            ) && !(stem.starts_with("COM") || stem.starts_with("LPT"))
                .then(|| &stem[3..])
                .is_some_and(|n| matches!(
                    n,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )),
            "不能使用设备名作为文件名"
        );
        result.push(part);
    }
    Ok(result)
}
pub(super) fn validate_manifest(files: &[FileInfo]) -> Result<u64> {
    ensure!(files.len() <= MAX_FILES, "文件清单过大");
    let mut names = std::collections::HashSet::new();
    let mut total = 0u64;
    for f in files {
        safe_relative(&f.rel_path)?;
        ensure!(
            names.insert(f.rel_path.replace('/', "\\").to_lowercase()),
            "文件清单包含重复路径"
        );
        ensure!(!f.directory || f.size == 0, "文件清单中的目录带有数据");
        total = total.checked_add(f.size).context("文件大小溢出")?;
        ensure!(f.size <= i64::MAX as u64, "文件大小超出可寻址范围");
    }
    Ok(total)
}
/// Modification time in nanoseconds since the Unix epoch (100 ns steps).
pub(super) fn modified_ns(m: &std::fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|v| v.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |v| u64::try_from(v.as_nanos()).unwrap_or(0))
}
fn not_link(p: &Path) -> Result<std::fs::Metadata> {
    let m = std::fs::symlink_metadata(p)?;
    ensure!(
        m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "不允许通过链接或重解析点传输：{}",
        p.display()
    );
    Ok(m)
}
pub(super) fn local_path(value: &str) -> Result<PathBuf> {
    let path = if value == ":/Default" {
        known_folder(&windows::Win32::UI::Shell::FOLDERID_Downloads)
            .context("用户下载目录不可用")?
    } else if value.len() == 2
        && value.as_bytes()[0].is_ascii_alphabetic()
        && value.as_bytes()[1] == b':'
    {
        // A drive item is a volume root, never a process-relative drive cwd.
        PathBuf::from(format!("{value}\\"))
    } else {
        PathBuf::from(value)
    };
    let text = path.to_str().context("文件路径无效")?;
    let bytes = text.as_bytes();
    ensure!(
        bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'/' | b'\\'),
        "仅支持本地磁盘的绝对路径"
    );
    let mut result = PathBuf::from(format!("{}:\\", bytes[0] as char));
    let tail = text[3..].trim_end_matches(['\\', '/']);
    if !tail.is_empty() {
        result.push(safe_relative(tail)?);
    }
    // Every existing ancestor must stay a normal directory, not a junction or device path.
    let mut current = PathBuf::new();
    for part in result.components() {
        current.push(part);
        if !current.is_absolute() {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(m) => ensure!(
                m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
                "不允许通过重解析点访问文件"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(result)
}
pub(super) fn canonical_dir(p: &Path) -> Result<PathBuf> {
    ensure!(p.is_absolute(), "本地目录不是绝对路径");
    ensure!(not_link(p)?.is_dir(), "本地路径不是目录");
    Ok(dunce::canonicalize(p)?)
}
fn underneath(path: &Path, root: &Path) -> bool {
    let a = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect::<Vec<_>>();
    let b = root
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect::<Vec<_>>();
    a.starts_with(&b)
}
fn check_handle(f: &File, root: &Path) -> Result<()> {
    ensure!(
        f.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "打开的文件已变为链接"
    );
    let mut path = vec![0u16; 32768];
    let n = unsafe {
        windows::Win32::Storage::FileSystem::GetFinalPathNameByHandleW(
            windows::Win32::Foundation::HANDLE(f.as_raw_handle()),
            &mut path,
            windows::Win32::Storage::FileSystem::FILE_NAME_NORMALIZED,
        )
    } as usize;
    ensure!(n > 0 && n < path.len(), "无法核对文件实际位置");
    let actual = PathBuf::from(String::from_utf16(&path[..n])?);
    ensure!(
        underneath(dunce::simplified(&actual), root),
        "文件实际位置超出已选择目录"
    );
    Ok(())
}
/// The manifest of `source`: its root directory, the top-level folder name
/// (empty for a single file) and every file and empty directory below it.
pub(super) fn scan(
    source: &Path,
    cancel: &CancellationToken,
) -> Result<(PathBuf, String, Vec<FileInfo>)> {
    let m = not_link(source)?;
    let source = dunce::canonicalize(source)?;
    let folder = if m.is_dir() {
        source
            .file_name()
            .context("不能传输整个磁盘")?
            .to_str()
            .context("文件夹名称无效")?
            .to_owned()
    } else {
        String::new()
    };
    let root = if m.is_dir() {
        source.clone()
    } else {
        source.parent().context("文件没有父目录")?.to_owned()
    };
    let relative = |path: &Path| -> Result<String> {
        Ok(path
            .strip_prefix(&root)?
            .to_str()
            .context("文件名称无效")?
            .replace('\\', "/"))
    };
    let mut files = Vec::new();
    let mut pending = vec![source];
    let mut visited = 0usize;
    while let Some(path) = pending.pop() {
        ensure!(!cancel.is_cancelled(), "已取消扫描");
        visited += 1;
        ensure!(visited <= MAX_FILES * 2, "目录包含过多项目");
        let m = not_link(&path)?;
        if m.is_dir() {
            let before = pending.len();
            for e in std::fs::read_dir(&path)? {
                pending.push(e?.path());
            }
            // The root itself becomes the receiver's top-level folder.
            if pending.len() == before && path != root {
                files.push(FileInfo::directory(relative(&path)?));
            }
        } else if m.is_file() {
            files.push(FileInfo::file(relative(&path)?, m.len(), modified_ns(&m)));
        }
        ensure!(files.len() <= MAX_FILES, "文件数量过多");
    }
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    validate_manifest(&files)?;
    Ok((root, folder, files))
}
pub(super) fn open_source(root: &Path, item: &FileInfo) -> Result<File> {
    ensure!(!item.directory, "目录没有文件内容");
    let path = root.join(safe_relative(&item.rel_path)?);
    let f = OpenOptions::new()
        .read(true)
        .share_mode(1)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path)?;
    check_handle(&f, root)?;
    let m = f.metadata()?;
    ensure!(
        m.is_file() && m.len() == item.size && item.same_time(modified_ns(&m)),
        wire::Failure::new(wire::code::CHANGED, "源文件已变化，无法续传")
    );
    Ok(f)
}
/// SHA-256 of the at most [`TAIL`] bytes of `file` before `offset`; empty at 0.
pub(super) fn tail_sha256(file: &mut File, offset: u64) -> Result<Vec<u8>> {
    if offset == 0 {
        return Ok(Vec::new());
    }
    let start = offset - offset.min(TAIL);
    file.seek(SeekFrom::Start(start))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    let mut remaining = offset - start;
    while remaining > 0 {
        let n = remaining.min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..n])?;
        hasher.update(&buffer[..n]);
        remaining -= n as u64;
    }
    Ok(hasher.finalize().to_vec())
}
/// The start a sender can confirm for `plan` of its source `item`: the
/// receiver's offset when its tail matches the same bytes here, else 0.
pub(super) fn confirm(root: &Path, item: &FileInfo, plan: &wire::ItemPlan) -> Result<u64> {
    if plan.skip || plan.offset == 0 || item.directory {
        return Ok(0);
    }
    ensure!(plan.offset <= item.size, "对端续传位置超过源文件");
    let mut file = open_source(root, item)?;
    Ok(if tail_sha256(&mut file, plan.offset)? == plan.tail_sha256 {
        plan.offset
    } else {
        0
    })
}
// Hold ancestor directory handles without FILE_SHARE_DELETE until finalization.
// This prevents a selected parent being swapped for a junction during a write.
pub(super) fn parents(root: &Path, relative: &Path, create: bool) -> Result<Vec<File>> {
    let mut locks = Vec::new();
    let mut p = root.to_path_buf();
    let lock_dir = |p: &Path| -> Result<File> {
        ensure!(not_link(p)?.is_dir(), "目标父路径不是目录");
        let f = OpenOptions::new()
            .read(true)
            .share_mode(3)
            .custom_flags(0x0200_0000 | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(p)?;
        check_handle(&f, root)?;
        Ok(f)
    };
    locks.push(lock_dir(&p)?);
    if let Some(parent) = relative.parent() {
        for c in parent.components() {
            p.push(c);
            if create && !p.exists() {
                std::fs::create_dir(&p)?;
            }
            locks.push(lock_dir(&p)?);
        }
    }
    Ok(locks)
}
/// An item being written to its hidden partial file.
pub(super) struct Receiving {
    pub file: tokio::fs::File,
    pub partial: PartialFile,
    pub position: u64,
    pub _locks: Vec<File>,
}
/// What a receiver offers for one manifest item, and the record entry it keeps.
pub(super) struct Holding {
    pub plan: wire::ItemPlan,
    pub partial: PartialFile,
}
/// Decide where receiving `item` under `root` starts: its partial file (with
/// the tail digest), already saved, skipped by `policy`, or from the start.
pub(super) fn plan(
    root: &Path,
    key: &str,
    item: &FileInfo,
    policy: i32,
    old: Option<&PartialFile>,
) -> Result<Holding> {
    ensure!(uuid::Uuid::parse_str(key).is_ok(), "续传任务身份无效");
    let relative = safe_relative(&item.rel_path)?;
    let skip = |partial: PartialFile| Holding {
        plan: wire::ItemPlan {
            skip: true,
            ..Default::default()
        },
        partial,
    };
    if let Some(old) = old {
        ensure!(
            old.info.same(item),
            wire::Failure::new(wire::code::CHANGED, "源文件已变化，无法续传")
        );
        if old.done || old.skipped {
            return Ok(skip(old.clone()));
        }
        if !item.directory {
            let target = safe_relative(&old.target)?;
            let temp = temp_path(root, &target, key);
            match std::fs::symlink_metadata(&temp) {
                Ok(m) if m.is_file() && m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 => {
                    let _locks = parents(root, &target, false)?;
                    let mut file = OpenOptions::new()
                        .read(true)
                        .share_mode(1)
                        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                        .open(&temp)?;
                    check_handle(&file, root)?;
                    let offset = file.metadata()?.len();
                    if offset <= item.size {
                        return Ok(Holding {
                            plan: wire::ItemPlan {
                                offset,
                                tail_sha256: tail_sha256(&mut file, offset)?,
                                skip: false,
                            },
                            partial: old.clone(),
                        });
                    }
                }
                Ok(_) => anyhow::bail!("续传临时路径已被替换"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Saved just before its record was: the target carries the
                    // item's size and time from finalization.
                    if finalized(&root.join(&target), item) {
                        return Ok(skip(PartialFile {
                            done: true,
                            ..old.clone()
                        }));
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        return Ok(Holding {
            plan: Default::default(),
            partial: old.clone(),
        });
    }
    let partial = PartialFile {
        info: item.clone(),
        target: item.rel_path.clone(),
        done: false,
        skipped: false,
    };
    if !item.directory && policy == conflict::SKIP && root.join(&relative).exists() {
        return Ok(skip(PartialFile {
            skipped: true,
            ..partial
        }));
    }
    Ok(Holding {
        plan: Default::default(),
        partial,
    })
}
fn finalized(path: &Path, item: &FileInfo) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| {
        m.is_file()
            && m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
            && m.len() == item.size
            && item.same_time(modified_ns(&m))
    })
}
/// Open the partial file of `partial` positioned at `offset`; the bytes
/// after `offset` are discarded.
pub(super) fn open(root: &Path, key: &str, partial: &PartialFile, offset: u64) -> Result<Receiving> {
    ensure!(uuid::Uuid::parse_str(key).is_ok(), "续传任务身份无效");
    ensure!(!partial.info.directory, "目录没有文件内容");
    let target = safe_relative(&partial.target)?;
    let locks = parents(root, &target, true)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .share_mode(1)
        .attributes(FILE_ATTRIBUTE_HIDDEN)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(temp_path(root, &target, key))?;
    check_handle(&file, root)?;
    let length = file.metadata()?.len();
    ensure!(
        offset <= length && offset <= partial.info.size,
        wire::Failure::new(wire::code::CHANGED, "续传位置超出已接收的数据")
    );
    if length != offset {
        file.set_len(offset)?;
    }
    file.seek(SeekFrom::Start(offset))?;
    Ok(Receiving {
        file: tokio::fs::File::from_std(file),
        partial: partial.clone(),
        position: offset,
        _locks: locks,
    })
}
/// Create the empty directory `item` under `root`.
pub(super) fn make_directory(root: &Path, item: &FileInfo) -> Result<()> {
    let relative = safe_relative(&item.rel_path)?;
    let _locks = parents(root, &relative, true)?;
    let path = root.join(&relative);
    match std::fs::create_dir(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    ensure!(not_link(&path)?.is_dir(), "目标已存在且不是目录");
    Ok(())
}
pub(super) fn temp_path(root: &Path, relative: &Path, key: &str) -> PathBuf {
    let mut p = root.join(relative);
    let name = p.file_name().unwrap_or_default().to_string_lossy();
    p.set_file_name(format!(".{name}.{key}.downloading"));
    p
}
/// Set the final modification time (100 ns precision) and drop the hidden
/// attribute of a finished partial file.
fn seal(file: &File, mtime_ns: u64) -> Result<()> {
    use windows::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx, SetFileInformationByHandle,
    };
    let handle = windows::Win32::Foundation::HANDLE(file.as_raw_handle());
    let size = std::mem::size_of::<FILE_BASIC_INFO>() as u32;
    let mut current = FILE_BASIC_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(handle, FileBasicInfo, (&raw mut current).cast(), size)
    }?;
    let attributes = current.FileAttributes & !FILE_ATTRIBUTE_HIDDEN;
    let update = FILE_BASIC_INFO {
        // Zero leaves a time unchanged.
        LastWriteTime: if mtime_ns > 0 {
            (mtime_ns / 100) as i64 + 116_444_736_000_000_000
        } else {
            0
        },
        // FILE_ATTRIBUTE_NORMAL is only valid alone.
        FileAttributes: if attributes == 0 { 0x80 } else { attributes },
        ..Default::default()
    };
    unsafe { SetFileInformationByHandle(handle, FileBasicInfo, (&raw const update).cast(), size) }?;
    Ok(())
}
fn unique_name(root: &Path, relative: &Path) -> Result<PathBuf> {
    let stem = relative
        .file_stem()
        .and_then(|s| s.to_str())
        .context("文件名无效")?
        .to_owned();
    let ext = relative
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| format!(".{s}"))
        .unwrap_or_default();
    let mut target = relative.to_path_buf();
    for n in 1..10000 {
        target.set_file_name(format!("{stem} ({n}){ext}"));
        if std::fs::symlink_metadata(root.join(&target)).is_err() {
            return Ok(target);
        }
    }
    anyhow::bail!("无法分配不重名的文件名")
}
/// Save a complete partial file under its final name per `policy`: replace
/// (overwrite), pick a free "name (n)" (rename) or drop it (skip) when the
/// name is taken by then.
pub(super) async fn finish(
    v: Receiving,
    root: &Path,
    key: &str,
    policy: i32,
) -> Result<PartialFile> {
    ensure!(
        v.position == v.partial.info.size,
        "收到的文件长度与清单不一致"
    );
    let Receiving {
        file,
        mut partial,
        _locks: locks,
        ..
    } = v;
    file.sync_all().await?;
    let file = file.into_std().await;
    let root = root.to_owned();
    let key = key.to_owned();
    tokio::task::spawn_blocking(move || -> Result<PartialFile> {
        let _locks = locks;
        seal(&file, partial.info.mtime_ns())?;
        drop(file);
        let relative = safe_relative(&partial.target)?;
        let source = temp_path(&root, &relative, &key);
        let mut target = relative.clone();
        let mut replace = false;
        if std::fs::symlink_metadata(root.join(&target)).is_ok() {
            ensure!(
                not_link(&root.join(&target))?.is_file(),
                "目标已经变成目录或链接"
            );
            match policy {
                conflict::OVERWRITE => replace = true,
                conflict::SKIP => {
                    std::fs::remove_file(&source)?;
                    partial.skipped = true;
                    return Ok(partial);
                }
                _ => target = unique_name(&root, &relative)?,
            }
        }
        use std::os::windows::ffi::OsStrExt;
        let wide = |p: &Path| p.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<_>>();
        let (from, to) = (wide(&source), wide(&root.join(&target)));
        use windows::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        let flags = if replace {
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH
        } else {
            MOVEFILE_WRITE_THROUGH
        };
        unsafe {
            MoveFileExW(
                windows::core::PCWSTR(from.as_ptr()),
                windows::core::PCWSTR(to.as_ptr()),
                flags,
            )
        }
        .context("无法完成目标文件保存")?;
        partial.target = target.to_str().context("目标文件名无效")?.replace('\\', "/");
        partial.done = true;
        Ok(partial)
    })
    .await?
}
pub(super) fn cleanup(root: &Path, key: &str, items: &[PartialFile]) -> Result<()> {
    ensure!(uuid::Uuid::parse_str(key).is_ok(), "续传任务身份无效");
    for item in items
        .iter()
        .filter(|i| !i.done && !i.skipped && !i.info.directory)
    {
        let rel = safe_relative(&item.target)?;
        let _locks = match parents(root, &rel, false) {
            Ok(locks) => locks,
            // The folder of a partial that never started does not exist.
            Err(e)
                if e
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(e) => return Err(e),
        };
        let p = temp_path(root, &rel, key);
        match std::fs::symlink_metadata(&p) {
            Ok(m) => {
                ensure!(
                    m.is_file() && m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
                    "临时路径已被替换"
                );
                std::fs::remove_file(p)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
#[derive(Clone)]
pub(crate) struct Store(PathBuf);
impl Store {
    pub(crate) fn new(account: &str, device: &str) -> Result<Self> {
        crate::account::api::validate_device_id(device)?;
        ensure!(!account.is_empty(), "无法确认当前账号");
        Ok(Self(
            crate::product::local_data_dir()
                .context("本地配置目录不可用")?
                .join("file-transfer")
                .join(format!("{:x}", Sha256::digest(account)))
                .join(format!("{device}.json")),
        ))
    }
    pub(super) fn load(&self) -> Result<Vec<Record>> {
        let f = match File::open(&self.0) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut b = Vec::new();
        f.take(64 * 1024 * 1024 + 1).read_to_end(&mut b)?;
        ensure!(b.len() <= 64 * 1024 * 1024, "传输历史过大");
        let records: Vec<Record> = serde_json::from_slice(&b)?;
        ensure!(records.len() <= 256, "传输历史过多");
        for r in &records {
            ensure!(uuid::Uuid::parse_str(&r.key).is_ok(), "传输记录身份无效");
            validate_manifest(&r.files)?;
            for f in &r.partial {
                safe_relative(&f.target)?;
            }
        }
        Ok(records)
    }
    pub(super) fn save(&self, r: &[Record]) -> Result<()> {
        ensure!(r.len() <= 256, "传输记录已满");
        let b = serde_json::to_vec(r)?;
        ensure!(b.len() <= 64 * 1024 * 1024, "传输记录过大");
        std::fs::create_dir_all(self.0.parent().context("历史路径无效")?)?;
        let temp = self
            .0
            .with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            f.write_all(&b)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&temp, &self.0)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temp);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("farwing-ft-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dunce::canonicalize(dir).unwrap()
    }

    #[test]
    fn unsafe_paths_and_device_names_are_rejected() {
        assert!(safe_relative("a/b/c.txt").is_ok());
        for bad in ["", "../x", "a/../b", "a//b", "con", "LPT1.txt", "a/nul.txt", "x:y", "dot."] {
            assert!(safe_relative(bad).is_err(), "{bad}");
        }
        assert!(local_path("C:\\Windows\\..\\x").is_err());
        assert!(local_path("\\\\server\\share").is_err());
        assert!(local_path("relative\\path").is_err());
        assert_eq!(local_path("C:").unwrap(), PathBuf::from("C:\\"));
    }

    #[test]
    fn scan_lists_files_and_empty_directories() {
        let root = scratch("scan");
        let source = root.join("tree");
        std::fs::create_dir_all(source.join("sub/empty")).unwrap();
        std::fs::create_dir_all(source.join("blank")).unwrap();
        std::fs::write(source.join("sub/a.txt"), b"abc").unwrap();
        std::fs::write(source.join("zero.bin"), b"").unwrap();
        let (base, folder, files) = scan(&source, &CancellationToken::new()).unwrap();
        assert_eq!(base, source);
        assert_eq!(folder, "tree");
        let names = files
            .iter()
            .map(|f| (f.rel_path.as_str(), f.directory, f.size))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                ("blank", true, 0),
                ("sub/a.txt", false, 3),
                ("sub/empty", true, 0),
                ("zero.bin", false, 0)
            ]
        );
        assert!(files[1].modified_ns > 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn plan_resumes_from_the_partial_and_finish_applies_the_policy() {
        let root = scratch("plan");
        let key = uuid::Uuid::new_v4().to_string();
        let item = FileInfo::file("d/f.bin".into(), 6, 1_600_000_000_123_456_700);
        // Nothing received yet: start at zero.
        let fresh = plan(&root, &key, &item, conflict::RENAME, None).unwrap();
        assert_eq!(fresh.plan, wire::ItemPlan::default());
        let mut receiving = open(&root, &key, &fresh.partial, 0).unwrap();
        let temp = temp_path(&root, Path::new("d/f.bin"), &key);
        assert!(std::fs::metadata(&temp).unwrap().file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            use tokio::io::AsyncWriteExt;
            receiving.file.write_all(b"abcd").await.unwrap();
            receiving.file.flush().await.unwrap();
        });
        drop(receiving);
        // A resume offers the partial length and the digest of its tail.
        let resumed = plan(&root, &key, &item, conflict::RENAME, Some(&fresh.partial)).unwrap();
        assert_eq!(resumed.plan.offset, 4);
        assert_eq!(resumed.plan.tail_sha256, Sha256::digest(b"abcd").to_vec());
        // The sender confirms the offset only when its own bytes match.
        let source = scratch("source");
        let mut same = FileInfo::file("d/f.bin".into(), 6, 0);
        std::fs::create_dir_all(source.join("d")).unwrap();
        std::fs::write(source.join("d/f.bin"), b"abcdef").unwrap();
        same.modified_ns = modified_ns(&std::fs::metadata(source.join("d/f.bin")).unwrap());
        assert_eq!(confirm(&source, &same, &resumed.plan).unwrap(), 4);
        std::fs::write(source.join("d/f.bin"), b"abXdef").unwrap();
        same.modified_ns = modified_ns(&std::fs::metadata(source.join("d/f.bin")).unwrap());
        assert_eq!(confirm(&source, &same, &resumed.plan).unwrap(), 0);
        // An existing target is kept under the rename policy.
        std::fs::write(root.join("d/f.bin"), b"old").unwrap();
        let mut receiving = open(&root, &key, &resumed.partial, 4).unwrap();
        runtime.block_on(async {
            use tokio::io::AsyncWriteExt;
            receiving.file.write_all(b"ef").await.unwrap();
            receiving.position = 6;
            let done = finish(receiving, &root, &key, conflict::RENAME).await.unwrap();
            assert!(done.done);
            assert_eq!(done.target, "d/f (1).bin");
        });
        assert_eq!(std::fs::read(root.join("d/f (1).bin")).unwrap(), b"abcdef");
        assert_eq!(std::fs::read(root.join("d/f.bin")).unwrap(), b"old");
        let saved = std::fs::metadata(root.join("d/f (1).bin")).unwrap();
        assert_eq!(saved.file_attributes() & FILE_ATTRIBUTE_HIDDEN, 0);
        assert_eq!(modified_ns(&saved), 1_600_000_000_123_456_700);
        assert!(!temp.exists());
        // The skip policy skips an existing target up front.
        let skipped = plan(&root, &key, &FileInfo::file("d/f.bin".into(), 1, 0), conflict::SKIP, None).unwrap();
        assert!(skipped.plan.skip && skipped.partial.skipped);
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(source);
    }
}
