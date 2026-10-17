//! Installing user's ordinary-permission tray startup; no Windows password.
use anyhow::{Context, Result, ensure};
use windows::{
    Win32::{Foundation::ERROR_FILE_NOT_FOUND, System::Registry::*},
    core::{PCWSTR, w},
};
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum State {
    Disabled,
    Enabled,
    DisabledByWindows,
}

/// Registration and Windows approval are independent; installation only retargets
/// the former and must never undo a user's decision in Startup Apps.
fn registered() -> Result<bool> {
    let mut size = 0u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Run"),
            w!("OpenUUYC"),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    result.ok()?;
    Ok(true)
}

/// Windows 的登录自启是当前用户的 Run 项，不依赖桌面环境，永远可用。
pub(crate) fn unavailable() -> Option<String> {
    None
}

pub(crate) fn state() -> Result<State> {
    if !registered()? {
        return Ok(State::Disabled);
    }
    let mut approval = [0u8; 12];
    let mut size = approval.len() as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run"),
            w!("OpenUUYC"),
            RRF_RT_REG_BINARY,
            None,
            Some(approval.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(State::Enabled);
    }
    result.ok().context("读取 Windows 启动应用状态失败")?;
    ensure!(size == 12, "Windows 启动应用状态长度无效：{size}");
    match u32::from_le_bytes(approval[..4].try_into().unwrap()) {
        2 | 6 => Ok(State::Enabled),
        3 | 7 => Ok(State::DisabledByWindows),
        value => anyhow::bail!("无法识别 Windows 启动应用状态：{value}"),
    }
}

/// First install keeps the existing default. Updates and migration recovery only
/// retarget an existing registration, preserving both absence and system denial.
pub(crate) fn configure_installation(first_install: bool) -> Result<()> {
    if first_install || registered()? {
        set(true)?;
    }
    Ok(())
}
/// The settings switch. Points at the installed program while the components
/// are installed, otherwise at this copy, as enabling remote wake does.
pub(crate) fn configure(enabled: bool) -> Result<()> {
    if !enabled {
        return set_image(false, std::path::Path::new(""));
    }
    let image = if super::resident::managed() {
        crate::platform::windows::components::application::image()?
    } else {
        std::env::current_exe()?
    };
    set_image(true, &image)?;
    ensure!(
        state()? == State::Enabled,
        "自启动项已登记，但 Windows 已禁用；请在 Windows 设置的“应用 → 启动”中启用 OpenUUYC"
    );
    Ok(())
}
pub(crate) fn set(enabled: bool) -> Result<()> {
    set_image(
        enabled,
        &crate::platform::windows::components::application::image()?,
    )
}
pub(crate) fn set_image(enabled: bool, path: &std::path::Path) -> Result<()> {
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Run"),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()?;
    }
    let result = (|| -> Result<()> {
        if enabled {
            ensure!(path.is_file(), "尚未部署自启动程序");
            let command: Vec<u16> = format!("\"{}\" gui --background", path.display())
                .encode_utf16()
                .chain(Some(0))
                .collect();
            unsafe {
                RegSetValueExW(
                    key,
                    w!("OpenUUYC"),
                    None,
                    REG_SZ,
                    Some(std::slice::from_raw_parts(
                        command.as_ptr().cast(),
                        command.len() * 2,
                    )),
                )
                .ok()?;
            }
        } else {
            let result = unsafe { RegDeleteValueW(key, PCWSTR(w!("OpenUUYC").as_ptr())) };
            ensure!(
                result.is_ok() || result == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND,
                "无法删除本程序自启动设置"
            );
        }
        Ok(())
    })();
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}
