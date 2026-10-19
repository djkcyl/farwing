//! User-scoped identity and protocol activation for unpackaged desktop toasts.
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use windows::{
    Win32::{
        Foundation::*,
        System::{Com::StructuredStorage::*, Com::*, Registry::*},
        UI::Shell::{PropertiesSystem::*, *},
    },
    core::{GUID, Interface, PCWSTR},
};
/// Per-profile protocol key, see [`crate::product::notification_scheme`].
fn scheme() -> String {
    format!(
        r"Software\Classes\{}",
        crate::product::notification_scheme()
    )
}
fn command_key() -> String {
    format!(r"{}\shell\open\command", scheme())
}
/// Value marking the protocol key as registered by this client.
const OWNER_VALUE: &str = "FarwingOwner";
fn owner() -> String {
    crate::product::object_name("Notifications.v1")
}
const APP_KEY: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
    pid: 5,
};
const TOAST_KEY: PROPERTYKEY = PROPERTYKEY {
    fmtid: APP_KEY.fmtid,
    pid: 26,
};
const ACTIVATOR: GUID = GUID::from_u128(0x1c2fcbab_048b_4428_a60d_a118eee39d2a);
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}
fn key(path: &str) -> Result<Key> {
    let mut h = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(path).as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_READ | KEY_WRITE,
            None,
            &mut h,
            None,
        )
        .ok()?;
    }
    Ok(Key(h))
}
fn set(h: HKEY, name: &str, value: &str) -> Result<()> {
    let value = wide(value);
    unsafe {
        RegSetValueExW(
            h,
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
    Ok(())
}
/// The activated process must run in the profile whose scheme was invoked.
fn command(image: &Path) -> String {
    if crate::product::is_default_profile() {
        format!("\"{}\" notification \"%1\"", image.display())
    } else {
        format!(
            "\"{}\" {} {} notification \"%1\"",
            image.display(),
            crate::product::PROFILE_ARG,
            crate::product::profile()
        )
    }
}
fn read(path: &str, name: &str) -> Result<Option<String>> {
    let mut value = vec![0u16; 32768];
    let mut bytes = (value.len() * 2) as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(path).as_ptr()),
            PCWSTR(wide(name).as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(value.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    if result == ERROR_FILE_NOT_FOUND || result == ERROR_PATH_NOT_FOUND {
        return Ok(None);
    }
    result.ok()?;
    let end = value
        .iter()
        .position(|&v| v == 0)
        .context("通知注册值无效")?;
    Ok(Some(String::from_utf16(&value[..end])?))
}
fn stored_command() -> Option<String> {
    read(&command_key(), "").ok().flatten()
}
fn owned_registration() -> bool {
    let mut value = [0u16; 128];
    let mut bytes = (value.len() * 2) as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(&scheme()).as_ptr()),
            PCWSTR(wide(OWNER_VALUE).as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(value.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    result.is_ok()
        && String::from_utf16_lossy(
            &value[..value.iter().position(|&v| v == 0).unwrap_or(value.len())],
        ) == owner()
}
pub(crate) fn owns_shortcut(link: &IShellLinkW, target: &Path) -> bool {
    let read = (|| -> Result<bool> {
        let properties: IPropertyStore = link.cast()?;
        let value = unsafe { properties.GetValue(&APP_KEY)? };
        let name = windows::core::BSTR::try_from(&value)?.to_string();
        Ok(name == crate::product::aumid()
            && owned_registration()
            && stored_command().is_some_and(|v| v.eq_ignore_ascii_case(&command(target))))
    })();
    read.unwrap_or(false)
}
fn shortcut() -> Result<PathBuf> {
    let p = unsafe { SHGetKnownFolderPath(&FOLDERID_Programs, KF_FLAG_DEFAULT, None)? };
    let root = unsafe { p.to_string() };
    unsafe {
        CoTaskMemFree(Some(p.0.cast()));
    }
    Ok(PathBuf::from(root?).join(crate::product::shortcut_file_name()))
}
pub(super) fn register(aumid: &str) -> Result<()> {
    let image = dunce::canonicalize(std::env::current_exe()?)?;
    let scheme = scheme();
    let old = read(&command_key(), "")?;
    let registered_owner = read(&scheme, OWNER_VALUE)?;
    let mut existing = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(&scheme).as_ptr()),
            None,
            KEY_READ,
            &mut existing,
        )
    };
    if opened.is_ok() {
        drop(Key(existing));
        ensure!(
            old.as_ref()
                .is_some_and(|v| v.eq_ignore_ascii_case(&command(&image)))
                || registered_owner.as_deref() == Some(owner().as_str()),
            "通知协议已登记到其他程序"
        );
    } else if opened != ERROR_FILE_NOT_FOUND && opened != ERROR_PATH_NOT_FOUND {
        opened.ok()?;
    }
    let path = shortcut()?;
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)? };
    let file: IPersistFile = link.cast()?;
    if path.exists() {
        unsafe {
            // Updating an existing shortcut also writes its property store.
            // STGM_READ makes SetValue/Commit fail with STG_E_ACCESSDENIED,
            // even when the user has full filesystem access to the shortcut.
            file.Load(
                PCWSTR(wide(&path.to_string_lossy()).as_ptr()),
                STGM_READWRITE,
            )
            .context("以读写方式打开通知快捷方式")?;
        }
        let mut target = [0u16; 32768];
        unsafe {
            link.GetPath(&mut target, std::ptr::null_mut(), 0)?;
        }
        let end = target
            .iter()
            .position(|&v| v == 0)
            .context("快捷方式目标无效")?;
        let target = PathBuf::from(String::from_utf16_lossy(&target[..end]));
        ensure!(
            target == image || owns_shortcut(&link, &target),
            "已有Farwing快捷方式指向其他程序，已保留"
        );
    }
    // A profile's shortcut must start that profile, like its protocol command.
    let arguments = if crate::product::is_default_profile() {
        "gui".to_owned()
    } else {
        format!(
            "{} {} gui",
            crate::product::PROFILE_ARG,
            crate::product::profile()
        )
    };
    unsafe {
        link.SetPath(PCWSTR(wide(&image.to_string_lossy()).as_ptr()))?;
        link.SetArguments(PCWSTR(wide(&arguments).as_ptr()))?;
        link.SetDescription(PCWSTR(wide(&crate::product::display_name()).as_ptr()))?;
        link.SetIconLocation(PCWSTR(wide(&image.to_string_lossy()).as_ptr()), 0)?;
    }
    let properties: IPropertyStore = link.cast()?;
    let text = PROPVARIANT::from(aumid);
    let mut app = PROPVARIANT::default();
    unsafe {
        PropVariantChangeType(
            &mut app,
            &text,
            PROPVAR_CHANGE_FLAGS(0),
            windows::Win32::System::Variant::VT_LPWSTR,
        )?;
    }
    let activator = unsafe { InitPropVariantFromCLSID(&ACTIVATOR)? };
    std::fs::create_dir_all(path.parent().context("快捷方式目录无效")?)?;
    unsafe {
        properties
            .SetValue(&APP_KEY, &app)
            .context("写入通知应用标识")?;
        properties
            .SetValue(&TOAST_KEY, &activator)
            .context("写入通知激活标识")?;
        properties.Commit().context("提交通知快捷方式属性")?;
        file.Save(PCWSTR(wide(&path.to_string_lossy()).as_ptr()), true)
            .context("保存通知快捷方式")?;
    }
    let root = key(&scheme)?;
    set(
        root.0,
        "",
        &format!("URL:{} notification", crate::product::display_name()),
    )?;
    set(root.0, "URL Protocol", "")?;
    set(root.0, OWNER_VALUE, &owner())?;
    let open = key(&command_key())?;
    set(open.0, "", &command(&image))?;
    unsafe {
        SHChangeNotify(
            SHCNE_CREATE,
            SHCNF_PATHW | SHCNF_FLUSH,
            Some(wide(&path.to_string_lossy()).as_ptr().cast()),
            None,
        );
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST | SHCNF_FLUSH, None, None);
    }
    Ok(())
}
pub(crate) fn unregister(image: &Path) -> Result<()> {
    if owned_registration()
        && stored_command().is_some_and(|v| v.eq_ignore_ascii_case(&command(image)))
    {
        unsafe {
            RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(wide(&scheme()).as_ptr())).ok()?;
        }
    }
    Ok(())
}
