use super::{
    protocol::*,
    service::{PartialFile, Record},
};
use anyhow::{Context, Result, ensure};
use prost::Message;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};
#[cfg(windows)]
use std::os::windows::{
    fs::{MetadataExt, OpenOptionsExt},
    io::AsRawHandle,
};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};
use tokio_util::sync::CancellationToken;

pub(super) const MAX_FILES: usize = 100_000;
/// User places offered by the local file browser, resolved per desktop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KnownFolder {
    Desktop,
    Downloads,
    Documents,
}

#[cfg(windows)]
pub(crate) fn known_folder(folder: KnownFolder) -> Option<PathBuf> {
    use windows::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads};
    let id = match folder {
        KnownFolder::Desktop => FOLDERID_Desktop,
        KnownFolder::Downloads => FOLDERID_Downloads,
        KnownFolder::Documents => FOLDERID_Documents,
    };
    let value = unsafe {
        windows::Win32::UI::Shell::SHGetKnownFolderPath(
            &id,
            windows::Win32::UI::Shell::KF_FLAG_DEFAULT,
            None,
        )
    }
    .ok()?;
    let path = unsafe { value.to_string() }.ok().map(PathBuf::from);
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(value.0.cast())) };
    path.filter(|p| p.is_dir())
}

#[cfg(not(windows))]
pub(crate) fn known_folder(folder: KnownFolder) -> Option<PathBuf> {
    let (key, default) = match folder {
        KnownFolder::Desktop => ("XDG_DESKTOP_DIR", "Desktop"),
        KnownFolder::Downloads => ("XDG_DOWNLOAD_DIR", "Downloads"),
        KnownFolder::Documents => ("XDG_DOCUMENTS_DIR", "Documents"),
    };
    crate::platform::file_locations::user_dir(key, default)
}

/// Windows reparse points and Unix symlinks both break out of a chosen root.
pub(super) fn is_link(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.is_symlink()
    }
}

/// Open flags that refuse to traverse a link at the final component.
fn no_follow(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(windows)]
    {
        // FILE_SHARE_READ + FILE_FLAG_OPEN_REPARSE_POINT
        options.share_mode(1).custom_flags(0x00200000)
    }
    #[cfg(not(windows))]
    {
        options.custom_flags(libc::O_NOFOLLOW)
    }
}

