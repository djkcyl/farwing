//! Single source of Farwing product identity and the per-process profile.
//!
//! A *profile* isolates per-user identity: data directories, credential
//! service names, kernel object names, pipes and the window markers used for
//! single-instance activation. Independent client instances with different
//! profiles can therefore run concurrently on one machine (mainly for
//! development). The `default` profile keeps the plain product names; every
//! other profile adds its name as an extra segment.
//!
//! Machine-wide components (Windows service, drivers, component installation)
//! belong to the default profile; their names are only meaningful there.
use anyhow::{Context, Result, bail, ensure};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::OnceLock;
#[cfg(windows)]
use windows::core::PCWSTR;

/// Brand and directory name (`%LOCALAPPDATA%\Farwing`, `Local\Farwing.*`).
pub const PRODUCT_NAME: &str = "Farwing";
/// Full client name for descriptive text.
pub const CLIENT_NAME: &str = "Farwing Client";
/// Lowercase identifier prefix (credential service names and similar keys).
pub const PRODUCT_PREFIX: &str = "farwing";
/// File name of the application image (`%ProgramFiles%\Farwing\Farwing.exe`).
pub const EXECUTABLE_NAME: &str = "Farwing.exe";
/// Machine-wide Windows service (default profile only). Literal copies exist
/// where `w!` needs a string literal; keep them in sync.
pub const SERVICE_NAME: &str = "FarwingHostService";
pub const SERVICE_DISPLAY_NAME: &str = "Farwing Host Service";
/// Inherited by child processes so plugin hosts, agents and relaunches stay in
/// the profile of the process that started them.
pub const PROFILE_ENV: &str = "FARWING_PROFILE";
/// Global command-line option selecting the profile.
pub const PROFILE_ARG: &str = "--profile";
pub const DEFAULT_PROFILE: &str = "default";

static PROFILE: OnceLock<String> = OnceLock::new();

/// Profile names match `^[a-z0-9][a-z0-9-]{0,31}$`; they become parts of
/// paths, object names and pipe names, so the alphabet is deliberately small.
pub fn valid_profile(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=32).contains(&bytes.len())
        && matches!(bytes[0], b'a'..=b'z' | b'0'..=b'9')
        && bytes
            .iter()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}

fn validated(name: &str, source: &str) -> Result<String> {
    ensure!(
        valid_profile(name),
        "配置档名称无效（来自 {source}）：{name:?}；只能包含小写字母、数字和 '-'，以字母或数字开头，最长 32 个字符"
    );
    Ok(name.to_owned())
}

fn resolve(cli_value: Option<&str>) -> Result<String> {
    if let Some(value) = cli_value {
        return validated(value, PROFILE_ARG);
    }
    match std::env::var(PROFILE_ENV) {
        Ok(value) if value.is_empty() => Ok(DEFAULT_PROFILE.to_owned()),
        Ok(value) => validated(&value, PROFILE_ENV),
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_PROFILE.to_owned()),
        Err(std::env::VarError::NotUnicode(_)) => bail!("{PROFILE_ENV} 不是有效的 Unicode 文本"),
    }
}

/// Extracts `--profile <NAME>` / `--profile=<NAME>` from raw process arguments
/// (without the executable path). Used before clap runs, because the profile
/// must be known before bootstrap, logging or any data directory is touched.
/// Scanning stops at `--`.
pub fn profile_arg<I>(args: I) -> Result<Option<String>>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let mut found: Option<String> = None;
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        let value = if arg == PROFILE_ARG {
            args.next().context("--profile 需要一个配置档名称")?
        } else if let Some(value) = arg
            .to_str()
            .and_then(|text| text.strip_prefix(PROFILE_ARG))
            .and_then(|rest| rest.strip_prefix('='))
        {
            OsString::from(value)
        } else {
            continue;
        };
        let value = value
            .into_string()
            .map_err(|_| anyhow::anyhow!("--profile 不是有效的 Unicode 文本"))?;
        if let Some(previous) = &found {
            ensure!(*previous == value, "--profile 只能指定一次");
        }
        found = Some(value);
    }
    Ok(found)
}

