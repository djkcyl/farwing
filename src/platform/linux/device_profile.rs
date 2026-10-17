//! Linux device facts and the signed-in user's static desktop wallpaper, in
//! the shape the Windows profile reports.
//!
//! Every fact comes from files an unprivileged desktop user can read. The
//! SMBIOS UUID (`/sys/class/dmi/id/product_uuid`) is readable only by root, so
//! the system UUID is derived from the installation's machine ID instead; it
//! then stays the same whoever runs the client, which the registration check
//! requires.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{io::Read, path::PathBuf, process::Command, time::Duration};

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

/// Namespace of the system UUID derived from the machine ID.
const SYSTEM_NAMESPACE: uuid::Uuid = uuid::Uuid::from_u128(0x3f0e6f7c_8a5b_4d2e_9c61_0b7e4a2d9f15);

fn read_trimmed(path: &str) -> Result<String> {
    Ok(std::fs::read_to_string(path)
        .with_context(|| format!("读取 {path}"))?
        .trim()
        .to_owned())
}

/// The installation identity, `/etc/machine-id` as a hyphenated UUID.
fn machine_id() -> Result<uuid::Uuid> {
    let text = read_trimmed("/etc/machine-id")
        .or_else(|_| read_trimmed("/var/lib/dbus/machine-id"))
        .context("系统未提供机器标识")?;
    let id = uuid::Uuid::try_parse(&text).context("机器标识格式无效")?;
    ensure!(!id.is_nil(), "机器标识为空");
    Ok(id)
}

pub(crate) fn computer_name() -> Result<String> {
    let name = read_trimmed("/proc/sys/kernel/hostname")?;
    ensure!(!name.is_empty(), "系统未提供主机名");
    Ok(name)
}

impl Hardware {
    pub fn read() -> Result<Self> {
        let machine = machine_id()?;
        let mut value = Self {
            name: computer_name()?,
            system_uuid: uuid::Uuid::new_v5(&SYSTEM_NAMESPACE, machine.as_bytes())
                .to_string()
                .to_uppercase(),
            machine_guid: machine.to_string(),
            ..Self::default()
        };
        let mut field = |label: &str, result: Result<String>| match result {
            Ok(value) if !value.is_empty() => value,
            Ok(_) => {
                value.errors.push(format!("{label}：系统未提供"));
                String::new()
            }
            Err(e) => {
                value.errors.push(format!("{label}：{e:#}"));
                String::new()
            }
        };
        value.cpu = field("处理器", proc_field("/proc/cpuinfo", "model name"));
        value.base_board = field(
            "主板",
            (|| {
                Ok(format!(
                    "Manufacturer: {}  Product: {}",
                    read_trimmed("/sys/class/dmi/id/board_vendor")?,
                    read_trimmed("/sys/class/dmi/id/board_name")?
                ))
            })(),
        );
        value.os = field("操作系统", os());
        value.mac = field("网卡地址", primary_mac());
        match graphics() {
            Ok(names) => value.video = names,
            Err(e) => value.errors.push(format!("显卡：{e:#}")),
        }
        match proc_field("/proc/meminfo", "MemTotal").and_then(|total| {
            let kib = total
                .split_whitespace()
                .next()
                .context("内存大小缺失")?
                .parse::<i64>()?;
            Ok(kib / 1024)
        }) {
            Ok(memory) => value.memory = memory,
            Err(e) => value.errors.push(format!("内存：{e:#}")),
        }
        value.screen = primary_screen().unwrap_or_else(|| "0x0".into());
        Ok(value)
    }
}

fn proc_field(path: &str, key: &str) -> Result<String> {
    let text = std::fs::read_to_string(path).with_context(|| format!("读取 {path}"))?;
    Ok(text
        .lines()
        .find_map(|line| line.split_once(':').filter(|(name, _)| name.trim() == key))
        .map(|(_, value)| value.trim().to_owned())
        .unwrap_or_default())
}

