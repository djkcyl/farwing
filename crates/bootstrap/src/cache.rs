use super::*;
const OWNER: &str = "Farwing native runtime cache v1\n";
const PARTIAL: &str = "Farwing.partial";
struct Cache {
    root: PathBuf,
    _parents: Vec<File>,
    _lock: File,
}
impl Cache {
    fn open(root: &Path, wait: bool) -> Result<Self> {
        let parent = root.parent().context("运行目录无效")?;
        let parents = vec![directory(parent)?, directory(root)?];
        let path = root.join("cache.lock");
        no_reparse(&path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(3)
            .open(path)?;
        let start = Instant::now();
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock)
                    if wait && start.elapsed() < Duration::from_secs(30) =>
                {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(error) => return Err(anyhow::anyhow!("运行目录正被占用：{error}")),
            }
        }
        let marker = root.join("owner.txt");
        no_reparse(&marker)?;
        if marker.exists() {
            ensure!(
                std::fs::metadata(&marker)?.len() == OWNER.len() as u64,
                "运行目录归属无效"
            );
            ensure!(
                std::fs::read_to_string(&marker)? == OWNER,
                "运行目录归属无效"
            );
        } else {
            ensure!(
                std::fs::read_dir(root)?.all(|e| e.is_ok_and(
                    |e| e.file_name() == "cache.lock" || e.file_name() == "owner.partial"
                )),
                "运行目录存在未登记文件"
            );
            let temporary = root.join("owner.partial");
            no_reparse(&temporary)?;
            if temporary.exists() {
                std::fs::remove_file(&temporary)?;
            }
            let mut owner = OpenOptions::new()
                .write(true)
                .create_new(true)
                .share_mode(0)
                .open(&temporary)?;
            owner.write_all(OWNER.as_bytes())?;
            owner.sync_all()?;
            drop(owner);
            replace(&temporary, &marker)?;
        }
        Ok(Self {
            root: root.into(),
            _parents: parents,
            _lock: lock,
        })
    }
    fn entries(&self) -> Result<Vec<PathBuf>> {
        let mut entries = Vec::new();
        for (index, entry) in std::fs::read_dir(&self.root)?.take(257).enumerate() {
            ensure!(index < 256, "运行目录条目过多，已保留以供检查");
            let entry = entry?;
            if valid_hash(&entry.file_name().to_string_lossy()) {
                entries.push(entry.path());
            }
            ensure!(entries.len() <= 128, "运行版本过多，已保留以供检查");
        }
        Ok(entries)
    }
}
fn lease(path: &Path) -> Result<File> {
    no_reparse(path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(3)
        .open(path)?;
    file.try_lock_shared()
        .map_err(|e| anyhow::anyhow!("运行版本正在维护：{e}"))?;
    Ok(file)
}

#[cfg(feature = "extract")]
pub fn extract(manifest: &Manifest, payload: &[u8]) -> Result<Image> {
    extract_at(&root()?, manifest, payload)
}
#[cfg(feature = "extract")]
fn extract_at(root: &Path, manifest: &Manifest, payload: &[u8]) -> Result<Image> {
    manifest.validate()?;
    let cache = Cache::open(root, true)?;
    let folder = cache.root.join(&manifest.sha256);
    let _directory = directory(&folder)?;
    let path = folder.join(IMAGE);
    let use_path = folder.join("in-use");
    let mut image = match verify(&path, manifest) {
        Ok(image) => image,
        Err(_) => {
            // Exclusive lease also protects a currently running damaged image.
            no_reparse(&use_path)?;
            let exclusive = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .share_mode(3)
                .open(&use_path)?;
            exclusive
                .try_lock()
                .map_err(|e| anyhow::anyhow!("运行文件被占用，请先退出该版本：{e}"))?;
            write_json(&folder.join(RECORD), manifest)?;
            let partial = folder.join(PARTIAL);
            no_reparse(&partial)?;
            if partial.exists() {
                std::fs::remove_file(&partial)?;
            }
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .share_mode(0)
                .open(&partial)?;
            let mut decoder = zstd::stream::read::Decoder::new(payload)?.take(manifest.bytes + 1);
            let written = std::io::copy(&mut decoder, &mut output)?;
            output.sync_all()?;
            drop(output);
            ensure!(written == manifest.bytes, "解包长度不匹配");
            drop(verify(&partial, manifest)?);
            replace(&partial, &path)?;
            write_json(&folder.join(RECORD), manifest)?;
            drop(exclusive);
            verify(&path, manifest)?
        }
    };
    // Repair missing metadata only from the embedded, verified manifest.
    write_json(&folder.join(RECORD), manifest)?;
    image._lease = Some(lease(&use_path)?);
    Ok(image)
}

pub struct Runtime {
    folder: PathBuf,
    pub origin: Option<PathBuf>,
    _image: Image,
}
impl Runtime {
    pub fn enter() -> Result<Option<Self>> {
        let image = std::env::current_exe()?;
        Self::enter_at(&root()?, &image)
    }
    fn enter_at(root: &Path, image: &Path) -> Result<Option<Self>> {
        let Some(folder) = image.parent() else {
            return Ok(None);
        };
        if !image
            .file_name()
            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(IMAGE))
            || !folder
                .file_name()
                .is_some_and(|n| valid_hash(&n.to_string_lossy().to_ascii_lowercase()))
        {
            return Ok(None);
        }
        let actual_root = folder.parent().context("运行目录缺失")?;
        if !same_directory(actual_root, root) {
            let physical = dunce::canonicalize(actual_root)?;
            let profile =
                dunce::canonicalize(known_folder(&windows::Win32::UI::Shell::FOLDERID_Profile)?)?;
            ensure!(
                physical.starts_with(profile)
                    && actual_root
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("runtime"))
                    && actual_root
                        .parent()
                        .and_then(Path::file_name)
                        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(PRODUCT)),
                "运行目录不属于当前用户"
            );
        }
        let _cache = Cache::open(actual_root, true)?;
        let _directory = directory(folder)?;
        let manifest: Manifest = read_json(&folder.join(RECORD))?;
        ensure!(
            folder
                .file_name()
                .unwrap()
                .to_string_lossy()
                .eq_ignore_ascii_case(&manifest.sha256),
            "运行版本目录不匹配"
        );
        let mut image = verify(image, &manifest)?;
        image._lease = Some(lease(&folder.join("in-use"))?);
        let origin = std::env::var_os(ORIGIN_ENV).map(PathBuf::from).or_else(|| {
            read_json::<Vec<u16>>(&folder.join("origin.json"))
                .ok()
                .filter(|v| v.len() <= 32767)
                .map(|v| PathBuf::from(std::ffi::OsString::from_wide(&v)))
        });
        Ok(Some(Self {
            folder: folder.into(),
            origin,
            _image: image,
        }))
    }
    pub fn collect(&self) -> Result<()> {
        collect_at(self.folder.parent().unwrap())
    }
    pub fn ready(&self) -> Result<()> {
        let _cache = Cache::open(self.folder.parent().unwrap(), true)?;
        if let Some(origin) = &self.origin {
            let context: Vec<u16> = origin.as_os_str().encode_wide().collect();
            write_json(&self.folder.join("origin.json"), &context)?;
        }
        no_reparse(&self.folder.join("ready"))?;
        write_json(&self.folder.join("ready"), &1)?;
        Ok(())
    }
}

