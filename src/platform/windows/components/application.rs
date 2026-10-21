//! Installed application ownership. Services consume this image; they do not own it.
use super::files::{reject_reparse, secure_directory};
use crate::i18n::t;
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use windows::{
    Win32::{System::Com::CoTaskMemFree, UI::Shell::*},
    core::GUID,
};

mod data;
pub(super) mod integration;
pub(crate) fn purge_machine_data() -> Result<()> {
    data::machine()
}
/// Content of `owner.txt`, marking the installation directory as ours.
/// `packaging` (the launcher) checks the same text before using the copy.
pub(crate) const RECEIPT: &str = "Farwing deployment v1\n";

pub(super) fn folder(id: &GUID) -> Result<PathBuf> {
    unsafe {
        let raw = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None)?;
        let text = raw.to_string();
        CoTaskMemFree(Some(raw.0.cast()));
        Ok(PathBuf::from(text?))
    }
}
/// Windows of other profiles (or viewers) still run the installed image;
/// the file cannot be removed until they exit.
#[derive(Debug, thiserror::Error)]
#[error("{}", (crate::i18n::t().system.component_image_in_use)(.0))]
pub(crate) struct ImageInUse(pub String);

/// User processes running the installed image. The service, its resident and
/// agents (SYSTEM) are stopped by the operation itself.
fn ensure_image_unused(image: &Path) -> Result<()> {
    use windows::Win32::{
        Foundation::ERROR_NO_MORE_FILES,
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
    };
    let image = std::fs::canonicalize(image)?;
    let name = image
        .file_name()
        .context(t().system.component_invalid_install_file_name)?
        .to_string_lossy()
        .to_string();
    let own = std::process::id();
    let snapshot = crate::platform::windows::host_service::pipe::Handle(unsafe {
        CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?
    });
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut busy = Vec::new();
    let mut result = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    while result.is_ok() {
        let len = entry.szExeFile.iter().position(|c| *c == 0).unwrap_or(entry.szExeFile.len());
        let pid = entry.th32ProcessID;
        if pid != own
            && String::from_utf16_lossy(&entry.szExeFile[..len]).eq_ignore_ascii_case(&name)
            && crate::platform::windows::host_service::process::image(pid)
                .and_then(|p| Ok(std::fs::canonicalize(p)?))
                .is_ok_and(|p| p == image)
            && crate::platform::windows::host_service::vault::sid(pid).is_ok_and(|sid| sid != "S-1-5-18")
        {
            busy.push(pid.to_string());
        }
        result = unsafe { Process32NextW(snapshot.0, &mut entry) };
    }
    if let Err(e) = result {
        ensure!(
            e.code() == ERROR_NO_MORE_FILES.to_hresult(),
            "{}",
            (t().system.component_running_check_failed)(&e.to_string())
        );
    }
    if busy.is_empty() {
        Ok(())
    } else {
        Err(ImageInUse(busy.join(t().system.component_list_separator)).into())
    }
}
/// `%ProgramFiles%\Farwing`. There is no other installation layout.
pub(crate) fn directory() -> Result<PathBuf> {
    Ok(folder(&FOLDERID_ProgramFiles)?.join(crate::product::PRODUCT_NAME))
}
pub(crate) fn image() -> Result<PathBuf> {
    Ok(directory()?.join(crate::product::EXECUTABLE_NAME))
}
pub(crate) fn verify_directory(directory: &Path) -> Result<()> {
    ensure!(
        directory == self::directory()?,
        "{}",
        t().system.component_invalid_install_directory
    );
    reject_reparse(
        directory
            .parent()
            .context(t().system.component_invalid_install_directory)?,
    )?;
    reject_reparse(directory)?;
    ensure!(
        std::fs::read_to_string(directory.join("owner.txt"))? == RECEIPT,
        "{}",
        t().system.component_install_directory_not_owned
    );
    Ok(())
}