/// Resolves the process profile: CLI `--profile`, then `FARWING_PROFILE`, else
/// `default`. The result is pinned for the process lifetime and exported as
/// `FARWING_PROFILE` so every child process inherits it.
///
/// Call at the very beginning of `main`, before any other thread exists.
pub fn init_profile(cli_value: Option<&str>) -> Result<()> {
    let resolved = resolve(cli_value)?;
    let current = PROFILE.get_or_init(|| resolved.clone());
    ensure!(
        *current == resolved,
        "配置档已确定为 {current:?}，不能再切换为 {resolved:?}"
    );
    // SAFETY: called from `main` before any thread is spawned, like the
    // bootstrap environment handling that follows it.
    unsafe {
        std::env::set_var(PROFILE_ENV, current);
    }
    Ok(())
}

/// The active profile. Falls back to the inherited environment (or `default`)
/// when `init_profile` was not called, e.g. in unit tests.
pub fn profile() -> &'static str {
    PROFILE
        .get_or_init(|| resolve(None).unwrap_or_else(|_| DEFAULT_PROFILE.to_owned()))
        .as_str()
}

pub fn is_default_profile() -> bool {
    profile() == DEFAULT_PROFILE
}

/// Product name for user-visible places that must tell profiles apart, such as
/// the notification-area tooltip: `Farwing` or `Farwing (<profile>)`.
pub fn display_name() -> String {
    if is_default_profile() {
        PRODUCT_NAME.to_owned()
    } else {
        format!("{PRODUCT_NAME} ({})", profile())
    }
}

/// URI scheme for toast activation: `farwing-notification` for the default
/// profile and `farwing-<profile>-notification` otherwise.
pub fn notification_scheme() -> String {
    if is_default_profile() {
        format!("{PRODUCT_PREFIX}-notification")
    } else {
        format!("{PRODUCT_PREFIX}-{}-notification", profile())
    }
}

/// Application User Model ID of the toast sender: `Farwing.RemoteAccess` or
/// `Farwing.RemoteAccess.<profile>`.
pub fn aumid() -> String {
    if is_default_profile() {
        format!("{PRODUCT_NAME}.RemoteAccess")
    } else {
        format!("{PRODUCT_NAME}.RemoteAccess.{}", profile())
    }
}

/// Start-menu shortcut carrying the profile's [`aumid`]: `Farwing.lnk` or
/// `Farwing (<profile>).lnk`. One shortcut holds one AUMID, so profiles need
/// distinct files.
pub fn shortcut_file_name() -> String {
    format!("{}.lnk", display_name())
}

/// `%LOCALAPPDATA%`, validated as an absolute directory.
#[cfg(windows)]
pub fn local_app_data() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return Ok(path);
    }
    known_folder(&windows::Win32::UI::Shell::FOLDERID_LocalAppData)
        .context(crate::i18n::t().system.local_app_data_unknown)
}

/// The XDG data directory for the current desktop user.
#[cfg(target_os = "linux")]
pub fn local_app_data() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return Ok(path);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|home| home.join(".local/share"))
        .context(crate::i18n::t().system.local_app_data_unknown)
}

/// Per-user data root, with non-default profiles under `profiles/<name>`.
pub fn local_data_dir() -> Result<PathBuf> {
    let base = local_app_data()?.join(PRODUCT_NAME);
    Ok(if is_default_profile() {
        base
    } else {
        base.join("profiles").join(profile())
    })
}

/// Machine scope `%ProgramData%\Farwing`. Owned by the machine-wide service,
/// so it is only meaningful for the default profile.
#[cfg(windows)]
pub fn program_data_dir() -> Result<PathBuf> {
    Ok(known_folder(&windows::Win32::UI::Shell::FOLDERID_ProgramData)
        .context(crate::i18n::t().system.program_data_unknown)?
        .join(PRODUCT_NAME))
}

#[cfg(windows)]
fn known_folder(id: &windows::core::GUID) -> Result<PathBuf> {
    use windows::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{KF_FLAG_DEFAULT, SHGetKnownFolderPath},
    };
    let raw = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None)? };
    let text = unsafe { raw.to_string() };
    unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    let path = PathBuf::from(text?);
    ensure!(
        path.is_absolute(),
        "{}",
        (crate::i18n::t().system.system_folder_invalid)(&path.display().to_string())
    );
    Ok(path)
}