/// Best-effort, bounded maintenance away from streaming/input execution.
pub fn collect() -> Result<()> {
    collect_at(&root()?)
}
fn collect_at(root: &Path) -> Result<()> {
    let registered = super::references::live()?;
    collect_referenced(root, &registered)
}
fn collect_referenced(root: &Path, registered: &[String]) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let cache = Cache::open(root, false)?;
    let mut entries = Vec::new();
    for path in cache.entries()? {
        if no_reparse(&path).is_err() {
            continue;
        }
        let Ok(m) = read_json::<Manifest>(&path.join(RECORD)) else {
            continue;
        };
        if m.validate().is_err() || path.file_name() != Some(std::ffi::OsStr::new(&m.sha256)) {
            continue;
        }
        let when = std::fs::metadata(path.join("ready"))
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        entries.push((when, path, m));
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, folder, m) in entries.into_iter().skip(2) {
        if registered.iter().any(|v| v.contains(&m.sha256)) {
            continue;
        }
        let _directory = directory(&folder)?;
        let use_path = folder.join("in-use");
        no_reparse(&use_path)?;
        let use_file = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(3)
            .open(&use_path)?;
        if use_file.try_lock().is_err() {
            continue;
        }
        let allowed = [
            IMAGE,
            RECORD,
            "origin.json",
            "ready",
            PARTIAL,
            "in-use",
            "manifest.partial",
            "origin.partial",
            "ready.partial",
        ];
        let files = std::fs::read_dir(&folder)?.collect::<std::io::Result<Vec<_>>>()?;
        if files.iter().any(|e| {
            !allowed.iter().any(|n| e.file_name() == *n)
                || no_reparse(&e.path()).is_err()
                || !e.file_type().is_ok_and(|t| t.is_file())
        }) {
            continue;
        }
        for file in files.iter().filter(|e| e.file_name() != "in-use") {
            std::fs::remove_file(file.path())?;
        }
        drop(use_file);
        std::fs::remove_file(use_path)?;
        drop(_directory);
        std::fs::remove_dir(folder)?;
    }
    Ok(())
}
