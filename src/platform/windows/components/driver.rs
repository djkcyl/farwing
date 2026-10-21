//! Common PnP package lifecycle. Backends supply identity and occupancy checks.
use super::{Kind, Status, files::Staging};
use crate::i18n::t;
use anyhow::{Context, Result, ensure};
use std::{
    mem::size_of,
    path::{Path, PathBuf},
};
use windows::{
    Win32::{
        Devices::DeviceAndDriverInstallation::*, Foundation::*, Security::WinTrust::*,
        System::Registry::*,
    },
    core::{GUID, PCWSTR},
};

pub(crate) struct Package {
    pub kind: Kind,
    pub class: GUID,
    pub hardware: &'static str,
    pub node: &'static str,
    pub description: &'static str,
    pub inf: &'static str,
    pub catalog: &'static str,
    pub files: &'static [(&'static str, &'static [u8])],
    pub repair: bool,
}
/// The driver was installed but its device did not start.
#[derive(Debug)]
pub(crate) struct NotStarted {
    /// The device's PnP problem code (0 when it simply has not started).
    pub problem: u32,
    reason: String,
}
impl NotStarted {
    /// Windows refused the driver's signature (code 52).
    pub fn unsigned(&self) -> bool {
        self.problem == CM_PROB_UNSIGNED_DRIVER.0
    }
}
impl std::fmt::Display for NotStarted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&(t().system.driver_not_started)(&self.reason))
    }
}
impl std::error::Error for NotStarted {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
struct DeviceSet(HDEVINFO);
impl Drop for DeviceSet {
    fn drop(&mut self) {
        let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}
fn property(
    set: &DeviceSet,
    device: &SP_DEVINFO_DATA,
    property: SETUP_DI_REGISTRY_PROPERTY,
) -> Result<Vec<String>> {
    let mut bytes = vec![0u8; 65536];
    let mut needed = 0;
    if let Err(error) = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            set.0,
            device,
            property,
            None,
            Some(&mut bytes),
            Some(&mut needed),
        )
    } {
        if error.code() == ERROR_INVALID_DATA.to_hresult() {
            return Ok(Vec::new());
        }
        return Err(error.into());
    }
    ensure!(
        needed as usize <= bytes.len() && needed % 2 == 0,
        "{}",
        t().system.driver_invalid_device_property
    );
    let chars: Vec<_> = bytes[..needed as usize]
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    Ok(chars
        .split(|c| *c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect())
}
fn inf_directory() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("SystemRoot").context(t().system.driver_windows_directory_unavailable)?,
    )
    .join("INF"))
}
fn oem_inf(name: &str) -> bool {
    name.to_ascii_lowercase()
        .strip_prefix("oem")
        .and_then(|s| s.strip_suffix(".inf"))
        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}
