//! One native executable identity behind the optional compressed distribution.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::os::windows::{
    ffi::{OsStrExt, OsStringExt},
    fs::{MetadataExt, OpenOptionsExt},
};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

mod cache;
mod references;
#[cfg(feature = "extract")]
pub use cache::extract;
pub use cache::{Runtime, collect};

/// Product directory name below `%LOCALAPPDATA%`. This crate cannot depend on
/// the application crate, so it mirrors `farwing::product::PRODUCT_NAME`.
pub const PRODUCT: &str = "Farwing";
pub const READY_ENV: &str = "FARWING_BOOT_READY";
pub const ORIGIN_ENV: &str = "FARWING_BOOT_ORIGIN";
/// Launcher handoff events are `Local\Farwing.Boot.<pid>-<uuid>`.
pub const BOOT_EVENT_PREFIX: &str = "Local\\Farwing.Boot.";
pub const IMAGE: &str = "Farwing.exe";
const RECORD: &str = "manifest.json";
const MAX_IMAGE: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub version: String,
    pub architecture: String,
    pub bytes: u64,
    pub sha256: String,
}
impl Manifest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1 && self.architecture == "x86_64",
            "分发包格式不支持"
        );
        ensure!((1024..=MAX_IMAGE).contains(&self.bytes), "分发包长度无效");
        ensure!(
            !self.version.is_empty()
                && self.version.len() <= 80
                && !self.version.chars().any(char::is_control),
            "分发包版本无效"
        );
        ensure!(valid_hash(&self.sha256), "分发包摘要无效");
        Ok(())
    }
}
fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn same_directory(a: &Path, b: &Path) -> bool {
    std::fs::canonicalize(a)
        .ok()
        .zip(std::fs::canonicalize(b).ok())
        .is_some_and(|(a, b)| {
            a.as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
        })
}
pub fn hash(mut file: &File) -> Result<String> {
    let mut digest = Sha256::new();
    let mut bytes = [0u8; 65536];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        digest.update(&bytes[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
pub fn known_folder(id: &windows::core::GUID) -> Result<PathBuf> {
    use windows::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{
            FOLDERID_LocalAppData, KF_FLAG_DEFAULT, KF_FLAG_RETURN_FILTER_REDIRECTION_TARGET,
            SHGetKnownFolderPath,
        },
    };
    let flags = if *id == FOLDERID_LocalAppData {
        KF_FLAG_RETURN_FILTER_REDIRECTION_TARGET
    } else {
        KF_FLAG_DEFAULT
    };
    let raw = unsafe { SHGetKnownFolderPath(id, flags, None)? };
    let text = unsafe { raw.to_string() };
    unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    Ok(PathBuf::from(text?))
}
pub fn root() -> Result<PathBuf> {
    Ok(known_folder(&windows::Win32::UI::Shell::FOLDERID_LocalAppData)?
        .join(PRODUCT)
        .join("runtime"))
}
fn no_reparse(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) => ensure!(
            m.file_attributes() & 0x400 == 0,
            "运行目录不能是重解析点：{}",
            path.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
fn directory(path: &Path) -> Result<File> {
    no_reparse(path)?;
    std::fs::create_dir_all(path)?;
    let f = OpenOptions::new()
        .read(true)
        .share_mode(3)
        .custom_flags(0x02000000 | 0x00200000)
        .open(path)?;
    ensure!(
        f.metadata()?.is_dir() && f.metadata()?.file_attributes() & 0x400 == 0,
        "运行目录无效"
    );
    Ok(f)
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    no_reparse(path)?;
    let mut f = OpenOptions::new().read(true).share_mode(1).open(path)?;
    ensure!(f.metadata()?.len() <= 16384, "运行记录过大");
    let mut bytes = Vec::new();
    Read::by_ref(&mut f).take(16385).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 16384, "运行记录过大");
    Ok(serde_json::from_slice(&bytes)?)
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension("partial");
    no_reparse(path)?;
    no_reparse(&temporary)?;
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= 16384, "运行记录过大");
    if temporary.exists() {
        std::fs::remove_file(&temporary)?;
    }
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(0)
        .open(&temporary)?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    drop(f);
    replace(&temporary, path)
}
fn replace(from: &Path, to: &Path) -> Result<()> {
    use windows::{
        Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        },
        core::PCWSTR,
    };
    let from: Vec<_> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<_> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )?;
    }
    Ok(())
}

/// Holds the verified bytes against writes/rename until CreateProcess has opened
/// the image (and until the native process acquires its own runtime lease).
pub struct Image {
    pub path: PathBuf,
    _file: File,
    _lease: Option<File>,
    _directories: Vec<File>,
}
pub fn verify(path: &Path, manifest: &Manifest) -> Result<Image> {
    manifest.validate()?;
    // Pin the image directory and runtime/application parents across process
    // creation, so verifying bytes cannot race a renamed ancestor directory.
    let mut directories = Vec::new();
    for parent in path.ancestors().skip(1).take(3) {
        ensure!(parent.is_dir(), "运行目录不存在");
        directories.push(directory(parent)?);
    }
    no_reparse(path)?;
    let f = OpenOptions::new().read(true).share_mode(1).open(path)?;
    ensure!(
        f.metadata()?.is_file()
            && f.metadata()?.len() == manifest.bytes
            && hash(&f)? == manifest.sha256,
        "运行文件校验失败"
    );
    Ok(Image {
        path: dunce::canonicalize(path)?,
        _file: f,
        _lease: None,
        _directories: directories,
    })
}

struct Handle(windows::Win32::Foundation::HANDLE);
unsafe impl Send for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
pub struct Ready(Handle);
impl Ready {
    /// Consume launch context before any application threads are started.
    pub fn from_environment() -> Result<Option<Self>> {
        let value = std::env::var_os(READY_ENV);
        // SAFETY: called only at the very beginning of native main.
        unsafe {
            std::env::remove_var(READY_ENV);
        }
        let Some(value) = value else { return Ok(None) };
        let name = value.to_str().context("启动交接标识无效")?;
        let suffix = name
            .strip_prefix(BOOT_EVENT_PREFIX)
            .context("启动交接标识无效")?;
        ensure!(
            !suffix.is_empty()
                && suffix.len() <= 100
                && suffix.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-'),
            "启动交接标识无效"
        );
        let wide: Vec<_> = value.encode_wide().chain(Some(0)).collect();
        let handle = unsafe {
            windows::Win32::System::Threading::OpenEventW(
                windows::Win32::System::Threading::EVENT_MODIFY_STATE,
                false,
                windows::core::PCWSTR(wide.as_ptr()),
            )?
        };
        Ok(Some(Self(Handle(handle))))
    }
    pub fn signal(self) -> Result<()> {
        unsafe {
            windows::Win32::System::Threading::SetEvent(self.0.0)?;
        }
        Ok(())
    }
}