fn os() -> Result<String> {
    let release = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .context("读取 os-release")?;
    let name = release
        .lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|value| value.trim().trim_matches('"').to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Linux".into());
    let kernel = read_trimmed("/proc/sys/kernel/osrelease")?;
    let bits = if cfg!(target_pointer_width = "64") {
        "64-bit"
    } else {
        "32-bit"
    };
    Ok(format!("{name} {bits} · Linux {kernel}"))
}

/// Display controllers on the PCI bus (class 0x03), named from `pci.ids`
/// the way `lspci` names them; each model once, like the Windows list.
fn graphics() -> Result<Vec<String>> {
    let mut names = Vec::new();
    let database = ["/usr/share/misc/pci.ids", "/usr/share/hwdata/pci.ids"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok());
    for entry in std::fs::read_dir("/sys/bus/pci/devices").context("读取 PCI 设备")? {
        let path = entry?.path();
        let read = |name: &str| read_trimmed(&path.join(name).to_string_lossy());
        if !read("class").is_ok_and(|class| class.starts_with("0x03")) {
            continue;
        }
        let hex = |name: &str| -> Result<u16> {
            let text = read(name)?;
            Ok(u16::from_str_radix(text.trim_start_matches("0x"), 16)?)
        };
        let (vendor, device) = (hex("vendor")?, hex("device")?);
        let name = database
            .as_deref()
            .and_then(|database| pci_name(database, vendor, device))
            .unwrap_or_else(|| format!("PCI {vendor:04x}:{device:04x}"));
        if !names.contains(&name) {
            names.push(name);
        }
    }
    Ok(names)
}

/// The `pci.ids` name of the PCI device at a sysfs device directory.
pub(super) fn pci_device_name(device: &std::path::Path) -> Option<String> {
    let hex = |name: &str| -> Option<u16> {
        let text = read_trimmed(&device.join(name).to_string_lossy()).ok()?;
        u16::from_str_radix(text.trim_start_matches("0x"), 16).ok()
    };
    let (vendor, id) = (hex("vendor")?, hex("device")?);
    ["/usr/share/misc/pci.ids", "/usr/share/hwdata/pci.ids"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .and_then(|database| pci_name(&database, vendor, id))
}

/// "NVIDIA RTX A6000" from "NVIDIA Corporation" and "GA102GL [RTX A6000]":
/// the bracketed marketing name where `pci.ids` has one.
fn pci_name(database: &str, vendor: u16, device: u16) -> Option<String> {
    let short = |name: &str| -> String {
        match (name.find('['), name.rfind(']')) {
            (Some(start), Some(end)) if start < end => name[start + 1..end].to_owned(),
            _ => name.to_owned(),
        }
    };
    let vendor_prefix = format!("{vendor:04x}  ");
    let device_prefix = format!("\t{device:04x}  ");
    let mut lines = database.lines();
    let vendor_name = lines.find_map(|line| line.strip_prefix(&vendor_prefix))?;
    let vendor_name = if vendor_name.contains('[') {
        short(vendor_name)
    } else {
        vendor_name
            .split_whitespace()
            .next()
            .unwrap_or(vendor_name)
            .to_owned()
    };
    let device_name = lines
        .take_while(|line| line.starts_with('\t') || line.starts_with('#') || line.is_empty())
        .find_map(|line| line.strip_prefix(&device_prefix))?;
    Some(format!("{vendor_name} {}", short(device_name)))
}

/// The physical Ethernet or Wi-Fi adapter carrying the default route with the
/// lowest metric, as Windows picks the gateway adapter with the lowest metric.
fn primary_mac() -> Result<String> {
    let routes = std::fs::read_to_string("/proc/net/route").context("读取路由表")?;
    let mut candidates = Vec::new();
    for line in routes.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [interface, destination, _gateway, flags, _, _, metric, ..] = fields[..] else {
            continue;
        };
        let flags = u32::from_str_radix(flags, 16).unwrap_or(0);
        // A default route that is up and goes through a gateway.
        if destination != "00000000" || flags & 0x3 != 0x3 {
            continue;
        }
        let device = PathBuf::from("/sys/class/net").join(interface);
        let physical = device.join("device").exists()
            && read_trimmed(&device.join("type").to_string_lossy()).is_ok_and(|t| t == "1")
            && read_trimmed(&device.join("operstate").to_string_lossy()).is_ok_and(|s| s == "up");
        if !physical {
            continue;
        }
        let address = read_trimmed(&device.join("address").to_string_lossy())?;
        candidates.push((metric.parse::<u32>().unwrap_or(u32::MAX), address));
    }
    candidates.sort();
    Ok(candidates
        .into_iter()
        .next()
        .map(|(_, mac)| mac.to_uppercase())
        .unwrap_or_default())
}