/// Keep the prior image and metadata until SCM configuration has succeeded.
pub(crate) struct Deployment {
    directory: PathBuf,
    backup: Option<PathBuf>,
    previous_images: Option<Vec<u8>>,
    created: bool,
    committed: bool,
    image_written: bool,
}
struct StagedImage(PathBuf);
impl Drop for StagedImage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
impl Deployment {
    pub(crate) fn prepare() -> Result<Self> {
        let directory = directory()?;
        let created = !directory.join("owner.txt").exists();
        reject_reparse(
            directory
                .parent()
                .context(t().system.component_invalid_install_directory)?,
        )?;
        reject_reparse(&directory)?;
        if created {
            ensure!(
                !directory.exists() || std::fs::read_dir(&directory)?.next().is_none(),
                "{}",
                t().system.component_install_directory_not_empty
            );
        } else {
            verify_directory(&directory)?;
        }
        std::fs::create_dir_all(&directory)?;
        secure_directory(&directory)?;
        let mut transaction = Self {
            previous_images: std::fs::read(directory.join("images.json")).ok(),
            directory,
            backup: None,
            created,
            committed: false,
            image_written: false,
        };
        if created {
            std::fs::write(transaction.directory.join("owner.txt"), RECEIPT)?;
        }
        let image = transaction.directory.join(crate::product::EXECUTABLE_NAME);
        reject_reparse(&image)?;
        let staged = transaction
            .directory
            .join(format!("staged-{}.exe", uuid::Uuid::new_v4().simple()));
        let _staged_cleanup = StagedImage(staged.clone());
        std::fs::copy(std::env::current_exe()?, &staged)?;
        if crate::platform::windows::host_service::process::image_hash(&staged)?
            != crate::platform::windows::host_service::process::image_hash(
                &std::env::current_exe()?
            )?
        {
            let _ = std::fs::remove_file(&staged);
            anyhow::bail!("{}", t().system.component_copy_verification_failed);
        }
        if image.exists() {
            let backup = transaction
                .directory
                .join(format!("retired-{}.exe", uuid::Uuid::new_v4().simple()));
            if let Err(error) = std::fs::rename(&image, &backup) {
                let _ = std::fs::remove_file(staged);
                return Err(error.into());
            }
            transaction.backup = Some(backup);
        }
        if let Err(error) = std::fs::rename(&staged, &image) {
            let _ = std::fs::remove_file(staged);
            return Err(error.into());
        }
        transaction.image_written = true;
        Ok(transaction)
    }
    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}
impl Drop for Deployment {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let image = self.directory.join(crate::product::EXECUTABLE_NAME);
        if let Some(backup) = &self.backup {
            let _ = std::fs::remove_file(&image);
            let _ = std::fs::rename(backup, &image);
        } else if self.image_written {
            let _ = std::fs::remove_file(&image);
        }
        if let Some(bytes) = &self.previous_images {
            let _ = std::fs::write(self.directory.join("images.json"), bytes);
        } else {
            let _ = std::fs::remove_file(self.directory.join("images.json"));
        }
        if self.created {
            for name in ["owner.txt", "suite.json", "sas-policy.json", "images.json"] {
                let _ = std::fs::remove_file(self.directory.join(name));
            }
            let _ = std::fs::remove_dir(&self.directory);
        }
    }
}
pub(crate) fn cleanup_retired(directory: &Path) -> Result<()> {
    verify_directory(directory)?;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name
            .strip_prefix("retired-")
            .and_then(|s| s.strip_suffix(".exe"))
            .is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            reject_reparse(&entry.path())?;
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}

