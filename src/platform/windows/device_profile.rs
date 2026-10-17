//! Windows device facts and the registered user's static desktop wallpaper.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{io::Read, path::PathBuf};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        NetworkManagement::IpHelper::*,
        System::{Registry::*, SystemInformation::*},
        UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN},
    },
    core::{PCWSTR, PWSTR},
};

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

pub(crate) fn registry_string(root: HKEY, key: &str, name: &str) -> Result<String> {
    let key: Vec<u16> = key.encode_utf16().chain(Some(0)).collect();
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut size = 0;
    unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            None,
            None,
            Some(&mut size),
        )
        .ok()?;
    }
    ensure!(size > 0 && size <= 65536, "系统字符串长度无效");
    let mut text = vec![0u16; (size as usize).div_ceil(2)];
    unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            None,
            Some(text.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()?;
    }
    let n = text.iter().position(|c| *c == 0).unwrap_or(text.len());
    Ok(String::from_utf16(&text[..n])?.trim().to_owned())
}

pub(crate) fn computer_name() -> Result<String> {
    let mut name = [0u16; 256];
    let mut size = name.len() as u32;
    unsafe {
        GetComputerNameExW(
            ComputerNameNetBIOS,
            Some(PWSTR(name.as_mut_ptr())),
            &mut size,
        )?;
    }
    Ok(String::from_utf16(&name[..size as usize])?)
}