/// Directory handle kept open so the path cannot be swapped mid-transfer.
fn no_follow_dir(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(windows)]
    {
        // FILE_SHARE_READ|WRITE + FILE_FLAG_BACKUP_SEMANTICS|FILE_FLAG_OPEN_REPARSE_POINT
        options.share_mode(3).custom_flags(0x02200000)
    }
    #[cfg(not(windows))]
    {
        options.custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
    }
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
        total = total.checked_add(f.size).context("文件大小溢出")?;
        ensure!(f.size <= i64::MAX as u64, "文件大小超出可寻址范围");
    }
    Ok(total)
}
pub(super) fn compress<T: Message>(v: &T) -> Result<Vec<u8>> {
    ensure!(v.encoded_len() <= 32 * 1024 * 1024, "文件清单过大");
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&v.encode_to_vec())?;
    Ok(enc.finish()?)
}
pub(super) fn decompress<T: Message + Default>(v: &[u8]) -> Result<T> {
    let mut bytes = Vec::new();
    flate2::read::ZlibDecoder::new(v)
        .take(32 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 32 * 1024 * 1024, "解压后的文件清单过大");
    Ok(T::decode(bytes.as_slice())?)
}
pub(super) fn modified(m: &std::fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|v| v.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |v| v.as_secs())
}
fn not_link(p: &Path) -> Result<std::fs::Metadata> {
    let m = std::fs::symlink_metadata(p)?;
    ensure!(
        !is_link(&m),
        "不允许通过链接或重解析点传输：{}",
        p.display()
    );
    Ok(m)
}
/// A protocol path from the remote peer as a local absolute path: a drive
/// path on Windows, a `/` path on Unix (the form macOS hosts use). Every
/// existing ancestor must be a plain directory, never a link.
#[cfg(windows)]
pub(super) fn local_path(value: &str) -> Result<PathBuf> {
    let path = if value == ":/Default" {
        known_folder(KnownFolder::Downloads).context("用户下载目录不可用")?
    } else if value.len() == 2
        && value.as_bytes()[0].is_ascii_alphabetic()
        && value.as_bytes()[1] == b':'
    {
        // A drive item is a volume root in this protocol, never a process-relative drive cwd.
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
        "仅支持本地磁盘的绝对路径：{text}"
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
                m.file_attributes() & 0x400 == 0,
                "不允许通过重解析点访问文件"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(result)
}
/// 回给对端的路径形状：分隔符一律用反斜杠。
///
/// 官方主控把它手里的路径原样回给我们（实测：它把我们给的 `full_path` 一字不差地放进
/// 了 `send_request`），但它拼路径时按反斜杠拆 parent/name，整条路径里没有反斜杠就判
/// `invalid_path`、把下载请求的源路径整个丢掉——我们收到的就是空 path。它随后浏览的
/// 也正是我们给的那个字符串，所以对外一律用反斜杠，回来时由 [`local_path`] 换回 POSIX。
/// Windows 侧的路径本来就是反斜杠，这里等于原样。
pub(super) fn wire_path(value: &str) -> String {
    value.replace('/', "\\")
}

#[cfg(not(windows))]
pub(super) fn local_path(value: &str) -> Result<PathBuf> {
    let text = if value == ":/Default" {
        // A download folder whose name is not UTF-8 cannot be spoken on the wire;
        // say so here instead of sending on a U+FFFD path that is then "not found".
        known_folder(KnownFolder::Downloads)
            .context("用户下载目录不可用")?
            .into_os_string()
            .into_string()
            .ok()
            .context("文件路径无效")?
    } else {
        // 对端按 `wire_path` 的形状回话：反斜杠是分隔符，先换回 POSIX 再校验。
        value.replace('\\', "/")
    };
    // 把收到的值带上：这条曾经只报「仅支持本地磁盘的绝对路径」，而对面（官方主控）
    // 到底发了什么形式完全看不出来，排查时只能干瞪眼。
    ensure!(text.starts_with('/'), "仅支持本地磁盘的绝对路径：{text}");
    let mut result = PathBuf::from("/");
    let tail = text.trim_matches('/');
    if !tail.is_empty() {
        result.push(safe_relative(tail)?);
    }
    let mut current = PathBuf::new();
    for part in result.components() {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(m) => ensure!(!is_link(&m), "不允许通过符号链接访问文件"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(result)
}
pub(super) fn canonical_dir(p: &Path) -> Result<PathBuf> {
    ensure!(p.is_absolute(), "请选择完整本地目录");
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
    ensure!(!is_link(&f.metadata()?), "打开的文件已变为链接");
    #[cfg(windows)]
    let actual = {
        let mut path = vec![0u16; 32768];
        let n = unsafe {
            windows::Win32::Storage::FileSystem::GetFinalPathNameByHandleW(
                windows::Win32::Foundation::HANDLE(f.as_raw_handle()),
                &mut path,
                windows::Win32::Storage::FileSystem::FILE_NAME_NORMALIZED,
            )
        } as usize;
        ensure!(n > 0 && n < path.len(), "无法核对文件实际位置");
        PathBuf::from(String::from_utf16(&path[..n])?)
    };
    // /proc/self/fd resolves the handle the same way, after every rename.
    #[cfg(not(windows))]
    let actual = std::fs::read_link(format!("/proc/self/fd/{}", f.as_raw_fd()))
        .context("无法核对文件实际位置")?;
    ensure!(
        underneath(dunce::simplified(&actual), root),
        "文件实际位置超出已选择目录"
    );
    Ok(())
}
pub(super) fn scan(
    source: &Path,
    cancel: &CancellationToken,
) -> Result<(PathBuf, String, Vec<FileInfo>)> {
    let m = not_link(source)?;
    let source = dunce::canonicalize(source)?;
    let folder = if m.is_dir() {
        source
            .file_name()
            .context("请选择文件夹而非整个磁盘")?
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
    let mut files = Vec::new();
    let mut pending = vec![source];
    let mut visited = 0usize;
    while let Some(path) = pending.pop() {
        ensure!(!cancel.is_cancelled(), "已取消扫描");
        visited += 1;
        ensure!(visited <= MAX_FILES * 2, "目录包含过多项目");
        let m = not_link(&path)?;
        if m.is_dir() {
            for e in std::fs::read_dir(&path)? {
                pending.push(e?.path());
            }
        } else if m.is_file() {
            let name = path
                .strip_prefix(&root)?
                .to_str()
                .context("文件名称无效")?
                .replace('\\', "/");
            files.push(FileInfo {
                rel_path: name,
                size: m.len(),
                modified_time: modified(&m),
            });
            ensure!(files.len() <= MAX_FILES, "文件数量过多");
        }
    }
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    validate_manifest(&files)?;
    Ok((root, folder, files))
}
pub(super) fn open_source(root: &Path, item: &FileInfo) -> Result<File> {
    let path = root.join(safe_relative(&item.rel_path)?);
    let f = no_follow(OpenOptions::new().read(true)).open(&path)?;
    check_handle(&f, root)?;
    let m = f.metadata()?;
    ensure!(
        m.is_file() && m.len() == item.size && modified(&m) == item.modified_time,
        "源文件已变化，不能继续原任务"
    );
    Ok(f)
}
// Hold ancestor directory handles without FILE_SHARE_DELETE until finalization.
// This prevents a selected parent being swapped for a junction during a write.
pub(super) fn parents(root: &Path, relative: &Path, create: bool) -> Result<Vec<File>> {
    let mut locks = Vec::new();
    let mut p = root.to_path_buf();
    let lock_dir = |p: &Path| -> Result<File> {
        ensure!(not_link(p)?.is_dir(), "目标父路径不是目录");
        let f = no_follow_dir(OpenOptions::new().read(true)).open(p)?;
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
pub(super) struct Receiving {
    pub file: tokio::fs::File,
    pub partial: PartialFile,
    pub position: u64,
    pub _locks: Vec<File>,
}
pub(super) fn prepare(
    root: &Path,
    key: &str,
    item: &FileInfo,
    policy: i32,
    old: Option<&PartialFile>,
) -> Result<Option<Receiving>> {
    ensure!(uuid::Uuid::parse_str(key).is_ok(), "续传任务身份无效");
    let rel = safe_relative(&item.rel_path)?;
    let mut locks = parents(root, &rel, true)?;
    if let Some(old) = old {
        ensure!(old.info == *item, "源文件已变化，不能续传");
        if old.done || old.skipped {
            return Ok(None);
        }
        let target_rel = safe_relative(&old.target)?;
        locks.extend(parents(root, &target_rel, false)?);
        let temp = temp_path(root, &target_rel, key);
        let file = no_follow(OpenOptions::new().read(true).write(true))
            .open(temp)
            .context("续传临时文件已丢失")?;
        check_handle(&file, root)?;
        let n = file.metadata()?.len();
        ensure!(n <= item.size, "续传临时文件大小无效");
        return Ok(Some(Receiving {
            file: tokio::fs::File::from_std(file),
            partial: old.clone(),
            position: n,
            _locks: locks,
        }));
    }
    let mut target = rel.clone();
    if root.join(&target).exists() {
        ensure!(
            not_link(&root.join(&target))?.is_file(),
            "目标已存在且不是普通文件"
        );
        if policy == 3 {
            return Ok(None);
        }
        if policy == 2 {
            let stem = target
                .file_stem()
                .and_then(|s| s.to_str())
                .context("文件名无效")?
                .to_owned();
            let ext = target
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| format!(".{s}"))
                .unwrap_or_default();
            for n in 1..10000 {
                target.set_file_name(format!("{stem} ({n}){ext}"));
                if !root.join(&target).exists() {
                    break;
                }
            }
            ensure!(!root.join(&target).exists(), "无法分配不重名的文件名");
        }
    }
    let temp = temp_path(root, &target, key);
    let file = no_follow(OpenOptions::new().read(true).write(true).create_new(true)).open(temp)?;
    check_handle(&file, root)?;
    Ok(Some(Receiving {
        file: tokio::fs::File::from_std(file),
        partial: PartialFile {
            info: item.clone(),
            target: target.to_str().context("目标文件名无效")?.into(),
            done: false,
            skipped: false,
        },
        position: 0,
        _locks: locks,
    }))
}
pub(super) fn temp_path(root: &Path, relative: &Path, key: &str) -> PathBuf {
    let mut p = root.join(relative);
    let name = p.file_name().unwrap_or_default().to_string_lossy();
    p.set_file_name(format!(".{name}.{key}.downloading"));
    p
}
pub(super) async fn finish(
    mut v: Receiving,
    root: &Path,
    key: &str,
    policy: i32,
) -> Result<PartialFile> {
    ensure!(
        v.position == v.partial.info.size,
        "收到的文件长度与清单不一致"
    );
    v.file.sync_all().await?;
    let file = v.file.into_std().await;
    if v.partial.info.modified_time != 0
        && let Some(time) =
            UNIX_EPOCH.checked_add(std::time::Duration::from_secs(v.partial.info.modified_time))
    {
        file.set_modified(time)?;
    }
    drop(file);
    let relative = safe_relative(&v.partial.target)?;
    let target = root.join(&relative);
    let source = temp_path(root, &relative, key);
    if target.exists() {
        ensure!(not_link(&target)?.is_file(), "目标已经变成目录或链接");
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let from = source
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let to = target
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        unsafe {
            windows::Win32::Storage::FileSystem::MoveFileExW(
                windows::core::PCWSTR(from.as_ptr()),
                windows::core::PCWSTR(to.as_ptr()),
                windows::Win32::Storage::FileSystem::MOVE_FILE_FLAGS(if policy == 1 {
                    1 | 8
                } else {
                    8
                }),
            )
        }
        .context("无法完成目标文件保存")?;
    }
    #[cfg(not(windows))]
    {
        // policy 1 replaces an existing target; otherwise the name must be free.
        if policy != 1 {
            ensure!(
                !matches!(std::fs::symlink_metadata(&target), Ok(_)),
                "目标文件已存在"
            );
        }
        std::fs::rename(&source, &target).context("无法完成目标文件保存")?;
    }
    v.partial.done = true;
    Ok(v.partial)
}
pub(super) fn cleanup(root: &Path, key: &str, items: &[PartialFile]) -> Result<()> {
    ensure!(uuid::Uuid::parse_str(key).is_ok(), "续传任务身份无效");
    for item in items.iter().filter(|i| !i.done && !i.skipped) {
        let rel = safe_relative(&item.target)?;
        let _locks = parents(root, &rel, false)?;
        let p = temp_path(root, &rel, key);
        match std::fs::symlink_metadata(&p) {
            Ok(m) => {
                ensure!(m.is_file() && !is_link(&m), "临时路径已被替换");
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
            crate::platform::paths::local_app_data()
                .context("本地配置目录不可用")?
                .join("OpenUUYC/file-transfer")
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
        ensure!(r.len() <= 256, "传输记录已满，请清理已完成记录");
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

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn unix_protocol_paths() {
        assert_eq!(local_path("/").unwrap(), PathBuf::from("/"));
        assert_eq!(local_path("/tmp/").unwrap(), PathBuf::from("/tmp"));
        assert!(local_path("relative/path").is_err());
        assert!(local_path("C:\\Users").is_err());
        assert!(local_path("/tmp/../etc").is_err());
        // 对外的路径是反斜杠形状（官方主控按反斜杠拆 parent/name，没有就丢弃整条
        // 下载请求的源路径），回来时换回 POSIX。
        assert_eq!(
            wire_path("/home/clansty/Downloads"),
            "\\home\\clansty\\Downloads"
        );
        assert_eq!(wire_path("/"), "\\");
        // What a Windows host hands out already has this shape.
        for windows in ["C:\\", "D:\\Users\\u\\a b.txt", "\\\\srv\\share\\x"] {
            assert_eq!(wire_path(windows), windows);
        }
        assert_eq!(
            local_path("\\home\\clansty\\Downloads").unwrap(),
            PathBuf::from("/home/clansty/Downloads")
        );
        assert_eq!(local_path("\\").unwrap(), PathBuf::from("/"));
    }
}
