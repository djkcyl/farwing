//! Explicit Ethernet WoL inspection/configuration. Never restarts an adapter.
use anyhow::{Context, Result, ensure};
use crate::i18n::t;
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
        uuid::Uuid::parse_str(&self.guid).context(t().system.wol_adapter_id_invalid)?;
        ensure!(
            self.guid.len() == 36
                && self
                    .guid
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() || b == b'-'),
            "{}",
            t().system.wol_adapter_id_invalid
        );
        ensure!(
            self.mac.len() == 12 && self.mac.bytes().all(|b| b.is_ascii_hexdigit()),
            "{}",
            t().system.wol_adapter_mac_invalid
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
            "WakeOnMagicPacket" => t().system.wol_property_magic_packet_system,
            "*WakeOnMagicPacket" => t().system.wol_property_magic_packet_driver,
            "AllowComputerToTurnOffDevice" => t().system.wol_property_power_management,
            "S5WakeOnLan" => t().system.wol_property_wake_from_shutdown,
            "EnablePME" => t().system.wol_property_pme,
            "DeviceWake" => t().system.wol_property_device_wake,
            "MagicPacketOnly" => t().system.wol_property_magic_packet_only,
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
        "{}",
        t().system.wol_admin_required
    );
    ensure!(!cancel.is_cancelled(), "{}", t().system.wol_cancelled);
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
        "{}",
        t().system.wol_system_directory_unavailable
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
        .context(t().system.wol_check_start_failed)?;
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
            break Err(anyhow::anyhow!("{}", t().system.wol_check_cancelled));
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
        .map_err(|_| anyhow::anyhow!("{}", t().system.wol_result_interrupted))??;
    let _ = errors.join();
    ensure!(outcome?.success(), "{}", t().system.wol_check_failed);
    ensure!(data.len() <= 1024 * 1024, "{}", t().system.wol_result_too_large);
    serde_json::from_slice(&data).context(t().system.wol_result_invalid)
}

/// CLI is only a narrowly scoped local elevation helper; cloud authorization
/// stays in the original account worker. No arbitrary scripts or registry keys.
pub(crate) fn configure_elevated(guid: String, mac: String) -> Result<()> {
    let key = AdapterKey { guid, mac };
    let result = inspect(Some(&key), true, &CancellationToken::new())?;
    ensure!(
        result.len() == 1 && result[0].errors.is_empty(),
        "部分网卡设置失败"
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
    ensure!(!cancel.is_cancelled(), "{}", t().system.wol_cancelled);
    unsafe { ShellExecuteExW(&mut info) }.context(t().system.wol_configure_not_started)?;
    let handle = crate::platform::windows::host_service::pipe::Handle(info.hProcess);
    ensure!(!handle.0.is_invalid(), "{}", t().system.wol_configure_process_unavailable);
    // Once elevated configuration starts, wait for its bounded operation. Do not
    // interrupt an arbitrary successful subset by killing the privileged helper.
    let result = unsafe { WaitForSingleObject(handle.0, 60_000) };
    ensure!(result == WAIT_OBJECT_0, "{}", t().system.wol_configure_unfinished);
    let mut code = 0;
    unsafe {
        GetExitCodeProcess(handle.0, &mut code)?;
    }
    let mut rows = inspect(Some(key), false, cancel)?;
    if code != 0 {
        for row in &mut rows {
            row.errors.push(t().system.wol_partially_applied.into());
        }
    }
    Ok(rows)
}