pub(crate) fn needs_handoff() -> Result<bool> {
    // The installed program belongs to the default profile; other profiles
    // run portable, from whichever copy was started.
    if !crate::product::is_default_profile() {
        return Ok(false);
    }
    let image = image()?;
    if !image.is_file() {
        return Ok(false);
    }
    verify_directory(
        image
            .parent()
            .context(t().system.component_invalid_install_path)?,
    )?;
    Ok(std::fs::canonicalize(std::env::current_exe()?)? != std::fs::canonicalize(image)?)
}
pub(crate) fn installed_version() -> Result<String> {
    use std::{
        io::Read,
        os::windows::process::CommandExt,
        process::Stdio,
        time::{Duration, Instant},
    };
    let image = image()?;
    verify_directory(
        image
            .parent()
            .context(t().system.component_invalid_install_path)?,
    )?;
    // --version exits in clap, before logging, account initialization or GUI.
    let mut child = std::process::Command::new(image)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(3) {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("{}", t().system.component_version_timeout);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    ensure!(status.success(), "{}", t().system.component_version_failed);
    let mut output = String::new();
    child
        .stdout
        .take()
        .context(t().system.component_version_output_missing)?
        .take(256)
        .read_to_string(&mut output)?;
    let version = output
        .trim()
        .strip_prefix(crate::product::PRODUCT_NAME)
        .and_then(|rest| rest.strip_prefix(' '))
        .context(t().system.component_invalid_installed_version)?;
    ensure!(
        !version.is_empty() && version.len() <= 80 && !version.chars().any(char::is_control),
        "{}",
        t().system.component_invalid_installed_version
    );
    Ok(version.to_owned())
}
pub(crate) fn start_installed(
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let image = image()?;
    verify_directory(
        image
            .parent()
            .context(t().system.component_invalid_install_path)?,
    )?;
    ensure!(
        image.is_file(),
        "{}",
        t().system.component_installed_image_missing
    );
    let child = std::process::Command::new(&image)
        .args(arguments)
        .current_dir(image.parent().unwrap())
        .creation_flags(0x08000000)
        .spawn()
        .context(t().system.component_start_installed_failed)?;
    // Hand off foreground permission while the launching UI is still alive.
    // Only this child receives it; services and unrelated processes do not.
    if let Err(error) =
        unsafe { windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(child.id()) }
    {
        tracing::debug!(%error, "foreground launch permission unavailable");
    }
    Ok(())
}
pub(crate) fn integrate_user() -> Result<()> {
    integration::shortcuts(true)?;
    integration::refresh_icons();
    Ok(())
}
pub(crate) fn register() -> Result<()> {
    integration::register(true)
}

pub(crate) fn request_uninstall(removal: super::RemovalOptions) -> Result<bool> {
    use crate::platform::windows::host_service::{startup, vault};
    if let Some(owner) = vault::owner()? {
        ensure!(
            owner == vault::sid(std::process::id())?,
            "{}",
            t().system.component_uninstall_owner_only
        );
        if !removal.remove_data {
            crate::account::auth::restore_portable()?;
        }
    }
    let notification_image = image()?;
    let args = format!("component application uninstall{}", removal.arguments());
    let reboot = super::elevate(&args, super::Kind::Application)?;
    startup::set(false)?;
    integration::shortcuts(false)?;
    crate::platform::windows::notifications::unregister(&notification_image)?;
    if removal.remove_data && !reboot {
        data::user()?;
    }
    Ok(reboot)
}
pub(crate) fn uninstall(removal: super::RemovalOptions) -> Result<bool> {
    use crate::platform::windows::{host_service, input};
    let directory = directory()?;
    removal.preflight()?;
    crate::platform::windows::virtual_audio::Defaults::recover_defaults()
        .context(t().system.component_restore_audio_before_uninstall_failed)?;
    if !directory.exists() {
        // Allow a failed data-cleanup step to be retried after the application
        // image has already been removed. All component removals remain scoped.
        ensure!(
            !host_service::install::status()?.installed,
            "{}",
            t().system.component_service_directory_missing
        );
        let mut reboot = removal.remove_optional_drivers()?;
        reboot |= input::install::uninstall()?;
        integration::register(false)?;
        return Ok(reboot);
    }
    verify_directory(&directory)?;
    // Before any component goes: a running image would stop the removal
    // half-way, after the drivers and the service were already removed.
    ensure_image_unused(&directory.join(crate::product::EXECUTABLE_NAME))?;
    let reboot = if directory.join("suite.json").is_file() {
        super::suite::execute(super::Operation::Uninstall, None, false, removal)?
    } else {
        host_service::install::preflight()?;
        host_service::install::stop_for_maintenance()?;
        let mut reboot = removal.remove_optional_drivers()?;
        reboot |= input::install::uninstall()?;
        reboot | host_service::install::uninstall()?
    };
    ensure!(!reboot, "{}", t().system.component_removal_needs_restart);
    remove_directory(&directory)?;
    integration::register(false)?;
    Ok(false)
}
fn remove_directory(directory: &Path) -> Result<()> {
    verify_directory(directory)?;
    cleanup_retired(directory)?;
    for name in [
        crate::product::EXECUTABLE_NAME,
        "images.json",
        "resident.pid",
        "suite.json",
        "sas-policy.json",
    ] {
        let file = directory.join(name);
        reject_reparse(&file)?;
        if file.exists() {
            std::fs::remove_file(&file).with_context(|| {
                (t().system.component_remove_file_failed)(&file.display().to_string())
            })?;
        }
    }
    std::fs::remove_file(directory.join("owner.txt"))?;
    if std::fs::read_dir(directory)?.next().is_none() {
        std::fs::remove_dir(directory)?;
    }
    Ok(())
}