/// Profile-specific object name: `Farwing.<suffix>` (default profile) or
/// `Farwing.<profile>.<suffix>`.
pub fn object_name(suffix: &str) -> String {
    if is_default_profile() {
        machine_object_name(suffix)
    } else {
        format!("{PRODUCT_NAME}.{}.{suffix}", profile())
    }
}

/// Profile-independent object name `Farwing.<suffix>`, for machine-wide
/// components and for names whose cross-profile sharing is harmless
/// (process-local window classes, clipboard formats).
pub fn machine_object_name(suffix: &str) -> String {
    format!("{PRODUCT_NAME}.{suffix}")
}

/// `Local\` + [`object_name`]: session-local, profile-specific kernel object.
pub fn local_object(suffix: &str) -> String {
    format!("Local\\{}", object_name(suffix))
}

/// `Global\` + [`object_name`]: machine-visible, profile-specific kernel object.
pub fn global_object(suffix: &str) -> String {
    format!("Global\\{}", object_name(suffix))
}

/// `Global\Farwing.<suffix>` regardless of profile, for locks that guard
/// machine-wide state such as component installation.
pub fn machine_global_object(suffix: &str) -> String {
    format!("Global\\{}", machine_object_name(suffix))
}

/// Named pipe path `\\.\pipe\Farwing.<suffix>` / `\\.\pipe\Farwing.<profile>.<suffix>`.
pub fn pipe_name(suffix: &str) -> String {
    format!(r"\\.\pipe\{}", object_name(suffix))
}

/// Credential-store service name `farwing.<suffix>` / `farwing.<profile>.<suffix>`.
pub fn keyring_service(suffix: &str) -> String {
    if is_default_profile() {
        format!("{PRODUCT_PREFIX}.{suffix}")
    } else {
        format!("{PRODUCT_PREFIX}.{}.{suffix}", profile())
    }
}

/// Window class name. Classes are process-local, so they stay profile-independent.
pub fn window_class(suffix: &str) -> String {
    machine_object_name(suffix)
}

/// Window property or registered window message used to find or address
/// another instance. Always profile-specific so profiles never activate,
/// close or update each other.
pub fn instance_atom(suffix: &str) -> String {
    object_name(suffix)
}

/// A null-terminated UTF-16 name built once on first use and kept for the
/// process lifetime, for Win32 APIs and structures that take `PCWSTR`.
///
/// ```ignore
/// static CLASS: WideName = WideName::new(|| product::local_object("Example.v1"));
/// unsafe { CreateMutexW(None, false, CLASS.pcwstr()) };
/// ```
#[cfg(windows)]
pub struct WideName {
    make: fn() -> String,
    cell: OnceLock<Vec<u16>>,
}
#[cfg(windows)]
impl WideName {
    pub const fn new(make: fn() -> String) -> Self {
        Self {
            make,
            cell: OnceLock::new(),
        }
    }
    /// Includes the terminating null.
    pub fn wide(&self) -> &[u16] {
        self.cell
            .get_or_init(|| (self.make)().encode_utf16().chain(Some(0)).collect())
    }
    pub fn pcwstr(&self) -> PCWSTR {
        PCWSTR(self.wide().as_ptr())
    }
}

/// Null-terminated UTF-16 copy of `text` for a single Win32 call.
#[cfg(windows)]
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_names() {
        let (longest, too_long) = ("a".repeat(32), "a".repeat(33));
        for name in ["default", "dev", "a", "0", "dev-2", "a-", longest.as_str()] {
            assert!(valid_profile(name), "{name}");
        }
        for name in ["", "-dev", "Dev", "dev_2", "dev.2", "dev 2", "é", too_long.as_str()] {
            assert!(!valid_profile(name), "{name}");
        }
    }

    #[test]
    fn profile_arguments() {
        let parse = |args: &[&str]| profile_arg(args.iter().map(OsString::from));
        assert_eq!(parse(&["gui"]).unwrap(), None);
        assert_eq!(parse(&["--profile", "dev", "gui"]).unwrap().as_deref(), Some("dev"));
        assert_eq!(parse(&["gui", "--profile=dev"]).unwrap().as_deref(), Some("dev"));
        assert_eq!(parse(&["--", "--profile", "dev"]).unwrap(), None);
        assert!(parse(&["--profile"]).is_err());
        assert!(parse(&["--profile", "a", "--profile", "b"]).is_err());
        assert_eq!(parse(&["--profiles", "x"]).unwrap(), None);
    }
}