impl Hardware {
    pub fn read() -> Result<Self> {
        let mut value = Self {
            name: computer_name()?,
            system_uuid: system_uuid()?,
            machine_guid: registry_string(
                HKEY_LOCAL_MACHINE,
                r"SOFTWARE\Microsoft\Cryptography",
                "MachineGuid",
            )?,
            ..Self::default()
        };
        ensure!(
            !value.name.is_empty() && !value.machine_guid.is_empty(),
            "本机名称或机器标识缺失"
        );
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
        value.cpu = field(
            "处理器",
            registry_string(
                HKEY_LOCAL_MACHINE,
                r"HARDWARE\DESCRIPTION\System\CentralProcessor\0",
                "ProcessorNameString",
            ),
        );
        value.base_board = field(
            "主板",
            (|| {
                let key = r"HARDWARE\DESCRIPTION\System\BIOS";
                Ok(format!(
                    "Manufacturer: {}  Product: {}",
                    registry_string(HKEY_LOCAL_MACHINE, key, "BaseBoardManufacturer")?,
                    registry_string(HKEY_LOCAL_MACHINE, key, "BaseBoardProduct")?
                ))
            })(),
        );
        value.os = field(
            "操作系统",
            (|| {
                let key = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
                let build = registry_string(HKEY_LOCAL_MACHINE, key, "CurrentBuildNumber")?;
                let mut product = registry_string(HKEY_LOCAL_MACHINE, key, "ProductName")?;
                // Windows 11 retains Windows 10 in this registry product label.
                if build.parse::<u32>()? >= 22000 && product.starts_with("Windows 10") {
                    product = product.replacen("Windows 10", "Windows 11", 1);
                }
                Ok(format!("{product} 64-bit · Build {build}"))
            })(),
        );
        value.mac = field("网卡地址", primary_mac());
        match graphics() {
            Ok(names) => value.video = names,
            Err(e) => value.errors.push(format!("显卡：{e:#}")),
        }
        let mut memory = MEMORYSTATUSEX {
            dwLength: size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        match unsafe { GlobalMemoryStatusEx(&mut memory) } {
            Ok(()) => value.memory = (memory.ullTotalPhys / (1024 * 1024)) as i64,
            Err(e) => value.errors.push(format!("内存：{e}")),
        }
        value.screen = unsafe {
            format!(
                "{}x{}",
                GetSystemMetrics(SM_CXSCREEN).max(0),
                GetSystemMetrics(SM_CYSCREEN).max(0)
            )
        };
        Ok(value)
    }
}

fn graphics() -> Result<Vec<String>> {
    let mut names = Vec::new();
    for index in 0.. {
        let mut device = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        if !unsafe { EnumDisplayDevicesW(None, index, &mut device, 0) }.as_bool() {
            break;
        }
        let length = device
            .DeviceString
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(device.DeviceString.len());
        let name = String::from_utf16(&device.DeviceString[..length])?;
        if !names.contains(&name) {
            names.push(name);
        }
    }
    Ok(names)
}

pub(crate) fn system_uuid() -> Result<String> {
    let size = unsafe { GetSystemFirmwareTable(RSMB, 0, None) };
    ensure!(
        size >= 8 && size <= 16 * 1024 * 1024,
        "无法读取 SMBIOS 系统标识"
    );
    let mut raw = vec![0; size as usize];
    ensure!(
        unsafe { GetSystemFirmwareTable(RSMB, 0, Some(&mut raw)) } == size,
        "SMBIOS 数据读取不完整"
    );
    ensure!(
        (raw[1], raw[2]) >= (2, 6),
        "SMBIOS 版本不支持标准 UUID 字节序"
    );
    let length = u32::from_le_bytes(raw[4..8].try_into()?) as usize;
    let table = raw.get(8..8 + length).context("SMBIOS 长度无效")?;
    let mut offset = 0;
    while let Some(header) = table.get(offset..offset + 4) {
        let length = usize::from(header[1]);
        ensure!(length >= 4, "SMBIOS 结构长度无效");
        let record = table
            .get(offset..offset + length)
            .context("SMBIOS 结构不完整")?;
        if header[0] == 1 {
            let bytes: [u8; 16] = record
                .get(8..24)
                .context("SMBIOS 系统 UUID 缺失")?
                .try_into()?;
            ensure!(
                bytes.iter().any(|b| *b != 0) && bytes.iter().any(|b| *b != 255),
                "SMBIOS 未提供有效系统 UUID"
            );
            return Ok(uuid::Uuid::from_bytes_le(bytes).to_string().to_uppercase());
        }
        if header[0] == 127 {
            break;
        }
        let strings = table
            .get(offset + length..)
            .context("SMBIOS 字符串区无效")?;
        let end = strings
            .windows(2)
            .position(|s| s == [0, 0])
            .context("SMBIOS 字符串区不完整")?;
        offset += length + end + 2;
    }
    anyhow::bail!("SMBIOS 系统信息不存在")
}

fn primary_mac() -> Result<String> {
    let mut size = 16 * 1024u32;
    for _ in 0..3 {
        let mut storage = vec![0u64; (size as usize).div_ceil(8)];
        let start = storage.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        let code = unsafe {
            GetAdaptersAddresses(0, GAA_FLAG_INCLUDE_GATEWAYS, None, Some(start), &mut size)
        };
        if code == ERROR_BUFFER_OVERFLOW.0 {
            ensure!(size <= 1024 * 1024, "网卡列表过大");
            continue;
        }
        WIN32_ERROR(code).ok()?;
        let mut candidates = Vec::new();
        let mut next = start;
        while !next.is_null() {
            let adapter = unsafe { &*next };
            next = adapter.Next;
            if !matches!(adapter.IfType, 6 | 71) || adapter.PhysicalAddressLength != 6 {
                continue;
            }
            if adapter.OperStatus != windows::Win32::NetworkManagement::Ndis::IfOperStatusUp {
                continue;
            }
            if adapter.FirstGatewayAddress.is_null() {
                continue;
            }
            candidates.push((
                adapter.Ipv4Metric,
                adapter.PhysicalAddress[..6]
                    .iter()
                    .map(|b| format!("{b:02X}"))
                    .collect::<Vec<_>>()
                    .join(":"),
            ));
        }
        candidates.sort();
        return Ok(candidates
            .into_iter()
            .next()
            .map(|(_, mac)| mac)
            .unwrap_or_default());
    }
    anyhow::bail!("网卡列表持续变化")
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct WallpaperSource {
    pub path: PathBuf,
    configured_path: PathBuf,
    pub modified: std::time::SystemTime,
    pub length: u64,
}
impl WallpaperSource {
    pub(crate) fn cached(&self) -> bool {
        self.path != self.configured_path
    }
}
pub(crate) fn wallpaper_source() -> Result<WallpaperSource> {
    let user = WallpaperUser::enter()?;
    wallpaper_source_inner(&user)
}
struct WallpaperUser {
    owner: Option<String>,
    token: Option<super::host_service::pipe::Handle>,
}
impl WallpaperUser {
    fn enter() -> Result<Self> {
        use super::host_service::{pipe::Handle, process, vault};
        if vault::sid(std::process::id())? != "S-1-5-18" {
            return Ok(Self {
                owner: None,
                token: None,
            });
        }
        let owner = vault::owner()?.context("壁纸用户未登记")?;
        let mut token = HANDLE::default();
        unsafe {
            windows::Win32::System::RemoteDesktop::WTSQueryUserToken(
                process::active_session(),
                &mut token,
            )
        }
        .context("当前没有可读取壁纸的已登录用户")?;
        let token = Handle(token);
        ensure!(
            vault::token_sid(token.0)? == owner,
            "当前登录用户不是设备登记用户"
        );
        unsafe {
            windows::Win32::Security::ImpersonateLoggedOnUser(token.0)?;
        }
        Ok(Self {
            owner: Some(owner),
            token: Some(token),
        })
    }

    fn desktop_key(&self) -> (HKEY, String) {
        match &self.owner {
            Some(owner) => (HKEY_USERS, format!(r"{owner}\Control Panel\Desktop")),
            None => (HKEY_CURRENT_USER, r"Control Panel\Desktop".into()),
        }
    }

    fn cached_wallpaper(&self) -> Result<PathBuf> {
        use windows::Win32::{System::Com::CoTaskMemFree, UI::Shell::*};
        // Impersonation does not change SYSTEM's process environment or its
        // known folders. Pass the verified enrollment user's token explicitly.
        let path = unsafe {
            SHGetKnownFolderPath(
                &FOLDERID_RoamingAppData,
                KF_FLAG_DEFAULT,
                self.token.as_ref().map(|token| token.0),
            )?
        };
        let value = unsafe { path.to_string() };
        unsafe { CoTaskMemFree(Some(path.0.cast())) };
        Ok(PathBuf::from(value?).join(r"Microsoft\Windows\Themes\TranscodedWallpaper"))
    }
}
impl Drop for WallpaperUser {
    fn drop(&mut self) {
        if self.owner.is_some() && unsafe { windows::Win32::Security::RevertToSelf() }.is_err() {
            std::process::abort();
        }
    }
}
fn wallpaper_source_inner(user: &WallpaperUser) -> Result<WallpaperSource> {
    // The resident runs as SYSTEM. Read only its explicitly enrolled user's
    // loaded desktop settings, never SYSTEM's wallpaper or another session's.
    let (root, key) = user.desktop_key();
    let path = registry_string(root, &key, "WallPaper")?;
    ensure!(!path.is_empty(), "Windows 当前没有静态图片壁纸");
    let path = PathBuf::from(path);
    resolve_wallpaper(path, || {
        Ok((
            user.cached_wallpaper()?,
            wallpaper_cache_record(root, &key)?,
        ))
    })
}

fn resolve_wallpaper(
    path: PathBuf,
    cache: impl FnOnce() -> Result<(PathBuf, Vec<u8>)>,
) -> Result<WallpaperSource> {
    match wallpaper_file(path.clone(), path.clone()) {
        Ok(source) => return Ok(source),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            ()
        }
        Err(error) => return Err(error),
    }
    // Do not pick an arbitrary old thumbnail when the original was removed.
    // The standard current cache must identify this exact configured image.
    let (cached_path, bytes) =
        cache().context("壁纸原文件不可用，无法核对 Windows 当前壁纸缓存")?;
    let cached_from = transcoded_source(&bytes);
    ensure!(
        cached_from.as_ref() == Some(&path),
        "壁纸原文件不可用，Windows 壁纸缓存与当前设置不一致，请重新选择壁纸"
    );
    wallpaper_file(cached_path, path).context("壁纸原文件不可用，Windows 当前壁纸缓存也不可用")
}

fn wallpaper_file(path: PathBuf, configured_path: PathBuf) -> Result<WallpaperSource> {
    ensure!(
        path.is_absolute() && !path.to_string_lossy().starts_with(r"\\"),
        "壁纸必须是本机文件"
    );
    let metadata = std::fs::metadata(&path).context("读取当前 Windows 壁纸")?;
    ensure!(
        metadata.is_file() && metadata.len() <= 64 * 1024 * 1024,
        "壁纸文件过大或不是图片文件"
    );
    Ok(WallpaperSource {
        path,
        configured_path,
        modified: metadata.modified()?,
        length: metadata.len(),
    })
}

fn wallpaper_cache_record(root: HKEY, key: &str) -> Result<Vec<u8>> {
    let key: Vec<u16> = key.encode_utf16().chain(Some(0)).collect();
    let name = windows::core::w!("TranscodedImageCache");
    let mut size = 0;
    unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            name,
            RRF_RT_REG_BINARY | RRF_SUBKEY_WOW6464KEY,
            None,
            None,
            Some(&mut size),
        )
        .ok()?;
    }
    ensure!((26..=65536).contains(&size), "Windows 壁纸缓存记录长度无效");
    let mut bytes = vec![0u8; size as usize];
    unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            name,
            RRF_RT_REG_BINARY | RRF_SUBKEY_WOW6464KEY,
            None,
            Some(bytes.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()?;
    }
    bytes.truncate(size as usize);
    Ok(bytes)
}

fn transcoded_source(bytes: &[u8]) -> Option<PathBuf> {
    // Windows' TranscodedImageCache stores a NUL-terminated UTF-16 source path
    // after its 24-byte header. Unrecognized records must not authorize a cache.
    let payload = bytes.get(24..)?;
    if payload.len() % 2 != 0 {
        return None;
    }
    let words: Vec<u16> = payload
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    let end = words.iter().position(|&word| word == 0)?;
    let text = String::from_utf16(&words[..end]).ok()?;
    (!text.is_empty()).then(|| PathBuf::from(text))
}

pub(crate) fn wallpaper_image(source: &WallpaperSource) -> Result<Vec<u8>> {
    let user = WallpaperUser::enter()?;
    let maximum = crate::platform::wallpaper::MAXIMUM_FILE;
    let mut bytes = Vec::new();
    std::fs::File::open(&source.path)?
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= maximum, "壁纸文件过大");
    let jpeg = crate::platform::wallpaper::card(bytes)?;
    ensure!(
        wallpaper_source_inner(&user)? == *source,
        "壁纸在读取期间已改变"
    );
    Ok(jpeg)
}