fn primary_screen() -> Option<String> {
    let monitors = super::display::topology::Topology::query(true)
        .ok()?
        .monitors();
    let monitor = monitors
        .iter()
        .find(|monitor| monitor.primary)
        .or_else(|| monitors.first())?;
    Some(format!("{}x{}", monitor.width, monitor.height))
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct WallpaperSource {
    pub path: PathBuf,
    pub modified: std::time::SystemTime,
    pub length: u64,
}
impl WallpaperSource {
    /// Windows may read a cached copy of the configured picture; Linux always
    /// reads the configured file itself.
    pub(crate) fn cached(&self) -> bool {
        false
    }
}

/// The static picture the desktop shows now: GNOME, Cinnamon and MATE keep it
/// in GSettings, KDE Plasma in its applet configuration.
pub(crate) fn wallpaper_source() -> Result<WallpaperSource> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_uppercase();
    let path = if desktop.contains("KDE") {
        plasma_wallpaper()?
    } else if desktop.contains("CINNAMON") || desktop.contains("X-CINNAMON") {
        gsettings_uri("org.cinnamon.desktop.background", "picture-uri")?
    } else if desktop.contains("MATE") {
        PathBuf::from(gsettings("org.mate.background", "picture-filename")?)
    } else if desktop.contains("GNOME") || desktop.contains("UNITY") || desktop.contains("BUDGIE") {
        let dark = gsettings("org.gnome.desktop.interface", "color-scheme")
            .is_ok_and(|scheme| scheme == "prefer-dark");
        gsettings_uri(
            "org.gnome.desktop.background",
            if dark {
                "picture-uri-dark"
            } else {
                "picture-uri"
            },
        )?
    } else {
        bail!("当前桌面环境的壁纸读取尚未支持");
    };
    ensure!(!path.as_os_str().is_empty(), "桌面当前没有静态图片壁纸");
    ensure!(path.is_absolute(), "壁纸必须是本机文件");
    let metadata = std::fs::metadata(&path).context("读取当前桌面壁纸")?;
    ensure!(
        metadata.is_file() && metadata.len() <= crate::platform::wallpaper::MAXIMUM_FILE,
        "壁纸文件过大或不是图片文件"
    );
    Ok(WallpaperSource {
        path,
        modified: metadata.modified()?,
        length: metadata.len(),
    })
}

pub(crate) fn wallpaper_image(source: &WallpaperSource) -> Result<Vec<u8>> {
    let maximum = crate::platform::wallpaper::MAXIMUM_FILE;
    let mut bytes = Vec::new();
    std::fs::File::open(&source.path)?
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= maximum, "壁纸文件过大");
    // A GNOME slideshow is an XML file, not a picture.
    let jpeg = crate::platform::wallpaper::card(bytes).context("壁纸不是静态图片")?;
    ensure!(wallpaper_source()? == *source, "壁纸在读取期间已改变");
    Ok(jpeg)
}

/// One GSettings string value, read with the `gsettings` tool: the settings
/// live in dconf's binary database, which has no stable file format to parse.
fn gsettings(schema: &str, key: &str) -> Result<String> {
    let mut child = Command::new("gsettings")
        .args(["get", schema, key])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("无法运行 gsettings")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait()? {
            Some(status) => {
                ensure!(status.success(), "gsettings 未提供 {schema} {key}");
                break;
            }
            None if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20))
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("gsettings 超时");
            }
        }
    }
    let mut output = String::new();
    child
        .stdout
        .take()
        .context("gsettings 没有输出")?
        .read_to_string(&mut output)?;
    Ok(output.trim().trim_matches('\'').to_owned())
}