fn bound_package(set: &DeviceSet, device: &SP_DEVINFO_DATA) -> Result<String> {
    let key = unsafe {
        SetupDiOpenDevRegKey(
            set.0,
            device,
            DICS_FLAG_GLOBAL.0,
            0,
            DIREG_DRV,
            KEY_QUERY_VALUE.0,
        )
    }?;
    let mut name = [0u16; 512];
    let mut bytes = std::mem::size_of_val(&name) as u32;
    let result = unsafe {
        RegGetValueW(
            key,
            None,
            windows::core::w!("InfPath"),
            RRF_RT_REG_SZ,
            None,
            Some(name.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    let _ = unsafe { RegCloseKey(key) };
    result.ok()?;
    let end = name
        .iter()
        .position(|c| *c == 0)
        .context(t().system.driver_invalid_package_name)?;
    let name = String::from_utf16_lossy(&name[..end]);
    ensure!(oem_inf(&name), "{}", t().system.driver_not_oem_package);
    Ok(name)
}
fn store_inf(name: &str) -> Result<PathBuf> {
    ensure!(oem_inf(name), "{}", t().system.driver_invalid_package_name);
    let mut path = [0u16; 32768];
    unsafe {
        SetupGetInfDriverStoreLocationW(
            PCWSTR(wide(&inf_directory()?.join(name).to_string_lossy()).as_ptr()),
            None,
            None,
            &mut path,
            None,
        )?;
    }
    let end = path
        .iter()
        .position(|c| *c == 0)
        .context(t().system.driver_invalid_store_path)?;
    Ok(PathBuf::from(String::from_utf16_lossy(&path[..end])))
}
fn verify_catalog(path: &Path) -> Result<()> {
    let path = wide(&path.to_string_lossy());
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(path.as_ptr()),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_WHOLECHAIN,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
        dwStateAction: WTD_STATEACTION_VERIFY,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let status = unsafe {
        WinVerifyTrust(
            HWND(-1isize as *mut _),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        )
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe {
        WinVerifyTrust(
            HWND(-1isize as *mut _),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        );
    }
    ensure!(
        status == 0,
        "{}",
        (t().system.driver_signature_check_failed)(&format!("{status:#x}"))
    );
    Ok(())
}
fn unavailable_reason(flags: CM_DEVNODE_STATUS_FLAGS, problem: CM_PROB) -> Option<String> {
    if problem == CM_PROB_UNSIGNED_DRIVER {
        Some(t().system.driver_unsigned.into())
    } else if problem == CM_PROB_DISABLED {
        Some(t().system.driver_device_disabled.into())
    } else if problem == CM_PROB_NEED_RESTART {
        Some(t().system.driver_restart_required.into())
    } else if problem.0 != 0 {
        Some((t().system.driver_not_ready)(problem.0))
    } else if !flags.contains(DN_STARTED) {
        Some(t().system.driver_installed_not_started.into())
    } else {
        None
    }
}
impl Package {
    fn nodes(&self) -> Result<(DeviceSet, Vec<SP_DEVINFO_DATA>)> {
        let set = DeviceSet(unsafe {
            SetupDiGetClassDevsW(
                Some(&self.class),
                None,
                None,
                SETUP_DI_GET_CLASS_DEVS_FLAGS(0),
            )
        }?);
        let mut nodes = Vec::new();
        for index in 0..16384 {
            let mut info = SP_DEVINFO_DATA {
                cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if let Err(error) = unsafe { SetupDiEnumDeviceInfo(set.0, index, &mut info) } {
                if error.code() == ERROR_NO_MORE_ITEMS.to_hresult() {
                    return Ok((set, nodes));
                }
                return Err(error.into());
            }
            if property(&set, &info, SPDRP_HARDWAREID)?
                .iter()
                .any(|s| s.eq_ignore_ascii_case(self.hardware))
            {
                nodes.push(info);
            }
        }
        anyhow::bail!("{}", t().system.driver_device_list_too_long)
    }
    /// Whether this build embeds the signed package (see build.rs).
    pub fn bundled(&self) -> bool {
        !self.files.is_empty()
    }
    fn matches_inf(&self, path: &Path) -> Result<bool> {
        if !self.bundled() {
            return Ok(false);
        }
        let expected = self
            .files
            .iter()
            .find(|(name, _)| *name == self.inf)
            .context(t().system.driver_package_missing_inf)?
            .1;
        let bytes = std::fs::read(path)?;
        // Ownership survives a driver version change; all other INF contents
        // (hardware ID, provider, service, files and security) must still match.
        let identity = |text: &str| {
            text.lines()
                .filter(|line| {
                    !line
                        .split_once('=')
                        .is_some_and(|(key, _)| key.trim().eq_ignore_ascii_case("DriverVer"))
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let text = |bytes: &[u8]| -> Option<String> {
            if let Some(bytes) = bytes.strip_prefix(&[0xff, 0xfe]) {
                if !bytes.len().is_multiple_of(2) {
                    return None;
                }
                String::from_utf16(
                    &bytes
                        .chunks_exact(2)
                        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                        .collect::<Vec<_>>(),
                )
                .ok()
            } else {
                std::str::from_utf8(bytes).ok().map(str::to_owned)
            }
        };
        Ok(bytes == expected
            || match (text(&bytes), text(expected)) {
                (Some(a), Some(b)) => identity(&a) == identity(&b),
                _ => false,
            })
    }
    fn current_package(&self, name: &str) -> Result<bool> {
        if !self.bundled() {
            return Ok(false);
        }
        let inf = store_inf(name)?;
        if !inf
            .file_name()
            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(self.inf))
        {
            return Ok(false);
        }
        let directory = inf
            .parent()
            .context(t().system.driver_invalid_store_directory)?;
        for (name, expected) in self.files {
            if std::fs::read(directory.join(name))? != *expected {
                return Ok(false);
            }
        }
        Ok(true)
    }
    fn matching_packages(&self) -> Result<Vec<String>> {
        let mut result = Vec::new();
        for entry in std::fs::read_dir(inf_directory()?)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if oem_inf(&name) && self.matches_inf(&entry.path())? {
                result.push(name);
            }
        }
        Ok(result)
    }
    /// Automatic application upgrades replace an outdated owned package. A
    /// current package rejected by this Windows boot is not a package update.
    pub fn needs_package_update(&self) -> Result<bool> {
        let (set, nodes) = self.nodes()?;
        ensure!(
            nodes.len() <= 1,
            "{}",
            (t().system.driver_multiple_devices)(self.kind.label())
        );
        let Some(device) = nodes.first() else {
            return Ok(false);
        };
        let bound = bound_package(&set, device)?;
        if !self.matches_inf(&inf_directory()?.join(&bound))? {
            return Ok(false);
        }
        Ok(!self.current_package(&bound)?)
    }
    /// Read only the matching device's runtime state after an interface-open
    /// failure. Do not scan the driver store or compare embedded package files
    /// on an audio request, and do not infer installation state from CreateFile.
    pub fn unavailable_reason(&self) -> Result<Option<String>> {
        let (_set, nodes) = self.nodes()?;
        ensure!(
            nodes.len() <= 1,
            "{}",
            (t().system.driver_multiple_devices)(self.kind.label())
        );
        let Some(device) = nodes.first() else {
            return Ok(Some(t().system.driver_device_not_installed.into()));
        };
        let (mut flags, mut problem) = (CM_DEVNODE_STATUS_FLAGS(0), CM_PROB(0));
        let result = unsafe { CM_Get_DevNode_Status(&mut flags, &mut problem, device.DevInst, 0) };
        ensure!(
            result == CR_SUCCESS,
            "{}",
            (t().system.driver_status_read_failed)(&format!("{result:?}"))
        );
        Ok(unavailable_reason(flags, problem))
    }
    pub fn status(&self) -> Result<Status> {
        let (set, nodes) = self.nodes()?;
        ensure!(
            nodes.len() <= 1,
            "{}",
            (t().system.driver_multiple_devices)(self.kind.label())
        );
        let packages = self.matching_packages()?;
        let Some(device) = nodes.first() else {
            return Ok(Status {
                label: if !self.bundled() {
                    t().system.driver_not_bundled
                } else if packages.is_empty() {
                    t().system.install_not_installed
                } else {
                    t().system.driver_package_left
                }
                .into(),
                installed: false,
                removable: !packages.is_empty(),
                ready: false,
            });
        };
        let bound = bound_package(&set, device).ok();
        let owned = bound
            .as_ref()
            .is_some_and(|bound| packages.iter().any(|p| p.eq_ignore_ascii_case(bound)));
        let current = owned
            && bound
                .as_ref()
                .is_some_and(|name| self.current_package(name).unwrap_or(false));
        let (mut flags, mut problem) = (CM_DEVNODE_STATUS_FLAGS(0), CM_PROB(0));
        let result = unsafe { CM_Get_DevNode_Status(&mut flags, &mut problem, device.DevInst, 0) };
        ensure!(
            result == CR_SUCCESS,
            "{}",
            (t().system.driver_status_read_failed)(&format!("{result:?}"))
        );
        Ok(Status {
            label: if !owned {
                t().system.driver_other_version.into()
            } else if !current {
                t().system.install_needs_update.into()
            } else {
                unavailable_reason(flags, problem)
                    .unwrap_or_else(|| t().system.install_ready.into())
            },
            installed: true,
            removable: owned,
            ready: current && problem.0 == 0 && flags.contains(DN_STARTED),
        })
    }
    pub fn install(&self) -> Result<bool> {
        ensure!(
            self.bundled(),
            "{}",
            (t().system.driver_package_not_bundled)(self.kind.label())
        );
        let (set, nodes) = self.nodes()?;
        ensure!(
            nodes.len() <= 1,
            "{}",
            (t().system.driver_multiple_devices)(self.kind.label())
        );
        let created = nodes.is_empty();
        if let Some(device) = nodes.first() {
            ensure!(
                self.repair,
                "{}",
                (t().system.driver_device_exists)(self.kind.label())
            );
            ensure!(
                self.matches_inf(&inf_directory()?.join(bound_package(&set, device)?))?,
                "{}",
                t().system.driver_not_owned
            );
        }
        let stage = Staging::new(self.files).context(t().system.driver_stage_failed)?;
        super::trust_certificate(self.kind).context(t().system.driver_trust_certificate_failed)?;
        verify_catalog(&stage.path.join(self.catalog))
            .context(t().system.driver_catalog_check_failed)?;
        let mut info = nodes.first().copied().unwrap_or(SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        });
        if created {
            unsafe {
                SetupDiCreateDeviceInfoW(
                    set.0,
                    PCWSTR(wide(self.node).as_ptr()),
                    &self.class,
                    PCWSTR(wide(self.description).as_ptr()),
                    None,
                    DICD_GENERATE_ID,
                    Some(&mut info),
                )?;
                let hardware: Vec<u8> = self
                    .hardware
                    .encode_utf16()
                    .chain([0, 0])
                    .flat_map(u16::to_le_bytes)
                    .collect();
                SetupDiSetDeviceRegistryPropertyW(
                    set.0,
                    &mut info,
                    SPDRP_HARDWAREID,
                    Some(&hardware),
                )?;
                SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set.0, Some(&info))?;
            }
        }
        let mut reboot = windows::core::BOOL::default();
        if let Err(error) = unsafe {
            UpdateDriverForPlugAndPlayDevicesW(
                None,
                PCWSTR(wide(self.hardware).as_ptr()),
                PCWSTR(wide(&stage.path.join(self.inf).to_string_lossy()).as_ptr()),
                if created {
                    UPDATEDRIVERFORPLUGANDPLAYDEVICES_FLAGS(0)
                } else {
                    INSTALLFLAG_FORCE
                },
                Some(&mut reboot),
            )
        } {
            if created {
                let _ = unsafe { SetupDiCallClassInstaller(DIF_REMOVE, set.0, Some(&info)) };
            }
            return Err(error.into());
        }
        if !reboot.as_bool() {
            let (mut flags, mut problem) = (CM_DEVNODE_STATUS_FLAGS(0), CM_PROB(0));
            let result =
                unsafe { CM_Get_DevNode_Status(&mut flags, &mut problem, info.DevInst, 0) };
            ensure!(
                result == CR_SUCCESS,
                "{}",
                (t().system.driver_status_read_failed)(&format!("{result:?}"))
            );
            if let Some(reason) = unavailable_reason(flags, problem) {
                return Err(NotStarted {
                    problem: problem.0,
                    reason,
                }
                .into());
            }
            ensure!(
                self.current_package(&bound_package(&set, &info)?)?,
                "{}",
                t().system.driver_old_package_in_use
            );
        }
        Ok(reboot.as_bool())
    }
    pub fn removal(&self) -> Result<Removal> {
        let (set, nodes) = self.nodes()?;
        ensure!(
            nodes.len() <= 1,
            "{}",
            (t().system.driver_multiple_devices)(self.kind.label())
        );
        let packages = self.matching_packages()?;
        if let Some(device) = nodes.first() {
            let bound = bound_package(&set, device)?;
            ensure!(
                packages.iter().any(|p| p.eq_ignore_ascii_case(&bound)),
                "{}",
                t().system.driver_not_owned
            );
        }
        Ok(Removal {
            set,
            device: nodes.into_iter().next(),
            packages,
        })
    }
}
pub(crate) struct Removal {
    set: DeviceSet,
    device: Option<SP_DEVINFO_DATA>,
    packages: Vec<String>,
}
impl Removal {
    pub fn instance(&self) -> Result<Option<String>> {
        let Some(device) = self.device.as_ref() else {
            return Ok(None);
        };
        let mut id = [0u16; 512];
        unsafe {
            SetupDiGetDeviceInstanceIdW(self.set.0, device, Some(&mut id), None)?;
        }
        let end = id
            .iter()
            .position(|c| *c == 0)
            .context(t().system.driver_invalid_device_id)?;
        Ok(Some(String::from_utf16_lossy(&id[..end])))
    }
    pub fn execute(self) -> Result<bool> {
        let Self {
            set,
            device,
            packages,
        } = self;
        let mut reboot = windows::core::BOOL::default();
        if let Some(info) = &device {
            unsafe {
                DiUninstallDevice(HWND::default(), set.0, info, 0, Some(&mut reboot))?;
            }
        }
        drop(set);
        if reboot.as_bool() {
            return Ok(true);
        }
        for name in &packages {
            unsafe { SetupUninstallOEMInfW(PCWSTR(wide(name).as_ptr()), 0, None) }
                .ok()
                .with_context(|| (t().system.driver_package_in_use)(name))?;
        }
        Ok(false)
    }
}
