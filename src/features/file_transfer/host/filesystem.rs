use super::super::service::PartialFile;
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

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
        ensure!(!scope.is_empty() && scope.len() <= 1024, "文件会话身份无效");
        let directory = crate::platform::paths::local_app_data()
            .context("用户目录不可用")?
            .join("OpenUUYC/file-receive")
            .join(format!("{:x}", Sha256::digest(scope)));
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
                    ensure!(bytes.len() <= 32 * 1024 * 1024, "续传记录过大");
                    let record: Record = serde_json::from_slice(&bytes)?;
                    ensure!(
                        store.path(&record.remote_key)? == entry.path(),
                        "续传记录身份不符"
                    );
                    store.cache_temporary(&record)
                })();
                if let Err(error) = restore {
                    tracing::debug!(%error,"file listing resume metadata unavailable");
                }
            }
        }
        Ok(store)
    }
    fn path(&self, key: &str) -> Result<PathBuf> {
        ensure!(
            !key.is_empty() && key.len() <= 1024 && !key.contains('\0'),
            "续传标识无效"
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
        ensure!(bytes.len() <= 32 * 1024 * 1024, "续传记录过大");
        let record: Record = serde_json::from_slice(&bytes)?;
        ensure!(
            record.remote_key == key && uuid::Uuid::parse_str(&record.key).is_ok(),
            "续传身份不符"
        );
        storage::validate_manifest(&record.files)?;
        let root = local_path(&record.root.to_string_lossy())?;
        ensure!(
            root == record.root && record.partial.len() <= storage::MAX_FILES,
            "续传记录路径无效"
        );
        for p in &record.partial {
            storage::safe_relative(&p.target)?;
            storage::safe_relative(&p.info.rel_path)?;
        }
        Ok(Some(record))
    }
    pub fn save(&self, r: &Record) -> Result<()> {
        let bytes = serde_json::to_vec(r)?;
        ensure!(bytes.len() <= 32 * 1024 * 1024, "续传记录过大");
        std::fs::create_dir_all(&self.directory)?;
        let final_path = self.path(&r.remote_key)?;
        if !final_path.exists() {
            ensure!(
                std::fs::read_dir(&self.directory)?.take(257).count() < 256,
                "未完成的接收任务过多，请先清理旧任务"
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
        ensure!(r.root.is_absolute(), "续传根目录无效");
        let mut entries = Vec::new();
        for p in r.partial.iter().filter(|p| !p.done && !p.skipped) {
            ensure!(entries.len() < 1, "接收记录同时打开了多个文件");
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
    pub fn clear(&self, key: &str) -> Result<()> {
        if let Some(r) = self.load(key)? {
            storage::cleanup(&r.root, &r.key, &r.partial)?;
            std::fs::remove_file(self.path(key)?)?;
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
/// The identity of a location in the busy set: Windows paths compare
/// case-insensitively with `\\` separators, Unix paths exactly.
fn busy_key(path: &Path) -> String {
    #[cfg(windows)]
    {
        path.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    }
    #[cfg(not(windows))]
    {
        let key = path.to_string_lossy();
        let trimmed = key.trim_end_matches('/');
        if trimmed.is_empty() { "/" } else { trimmed }.to_owned()
    }
}
/// Whether `key` is `parent` or lies underneath it.
fn within(key: &str, parent: &str) -> bool {
    let separator = std::path::MAIN_SEPARATOR;
    key == parent
        || key.starts_with(&format!(
            "{}{separator}",
            parent.trim_end_matches(separator)
        ))
}
fn parent_locks(path: &Path) -> Result<Vec<File>> {
    let parent = path.parent().context("不能修改磁盘根目录")?;
    #[cfg(windows)]
    let root = PathBuf::from(format!(
        "{}:\\",
        path.to_string_lossy().as_bytes()[0] as char
    ));
    #[cfg(not(windows))]
    let root = PathBuf::from("/");
    let rel = path.strip_prefix(&root)?;
    ensure!(
        parent != path && rel.components().count() > 0,
        "不能修改磁盘根目录"
    );
    storage::parents(&root, rel, false)
}
pub(super) fn receiving_root(destination: &Path, folder: &str, policy: i32) -> Result<PathBuf> {
    if folder.is_empty() {
        return storage::canonical_dir(destination);
    }
    let rel = storage::safe_relative(folder)?;
    ensure!(rel.components().count() == 1, "接收文件夹名称无效");
    let _locks = storage::parents(destination, &rel, false)?;
    let mut path = destination.join(&rel);
    if policy == 2 {
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
        anyhow::bail!("无法分配接收文件夹")
    }
    if !path.exists() {
        std::fs::create_dir(&path)?;
    }
    storage::canonical_dir(&path)
}
pub(super) struct Reservation {
    store: Arc<Store>,
    key: String,
}
impl Reservation {
    pub fn acquire(store: Arc<Store>, root: &Path) -> Result<Self> {
        let key = busy_key(root);
        let mut active = super::super::lock(&store.busy);
        ensure!(
            !active.iter().any(|p| within(&key, p) || within(p, &key)),
            "另一个文件操作正在使用该位置"
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

pub(super) fn operation(
    message: Incoming,
    store: &Arc<Store>,
    stop: &CancellationToken,
    capabilities: Capabilities,
) -> Result<Option<Packet>> {
    let Payload::Request(req) = message.payload else {
        return Ok(None);
    };
    let result = (|| -> Result<Option<Res>> {
        ensure!(!stop.is_cancelled(), "文件操作已撤销");
        Ok(Some(match &req {
            Req::ReadDir(v) => {
                let path = if v.path == ":/" {
                    v.path.clone()
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
                // 对端（官方主控）要求拿到手的路径带反斜杠，见 `storage::wire_path`。
                for entry in &mut entries {
                    entry.full_path = storage::wire_path(&entry.full_path);
                }
                let (entries, dir_data) = if capabilities.compressed {
                    (Vec::new(), storage::compress(&DirectoryData { entries })?)
                } else {
                    (entries, Vec::new())
                };
                Res::FileDirectory(FileDirectory {
                    id: v.id.clone(),
                    path: reply_directory(&v.path, &path),
                    entries,
                    dir_data,
                    file_error: 1,
                    ..Default::default()
                })
            }
            Req::FileExist(v) => {
                ensure!(v.names.len() <= storage::MAX_FILES, "同名检查项目过多");
                let root = directory(&v.path)?;
                let mut results = Vec::new();
                for name in &v.names {
                    let rel = storage::safe_relative(name)?;
                    ensure!(rel.components().count() == 1, "同名检查名称无效");
                    let p = root.join(rel);
                    let key = busy_key(&p);
                    let busy = super::super::lock(&store.busy)
                        .iter()
                        .any(|s| within(&key, s));
                    results.push(FileExistResult {
                        name: name.clone(),
                        has_same: p.try_exists()?,
                        has_transfering: busy,
                    });
                }
                Res::FileExistResponse(FileExistResponse {
                    path: v.path.clone(),
                    results,
                })
            }
            Req::DirCreate(v) => {
                let parent = directory(&v.path)?;
                let _lock = Reservation::acquire(store.clone(), &parent)?;
                let mut made = None;
                for n in 0..10000 {
                    ensure!(!stop.is_cancelled(), "文件操作已撤销");
                    let name = if n == 0 {
                        "新建文件夹".to_owned()
                    } else {
                        format!("新建文件夹 ({n})")
                    };
                    let p = parent.join(&name);
                    let _parents = parent_locks(&p)?;
                    ensure!(!stop.is_cancelled(), "文件操作已撤销");
                    match std::fs::create_dir(&p) {
                        Ok(()) => {
                            made = Some(p);
                            break;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                Res::OperationResult(FileOperationResult {
                    file_error: 1,
                    path: storage::wire_path(
                        &made.context("无法分配文件夹名称")?.to_string_lossy(),
                    ),
                    err_msg: String::new(),
                })
            }
            Req::Rename(v) => {
                let from = local_path(&v.path)?;
                let to = local_path(&v.new_name)?;
                ensure!(from.parent() == to.parent(), "重命名不能移动到其他目录");
                let _lock = Reservation::acquire(
                    store.clone(),
                    from.parent().context("不能重命名根目录")?,
                )?;
                let _parents = parent_locks(&from)?;
                ensure!(!to.try_exists()?, "目标名称已经存在");
                ensure!(!stop.is_cancelled(), "文件操作已撤销");
                std::fs::rename(&from, &to)?;
                Res::OperationResult(FileOperationResult {
                    file_error: 1,
                    path: storage::wire_path(&to.to_string_lossy()),
                    err_msg: String::new(),
                })
            }
            Req::RemoveFile(v) => {
                let p = local_path(&v.path)?;
                let _lock = Reservation::acquire(store.clone(), &p)?;
                let _parents = parent_locks(&p)?;
                ensure!(std::fs::symlink_metadata(&p)?.is_file(), "指定路径不是文件");
                ensure!(!stop.is_cancelled(), "文件操作已撤销");
                std::fs::remove_file(&p)?;
                Res::OperationResult(FileOperationResult {
                    file_error: 1,
                    path: v.path.clone(),
                    err_msg: String::new(),
                })
            }
            Req::RemoveDir(v) => {
                let p = local_path(&v.path)?;
                let _lock = Reservation::acquire(store.clone(), &p)?;
                let _parents = parent_locks(&p)?;
                if v.recursive {
                    remove_tree(&p, stop, 0)?;
                } else {
                    ensure!(!stop.is_cancelled(), "文件操作已撤销");
                    std::fs::remove_dir(&p)?;
                }
                Res::Result(FileTransferResult {
                    id: v.id.clone(),
                    file_error: 1,
                    err_msg: String::new(),
                })
            }
            Req::ClearSendTemp(v) => {
                let _lock = Reservation::acquire(store.clone(), &store.directory)?;
                ensure!(!stop.is_cancelled(), "文件操作已撤销");
                store.clear(&v.task_unique_id)?;
                return Ok(None);
            }
            _ => anyhow::bail!("文件请求不属于管理操作"),
        }))
    })();
    match result {
        Ok(value) => {
            let packet = value.map(|v| response(message.header, v));
            if packet
                .as_ref()
                .is_some_and(|p| p.data.len() >= PAYLOAD_LIMIT)
            {
                reject(&request(message.header, req, false).data, 6)
            } else {
                Ok(packet)
            }
        }
        Err(e) => {
            tracing::warn!(request=?message.header, error=%e, "host file management failed");
            let code = failure(&e);
            let bytes = request(message.header, req, false).data;
            reject(&bytes, code)
        }
    }
}
/// The directory a `FileDirectory` reply names. The official directory producer
/// echoes the requested directory; only its virtual default-directory alias
/// resolves to a real path, which leaves in wire shape like every other path we
/// hand out (see `storage::wire_path`), or the official controller would browse,
/// go up and upload from a POSIX path it cannot split.
fn reply_directory(requested: &str, resolved: &str) -> String {
    if requested == ":/Default" {
        storage::wire_path(resolved)
    } else {
        requested.to_owned()
    }
}
fn remove_tree(path: &Path, stop: &CancellationToken, depth: usize) -> Result<()> {
    ensure!(
        !stop.is_cancelled() && depth < 128,
        "目录删除已取消或层级过深"
    );
    let m = std::fs::symlink_metadata(path)?;
    ensure!(m.is_dir() && !storage::is_link(&m), "删除目标不是普通目录");
    let held = storage::parents(path, Path::new(".openuuyc-directory-lock"), false)?;
    for child in std::fs::read_dir(path)? {
        let p = child?.path();
        let m = std::fs::symlink_metadata(&p)?;
        ensure!(!storage::is_link(&m), "不能递归删除重解析点或符号链接");
        if m.is_dir() {
            remove_tree(&p, stop, depth + 1)?
        } else {
            ensure!(!stop.is_cancelled(), "目录删除已取消");
            std::fs::remove_file(p)?;
        }
    }
    drop(held);
    ensure!(!stop.is_cancelled(), "目录删除已取消");
    std::fs::remove_dir(path)?;
    Ok(())
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn busy_locations_nest_by_component() {
        assert_eq!(busy_key(Path::new("/home/a/")), "/home/a");
        assert_eq!(busy_key(Path::new("/")), "/");
        assert!(within("/home/a/b", "/home/a"));
        assert!(within("/home/a", "/home/a"));
        assert!(!within("/home/ab", "/home/a"));
        assert!(within("/home", "/"));
        // Linux names are case-sensitive.
        assert!(!within("/home/A", "/home/a"));
    }

    #[test]
    fn default_directory_replies_in_wire_shape() {
        assert_eq!(
            reply_directory(":/Default", "/home/u/Downloads"),
            "\\home\\u\\Downloads"
        );
        // Any other request is echoed as the peer sent it.
        assert_eq!(reply_directory("\\home\\u", "/home/u"), "\\home\\u");
        assert_eq!(reply_directory("/home/u", "/home/u"), "/home/u");
        // A Windows host's resolved download folder is already in wire shape.
        assert_eq!(
            reply_directory(":/Default", "C:\\Users\\u\\Downloads"),
            "C:\\Users\\u\\Downloads"
        );
    }
}

/// `storage::wire_path` is applied to every path a host hands out. On a Windows
/// host those paths come from `dunce::canonicalize`, `read_dir`, the shell's known
/// folders, drive roots and `local_path`, which joins component by component, so
/// they hold no `/` and the replies are exactly what they were before.
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    fn unchanged(path: &str) {
        assert_eq!(storage::wire_path(path), path, "{path}");
    }

    #[test]
    fn host_replies_keep_their_shape() {
        for entry in super::super::super::service::local_list(":/").unwrap() {
            unchanged(&entry.full_path);
        }
        if let Ok(downloads) = directory(":/Default") {
            let downloads = downloads.to_string_lossy();
            assert_eq!(reply_directory(":/Default", &downloads), downloads);
        }

        let root = storage::canonical_dir(&std::env::temp_dir()).unwrap();
        let dir = root.join(format!("openuuyc-wire-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a b.txt"), b"x").unwrap();
        // A peer may send either separator; what we resolve and list is backslashed.
        let sent = dir.to_str().unwrap().replace('\\', "/");
        let resolved = directory(&sent).unwrap();
        unchanged(&resolved.to_string_lossy());
        let entries =
            super::super::super::service::local_list(&resolved.to_string_lossy()).unwrap();
        assert_eq!(entries.len(), 2);
        for entry in entries {
            unchanged(&entry.full_path);
        }
        // DirCreate and Rename reply with a path built from `directory`/`local_path`.
        unchanged(&resolved.join("新建文件夹").to_string_lossy());
        unchanged(
            &local_path(&format!("{sent}/sub/renamed"))
                .unwrap()
                .to_string_lossy(),
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
