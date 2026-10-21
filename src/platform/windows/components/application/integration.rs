//! Windows application registration and installing-user shortcuts.
use super::*;
use windows::{
    Win32::{
        Foundation::*,
        System::{Com::*, Registry::*},
    },
    core::{Interface, PCWSTR, w},
};
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
const KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Farwing");
pub(super) fn register(enabled: bool) -> Result<()> {
    unsafe {
        if !enabled {
            let result = RegDeleteTreeW(HKEY_LOCAL_MACHINE, KEY);
            ensure!(
                result.is_ok() || result == ERROR_FILE_NOT_FOUND,
                "{}",
                t().system.component_unregister_failed
            );
            return Ok(());
        }
        let mut key = HKEY::default();
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            KEY,
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .ok()?;
        let result = (|| -> Result<()> {
            let image = image()?;
            for (name, value) in [
                ("DisplayName", crate::product::CLIENT_NAME.into()),
                ("Publisher", crate::product::PRODUCT_NAME.into()),
                ("DisplayVersion", env!("CARGO_PKG_VERSION").into()),
                ("InstallLocation", directory()?.display().to_string()),
                ("DisplayIcon", format!("\"{}\",0", image.display())),
                (
                    "UninstallString",
                    format!("\"{}\" uninstall", image.display()),
                ),
            ] {
                let value = wide(&value);
                RegSetValueExW(
                    key,
                    PCWSTR(wide(name).as_ptr()),
                    None,
                    REG_SZ,
                    Some(std::slice::from_raw_parts(
                        value.as_ptr().cast(),
                        value.len() * 2,
                    )),
                )
                .ok()?;
            }
            for name in ["NoModify", "NoRepair"] {
                RegSetValueExW(
                    key,
                    PCWSTR(wide(name).as_ptr()),
                    None,
                    REG_DWORD,
                    Some(&1u32.to_le_bytes()),
                )
                .ok()?;
            }
            Ok(())
        })();
        let _ = RegCloseKey(key);
        result
    }
}
struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}
pub(super) fn shortcuts(enabled: bool) -> Result<()> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
    }
    let _apartment = Apartment;
    let image = image()?;
    for id in [&FOLDERID_Desktop, &FOLDERID_Programs] {
        let root = folder(id)?;
        let path = root.join(format!("{}.lnk", crate::product::PRODUCT_NAME));
        let link: IShellLinkW =
            unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)? };
        let file: IPersistFile = link.cast()?;
        if path.exists() {
            unsafe {
                file.Load(PCWSTR(wide(&path.to_string_lossy()).as_ptr()), STGM_READ)?;
            }
            let mut target = [0u16; 32768];
            unsafe {
                link.GetPath(&mut target, std::ptr::null_mut(), 0)?;
            }
            let end = target
                .iter()
                .position(|c| *c == 0)
                .context(t().system.component_invalid_shortcut_target)?;
            let target = PathBuf::from(String::from_utf16_lossy(&target[..end]));
            ensure!(
                target == image
                    || crate::platform::windows::notifications::owns_shortcut(&link, &target),
                "{}",
                t().system.component_shortcut_conflict
            );
        }
        if enabled {
            std::fs::create_dir_all(root)?;
            unsafe {
                link.SetPath(PCWSTR(wide(&image.to_string_lossy()).as_ptr()))?;
                link.SetArguments(w!("gui"))?;
                link.SetWorkingDirectory(PCWSTR(wide(&directory()?.to_string_lossy()).as_ptr()))?;
                link.SetDescription(PCWSTR(wide(crate::product::PRODUCT_NAME).as_ptr()))?;
                link.SetIconLocation(PCWSTR(wide(&image.to_string_lossy()).as_ptr()), 0)?;
                file.Save(PCWSTR(wide(&path.to_string_lossy()).as_ptr()), true)?;
            }
        } else if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}
/// Explorer keeps the program's icon cached across an update of the file,
/// so the taskbar (which takes it from the Start-menu shortcut), the
/// shortcuts and Start would keep showing the old artwork. Best effort.
pub(super) fn refresh_icons() {
    let Ok(image) = image() else {
        return;
    };
    let mut paths = vec![image];
    for id in [&FOLDERID_Desktop, &FOLDERID_Programs] {
        if let Ok(root) = folder(id) {
            paths.push(root.join(format!("{}.lnk", crate::product::PRODUCT_NAME)));
        }
    }
    for path in paths.iter().filter(|path| path.exists()) {
        let path = wide(&path.to_string_lossy());
        let mut info = SHFILEINFOW::default();
        unsafe {
            if SHGetFileInfoW(
                PCWSTR(path.as_ptr()),
                Default::default(),
                Some(&mut info),
                size_of::<SHFILEINFOW>() as u32,
                SHGFI_SYSICONINDEX,
            ) != 0
            {
                // The cached image itself, wherever it is shown.
                SHChangeNotify(
                    SHCNE_UPDATEIMAGE,
                    SHCNF_DWORD | SHCNF_FLUSH,
                    Some((&raw const info.iIcon).cast()),
                    None,
                );
            }
            SHChangeNotify(
                SHCNE_UPDATEITEM,
                SHCNF_PATHW | SHCNF_FLUSH,
                Some(path.as_ptr().cast()),
                None,
            );
        }
    }
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST | SHCNF_FLUSH, None, None) };
}
