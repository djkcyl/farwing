//! Protected staging and shared filesystem checks for component deployment.
use crate::i18n::t;
use anyhow::{Result, ensure};
use std::path::{Path, PathBuf};
use windows::{
    Win32::{
        Foundation::*, Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT,
        System::Com::CoTaskMemFree, UI::Shell::*,
    },
    core::{PCWSTR, w},
};
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
pub(crate) fn secure_directory(path: &Path) -> Result<()> {
    use windows::Win32::Security::{Authorization::*, *};
    unsafe {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w!("O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;BU)"),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )?;
        let result = (|| -> Result<()> {
            let (mut present, mut defaulted) = (
                windows::core::BOOL::default(),
                windows::core::BOOL::default(),
            );
            let mut dacl = std::ptr::null_mut();
            let mut owner = PSID::default();
            GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)?;
            GetSecurityDescriptorOwner(descriptor, &mut owner, &mut defaulted)?;
            SetNamedSecurityInfoW(
                PCWSTR(wide(&path.to_string_lossy()).as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION
                    | PROTECTED_DACL_SECURITY_INFORMATION
                    | OWNER_SECURITY_INFORMATION,
                Some(owner),
                None,
                Some(dacl),
                None,
            )
            .ok()?;
            Ok(())
        })();
        LocalFree(Some(HLOCAL(descriptor.0)));
        result
    }
}

pub(crate) fn reject_reparse(path: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    if path.exists() {
        ensure!(
            std::fs::symlink_metadata(path)?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0
                == 0,
            "{}",
            t().system.component_directory_reparse
        );
    }
    Ok(())
}
pub(crate) struct Staging {
    pub path: PathBuf,
    files: Vec<PathBuf>,
}
impl Staging {
    pub fn new(entries: &[(&str, &[u8])]) -> Result<Self> {
        let base = unsafe {
            let raw = SHGetKnownFolderPath(&FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, None)?;
            let value = raw.to_string();
            CoTaskMemFree(Some(raw.0.cast()));
            PathBuf::from(value?)
        };
        reject_reparse(&base)?;
        let root = base.join(format!("{}Components", crate::product::PRODUCT_NAME));
        reject_reparse(&root)?;
        std::fs::create_dir_all(&root)?;
        secure_directory(&root)?;
        let path = root.join(format!("package-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path)?;
        let mut stage = Self {
            path,
            files: Vec::new(),
        };
        for &(name, bytes) in entries {
            ensure!(
                Path::new(name).file_name().is_some_and(|n| n == name),
                "{}",
                t().system.component_invalid_package_file_name
            );
            let path = stage.path.join(name);
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)?;
            stage.files.push(path);
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        Ok(stage)
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        for path in &self.files {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir(&self.path);
    }
}
