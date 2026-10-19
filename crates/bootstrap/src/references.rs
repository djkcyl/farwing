//! Re-read owned OS entry points before collecting cached binaries. A failed
//! metadata write must never make a live registration point at a deleted file.
use super::*;
use windows::{
    Win32::{
        Foundation::*,
        System::{Com::*, Registry::*},
        UI::Shell::*,
    },
    core::{Interface, PCWSTR},
};
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn value(key: &str, name: &str) -> Result<Option<String>> {
    let mut bytes = 0;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(key).as_ptr()),
            PCWSTR(wide(name).as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut bytes),
        )
    };
    if result == ERROR_FILE_NOT_FOUND || result == ERROR_PATH_NOT_FOUND {
        return Ok(None);
    }
    result.ok()?;
    ensure!(bytes <= 65536, "启动引用长度无效");
    let mut text = vec![0u16; (bytes as usize).div_ceil(2)];
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(key).as_ptr()),
            PCWSTR(wide(name).as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(text.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
        .ok()?;
    }
    let length = text.iter().position(|v| *v == 0).unwrap_or(text.len());
    Ok(Some(String::from_utf16(&text[..length])?))
}
pub(super) fn live() -> Result<Vec<String>> {
    // Default-profile entry points, mirroring the application crate's Run
    // value, `product::notification_scheme()` and `product::shortcut_file_name()`.
    let mut values = Vec::new();
    for (key, name) in [
        (r"Software\Microsoft\Windows\CurrentVersion\Run", PRODUCT),
        (
            r"Software\Classes\farwing-notification\shell\open\command",
            "",
        ),
    ] {
        if let Some(value) = value(key, name)? {
            values.push(value.to_ascii_lowercase());
        }
    }
    let link = known_folder(&FOLDERID_Programs)?.join(format!("{PRODUCT}.lnk"));
    no_reparse(&link)?;
    if link.exists() {
        struct Apartment(bool);
        impl Drop for Apartment {
            fn drop(&mut self) {
                if self.0 {
                    unsafe {
                        CoUninitialize();
                    }
                }
            }
        }
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if result.is_err() && result != RPC_E_CHANGED_MODE {
            result.ok()?;
        }
        let _apartment = Apartment(result.is_ok());
        let shell: IShellLinkW =
            unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)? };
        let file: IPersistFile = shell.cast()?;
        let path: Vec<_> = link.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            file.Load(PCWSTR(path.as_ptr()), STGM_READ)?;
        }
        let mut buffer = [0u16; 32768];
        unsafe {
            shell.GetPath(&mut buffer, std::ptr::null_mut(), 0)?;
        }
        let end = buffer
            .iter()
            .position(|v| *v == 0)
            .context("快捷方式目标无效")?;
        values.push(String::from_utf16(&buffer[..end])?.to_ascii_lowercase());
    }
    Ok(values)
}