fn gsettings_uri(schema: &str, key: &str) -> Result<PathBuf> {
    file_uri(&gsettings(schema, key)?)
}

/// The path of a `file://` URI; any other scheme is not a local file.
fn file_uri(uri: &str) -> Result<PathBuf> {
    if uri.is_empty() {
        return Ok(PathBuf::new());
    }
    let Some(encoded) = uri.strip_prefix("file://") else {
        if uri.starts_with('/') {
            return Ok(PathBuf::from(uri));
        }
        bail!("壁纸必须是本机文件");
    };
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut rest = encoded.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'%'
            && let Some(hex) = tail.get(..2)
            && let Ok(value) = u8::from_str_radix(std::str::from_utf8(hex)?, 16)
        {
            bytes.push(value);
            rest = &tail[2..];
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

/// KDE Plasma's image wallpaper: the first desktop containment's `Image`.
fn plasma_wallpaper() -> Result<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("找不到用户配置目录")?
        .join("plasma-org.kde.plasma.desktop-appletsrc");
    let text = std::fs::read_to_string(&config).context("读取 Plasma 桌面配置")?;
    let mut in_image = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_image = line.ends_with("[Wallpaper][org.kde.image][General]");
            continue;
        }
        if in_image && let Some(value) = line.strip_prefix("Image=") {
            return file_uri(value.trim());
        }
    }
    bail!("Plasma 桌面当前没有静态图片壁纸")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_graphics_from_pci_ids() {
        let database = "10de  NVIDIA Corporation\n\t2230  GA102GL [RTX A6000]\n\t2231  Other\n1002  Advanced Micro Devices, Inc. [AMD/ATI]\n\t73bf  Navi 21 [Radeon RX 6800/6800 XT / 6900 XT]\n8086  Intel Corporation\n\t4680  AlderLake-S GT1\n";
        assert_eq!(
            pci_name(database, 0x10de, 0x2230).as_deref(),
            Some("NVIDIA RTX A6000")
        );
        assert_eq!(
            pci_name(database, 0x1002, 0x73bf).as_deref(),
            Some("AMD/ATI Radeon RX 6800/6800 XT / 6900 XT")
        );
        assert_eq!(
            pci_name(database, 0x8086, 0x4680).as_deref(),
            Some("Intel AlderLake-S GT1")
        );
        // A device listed under another vendor is not borrowed.
        assert_eq!(pci_name(database, 0x10de, 0x73bf), None);
    }

    #[test]
    fn decodes_file_uris() {
        assert_eq!(
            file_uri("file:///usr/share/backgrounds/a%20b.png").unwrap(),
            PathBuf::from("/usr/share/backgrounds/a b.png")
        );
        assert_eq!(file_uri("").unwrap(), PathBuf::new());
        assert!(file_uri("https://example.com/a.png").is_err());
    }

    /// Needs a desktop session: `cargo test -- --ignored reads_this_machine`.
    #[test]
    #[ignore]
    fn reads_this_machine() {
        let hardware = Hardware::read().unwrap();
        println!(
            "name {}\nsystem {}\nmachine {}\nos {}\nboard {}\ncpu {}\nvideo {:?}\nmac {}\nmemory {} MiB\nscreen {}\nerrors {:?}",
            hardware.name,
            hardware.system_uuid,
            hardware.machine_guid,
            hardware.os,
            hardware.base_board,
            hardware.cpu,
            hardware.video,
            hardware.mac,
            hardware.memory,
            hardware.screen,
            hardware.errors
        );
        let source = wallpaper_source().unwrap();
        println!("wallpaper {}", source.path.display());
        let card = wallpaper_image(&source).unwrap();
        println!("card {} bytes", card.len());
    }
}
