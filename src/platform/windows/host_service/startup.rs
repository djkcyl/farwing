//! Installing user's ordinary-permission tray startup; no Windows password.
use crate::i18n::t;
use anyhow::{Result, ensure};
use windows::{
    Win32::System::Registry::*,
    core::{PCWSTR, w},
};
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
            ensure!(
                path.is_file(),
                "{}",
                t().system.service_autostart_not_deployed
            );
            let command: Vec<u16> = format!("\"{}\" gui --background", path.display())
                .encode_utf16()
                .chain(Some(0))
                .collect();
            unsafe {
                RegSetValueExW(
                    key,
                    w!("Farwing"),
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
            let result = unsafe { RegDeleteValueW(key, PCWSTR(w!("Farwing").as_ptr())) };
            ensure!(
                result.is_ok() || result == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND,
                "{}",
                t().system.service_autostart_remove_failed
            );
        }
        Ok(())
    })();
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}
