//! Device facts for the viewer role. Only the identity fields the
//! registration check needs are filled; the desktop facts (GPU, memory,
//! wallpaper) stay empty until a macOS host role exists.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Hardware {
    pub name: String,
    pub system_uuid: String,
    pub machine_guid: String,
    pub os: String,
    pub base_board: String,
    pub cpu: String,
    pub video: Vec<String>,
    pub mac: String,
    pub memory: i64,
    pub screen: String,
    pub errors: Vec<String>,
}

pub(crate) fn computer_name() -> Result<String> {
    let mut buffer = [0u8; 256];
    // macOS's gethostname takes char* (signed) where Linux's takes void*.
    let ok = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0;
    ensure!(ok, "系统未提供主机名");
    let name = buffer
        .split(|b| *b == 0)
        .next()
        .unwrap_or_default();
    let name = String::from_utf8_lossy(name).trim().to_owned();
    ensure!(!name.is_empty(), "系统未提供主机名");
    Ok(name)
}

/// The machine identity `IOPlatformUUID` reports, via `ioreg`.
fn machine_id() -> Result<String> {
    let output = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .context("读取 IOPlatformUUID 失败")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .find(|line| line.contains("IOPlatformUUID"))
        .context("系统未提供机器标识")?;
    let id = line
        .split('"')
        .nth(3)
        .unwrap_or_default()
        .trim()
        .to_owned();
    ensure!(!id.is_empty(), "机器标识为空");
    Ok(id)
}

impl Hardware {
    pub fn read() -> Result<Self> {
        let machine = machine_id()?;
        Ok(Self {
            name: computer_name()?,
            system_uuid: machine.to_uppercase(),
            machine_guid: machine,
            os: "macOS".into(),
            ..Self::default()
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct WallpaperSource {
    pub path: std::path::PathBuf,
    pub modified: std::time::SystemTime,
    pub length: u64,
}
impl WallpaperSource {
    pub(crate) fn cached(&self) -> bool {
        false
    }
}

pub(crate) fn wallpaper_source() -> Result<WallpaperSource> {
    anyhow::bail!("macOS host role is not supported")
}
pub(crate) fn wallpaper_image(_source: &WallpaperSource) -> Result<Vec<u8>> {
    anyhow::bail!("macOS host role is not supported")
}
