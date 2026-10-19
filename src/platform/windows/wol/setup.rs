//! Explicit Ethernet WoL inspection/configuration. Never restarts an adapter.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::Read,
    os::windows::process::CommandExt,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AdapterKey {
    pub guid: String,
    pub mac: String,
}
impl AdapterKey {
    pub fn validate(&self) -> Result<()> {
        uuid::Uuid::parse_str(&self.guid).context("网卡标识无效")?;
        ensure!(
            self.guid.len() == 36
                && self
                    .guid
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() || b == b'-'),
            "网卡标识无效"
        );
        ensure!(
            self.mac.len() == 12 && self.mac.bytes().all(|b| b.is_ascii_hexdigit()),
            "网卡MAC无效"
        );
        Ok(())
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Property {
    pub key: String,
    pub value: String,
    pub writable: bool,
}
impl Property {
    pub fn title(&self) -> &str {
        match self.key.as_str() {
            "WakeOnMagicPacket" => "魔术包唤醒（系统）",
            "*WakeOnMagicPacket" => "魔术包唤醒（驱动）",
            "AllowComputerToTurnOffDevice" => "网卡电源管理",
            "S5WakeOnLan" => "关机后唤醒",
            "EnablePME" => "电源管理事件（PME）",
            "DeviceWake" => "允许网卡唤醒电脑",
            "MagicPacketOnly" => "仅允许魔术包唤醒",
            _ => &self.key,
        }
    }
    pub fn enabled(&self) -> Option<bool> {
        match self.value.as_str() {
            "Enabled" | "1" => Some(true),
            "Disabled" | "0" => Some(false),
            _ => None,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Adapter {
    pub key: AdapterKey,
    pub name: String,
    pub description: String,
    pub index: u32,
    pub connected: bool,
    pub items: Vec<Property>,
    pub errors: Vec<String>,
}

pub(crate) fn inspect(
    target: Option<&AdapterKey>,
    apply: bool,
    cancel: &CancellationToken,
) -> Result<Vec<Adapter>> {
    if let Some(key) = target {
        key.validate()?;
    }
    ensure!(
        !apply || (target.is_some() && crate::platform::windows::components::elevated()?),
        "配置网卡需要管理员权限"
    );
    ensure!(!cancel.is_cancelled(), "操作已取消");
    let init = match target {
        Some(key) => format!("$Target='{}';$ExpectedMac='{}';", key.guid, key.mac),
        None => "$Target=$null;$ExpectedMac=$null;".into(),
    };
    let script = format!(
        "{init}$Apply=${};\n{}",
        if apply { "true" } else { "false" },
        include_str!("setup.ps1")
    );
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(
        script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    );
    // Fixed OS binary, no profiles, no user-controlled script or wildcard names.
    let mut directory = [0u16; 32768];
    let length = unsafe {
        windows::Win32::System::SystemInformation::GetSystemDirectoryW(Some(&mut directory))
    } as usize;
    ensure!(
        length > 0 && length < directory.len(),
        "Windows系统目录不可用"
    );
    let exe = std::path::PathBuf::from(String::from_utf16_lossy(&directory[..length]))
        .join("WindowsPowerShell/v1.0/powershell.exe");
    let mut child = Command::new(exe)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &encoded,
        ])
        .creation_flags(0x08000000)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("无法启动网卡配置检查")?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let output = std::thread::spawn(move || {
        let mut data = Vec::new();
        stdout
            .take(1024 * 1024 + 1)
            .read_to_end(&mut data)
            .map(|_| data)
    });
    let errors = std::thread::spawn(move || {
        let mut data = Vec::new();
        stderr.take(64 * 1024).read_to_end(&mut data).map(|_| data)
    });
    let start = Instant::now();
    let outcome = loop {
        if cancel.is_cancelled() || start.elapsed() > Duration::from_secs(45) {
            let _ = child.kill();
            let _ = child.wait();
            break Err(anyhow::anyhow!(
                "网卡检查或配置已取消/超时；请重新检查实际状态"
            ));
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(30)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(e.into());
            }
        }
    };
    let data = output
        .join()
        .map_err(|_| anyhow::anyhow!("网卡结果读取中断"))??;
    let _ = errors.join();
    ensure!(
        outcome?.success(),
        "网卡检查或配置失败；请确认目标仍存在且有读取/配置权限"
    );
    ensure!(data.len() <= 1024 * 1024, "网卡结果过大");
    serde_json::from_slice(&data).context("网卡检查结果格式错误")
}

/// CLI is only a narrowly scoped local elevation helper; cloud authorization
/// stays in the original account worker. No arbitrary scripts or registry keys.
pub(crate) fn configure_elevated(guid: String, mac: String) -> Result<()> {
    let key = AdapterKey { guid, mac };
    let result = inspect(Some(&key), true, &CancellationToken::new())?;
    ensure!(
        result.len() == 1 && result[0].errors.is_empty(),
        "部分网卡设置失败，请重新检查；已成功的设置保留"
    );
    Ok(())
}

pub(crate) fn configure(key: &AdapterKey, cancel: &CancellationToken) -> Result<Vec<Adapter>> {
    key.validate()?;
    if crate::platform::windows::components::elevated()? {
        return inspect(Some(key), true, cancel);
    }
    use windows::{
        Win32::{
            Foundation::*,
            System::Threading::*,
            UI::{Shell::*, WindowsAndMessaging::SW_HIDE},
        },
        core::{PCWSTR, w},
    };
    let exe = std::env::current_exe()?
        .to_string_lossy()
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let args = format!("wol-configure --interface {} --mac {}", key.guid, key.mac)
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: w!("runas"),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: PCWSTR(args.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    ensure!(!cancel.is_cancelled(), "操作已取消");
    unsafe { ShellExecuteExW(&mut info) }.context("网卡配置未启动（可能取消了管理员授权）")?;
    let handle = crate::platform::windows::host_service::pipe::Handle(info.hProcess);
    ensure!(!handle.0.is_invalid(), "网卡配置进程不可用");
    // Once elevated configuration starts, wait for its bounded operation. Do not
    // interrupt an arbitrary successful subset by killing the privileged helper.
    let result = unsafe { WaitForSingleObject(handle.0, 60_000) };
    ensure!(result == WAIT_OBJECT_0, "网卡配置尚未结束；请稍后重新检查");
    let mut code = 0;
    unsafe {
        GetExitCodeProcess(handle.0, &mut code)?;
    }
    let mut rows = inspect(Some(key), false, cancel)?;
    if code != 0 {
        for row in &mut rows {
            row.errors.push("部分设置未完成，请检查当前状态".into());
        }
    }
    Ok(rows)
}
