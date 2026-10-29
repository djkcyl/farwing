//! Privileged helper lifecycle. Executable identity and active session are checked
//! before attaching a channel; children belong to a kill-on-close job.
use super::pipe::Handle;
use crate::i18n::t;
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::time::Duration;
use windows::{
    Win32::{
        Foundation::*,
        Security::*,
        System::{JobObjects::*, RemoteDesktop::*, SystemServices::*, Threading::*},
    },
    core::{PCWSTR, PWSTR, w},
};

pub(crate) fn session(pid: u32) -> Result<u32> {
    let mut session = 0;
    unsafe {
        ProcessIdToSessionId(pid, &mut session)?;
    }
    Ok(session)
}
/// Whether a user is signed in to `session`; overlays never draw over the
/// logon screen.
pub(crate) fn logged_on(session: u32) -> bool {
    use windows::{Win32::System::RemoteDesktop::*, core::PWSTR};
    if session == u32::MAX {
        return false;
    }
    unsafe {
        let mut text = PWSTR::null();
        let mut bytes = 0;
        if WTSQuerySessionInformationW(None, session, WTSUserName, &mut text, &mut bytes).is_err() {
            return false;
        }
        let available = !text.is_null() && bytes > 2 && *text.0 != 0;
        if !text.is_null() {
            WTSFreeMemory(text.0.cast());
        }
        available
    }
}

pub(crate) fn active_session() -> u32 {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<(std::time::Instant, u32)>>> =
        std::sync::OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if let Some((when, session)) = *cache
        && when.elapsed() < std::time::Duration::from_millis(100)
    {
        return session;
    }
    let session = query_active_session();
    *cache = Some((std::time::Instant::now(), session));
    session
}
fn query_active_session() -> u32 {
    unsafe {
        let console = WTSGetActiveConsoleSessionId();
        let mut sessions = std::ptr::null_mut();
        let mut count = 0;
        if WTSEnumerateSessionsW(None, 0, 1, &mut sessions, &mut count).is_err() {
            return console;
        }
        let selected = if !sessions.is_null() && count <= 4096 {
            let rows = std::slice::from_raw_parts(sessions, count as usize);
            rows.iter()
                .find(|s| s.State == WTSActive && s.SessionId == console)
                .or_else(|| rows.iter().find(|s| s.State == WTSActive))
                .map(|s| s.SessionId)
                .unwrap_or(console)
        } else {
            console
        };
        if !sessions.is_null() {
            WTSFreeMemory(sessions.cast());
        }
        selected
    }
}
pub(crate) fn image(pid: u32) -> Result<std::path::PathBuf> {
    let process = Handle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)? });
    let mut path = vec![0u16; 32768];
    let mut size = path.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut size,
        )?;
    }
    Ok(String::from_utf16(&path[..size as usize])?.into())
}
pub(crate) fn verify_image(pid: u32) -> Result<()> {
    verify_path(image(pid)?)
}
pub(crate) fn verify_path(path: std::path::PathBuf) -> Result<()> {
    ensure!(
        image_hash(&path)? == image_hash(&std::env::current_exe()?)?,
        "{}",
        t().system.service_image_mismatch
    );
    Ok(())
}
/// SHA-256 of an executable. Each file version is read once per process: the
/// open handle's volume, file index, size and write time key the cache, so a
/// replaced or rewritten file is hashed again.
pub(crate) fn image_hash(path: &std::path::Path) -> Result<String> {
    use std::io::Read;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    static HASHES: std::sync::Mutex<Vec<([u32; 6], String)>> = std::sync::Mutex::new(Vec::new());
    let mut file = std::fs::File::open(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info)? };
    let key = [
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
        info.nFileSizeLow,
        info.ftLastWriteTime.dwHighDateTime,
        info.ftLastWriteTime.dwLowDateTime,
    ];
    let hashes = || HASHES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, hash)) = hashes().iter().find(|(known, _)| *known == key) {
        return Ok(hash.clone());
    }
    let mut digest = Sha256::new();
    let mut bytes = [0; 65536];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        digest.update(&bytes[..n]);
    }
    let hash = format!("{:x}", digest.finalize());
    hashes().push((key, hash.clone()));
    Ok(hash)
}
fn privilege(name: PCWSTR) -> Result<()> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )?;
    }
    let token = Handle(token);
    let mut luid = LUID::default();
    unsafe {
        LookupPrivilegeValueW(None, name, &mut luid)?;
    }
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    unsafe {
        AdjustTokenPrivileges(token.0, false, Some(&privileges), 0, None, None)?;
        ensure!(
            GetLastError() != ERROR_NOT_ALL_ASSIGNED,
            "{}",
            t().system.service_missing_privilege
        );
    }
    Ok(())
}
pub(crate) struct Agent {
    pub process: Handle,
    pub pid: u32,
    _job: Handle,
    stop: Option<Handle>,
}
impl Agent {
    pub fn start_user(session: u32) -> Result<Self> {
        Self::start_role(session, "", "user-background")
    }
    pub fn start(session: u32, pipe: &str) -> Result<Self> {
        Self::start_role(session, pipe, "input-agent")
    }
    pub fn start_capture(session: u32, pipe: &str) -> Result<Self> {
        Self::start_role(session, pipe, "capture-agent")
    }
    /// The background host: LocalService with the service's own SID
    /// (`as_service`), or SYSTEM.
    pub fn start_resident(session: u32, as_service: bool) -> Result<Self> {
        if as_service {
            Self::start_host(session)
        } else {
            Self::start_role(session, "", "host-resident")
        }
    }
    pub fn stop_gracefully(self) {
        if let Some(stop) = &self.stop {
            unsafe {
                let _ = SetEvent(stop.0);
                let _ = WaitForSingleObject(self.process.0, 8000);
            }
        }
    }
    fn start_role(session: u32, pipe: &str, role: &str) -> Result<Self> {
        ensure!(
            session != u32::MAX,
            "{}",
            t().system.service_no_windows_session
        );
        privilege(w!("SeTcbPrivilege"))?;
        privilege(w!("SeAssignPrimaryTokenPrivilege"))?;
        privilege(w!("SeIncreaseQuotaPrivilege"))?;
        let primary = if role == "user-background" {
            let mut token = HANDLE::default();
            unsafe {
                WTSQueryUserToken(session, &mut token)?;
            }
            Handle(token)
        } else {
            let mut token = HANDLE::default();
            unsafe {
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_DUPLICATE | TOKEN_QUERY,
                    &mut token,
                )?;
            }
            let token = Handle(token);
            let mut primary = HANDLE::default();
            unsafe {
                DuplicateTokenEx(
                    token.0,
                    TOKEN_ALL_ACCESS,
                    None,
                    SecurityImpersonation,
                    TokenPrimary,
                    &mut primary,
                )?;
            }
            let primary = Handle(primary);
            unsafe {
                SetTokenInformation(
                    primary.0,
                    TokenSessionId,
                    (&session as *const u32).cast(),
                    4,
                )?;
                if role == "host-resident" {
                    // Only the already privileged interactive resident draws
                    // host overlays. GUI/files/clipboard keep their user token.
                    let enabled = 1u32;
                    if let Err(error) = SetTokenInformation(
                        primary.0,
                        TokenUIAccess,
                        (&enabled as *const u32).cast(),
                        4,
                    ) {
                        tracing::warn!(%error, "host overlay UIAccess unavailable; fullscreen annotations may be occluded");
                    }
                }
            }
            primary
        };
        let job = Handle(unsafe { CreateJobObjectW(None, None)? });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = if role == "user-background" {
            JOB_OBJECT_LIMIT(0)
        } else {
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        };
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )?;
        }
        let exe = std::env::current_exe()?;
        let exe = exe
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("{}", t().system.service_invalid_path))?;
        ensure!(
            !exe.contains('"') && !pipe.contains('"'),
            "{}",
            t().system.service_invalid_arguments
        );
        let stop = if role == "host-resident" {
            let name: Vec<u16> = crate::product::global_object(&format!(
                "Resident.Stop.{}.{}",
                std::process::id(),
                session
            ))
            .encode_utf16()
            .chain(Some(0))
            .collect();
            Some(Handle(unsafe {
                CreateEventW(None, true, false, PCWSTR(name.as_ptr()))?
            }))
        } else {
            None
        };
        let arguments = if matches!(role, "host-resident" | "display-agent" | "desktop-access") {
            String::new()
        } else {
            format!(" --pipe \"{pipe}\"")
        };
        let mut command: Vec<u16> = if role == "user-background" {
            format!("\"{exe}\" gui --background")
        } else {
            format!(
                "\"{exe}\" {role}{arguments} --parent {}",
                std::process::id()
            )
        }
        .encode_utf16()
        .chain(Some(0))
        .collect();
        let path: Vec<u16> = exe.encode_utf16().chain(Some(0)).collect();
        let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain(Some(0)).collect();
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        struct Environment(*mut std::ffi::c_void);
        impl Drop for Environment {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    unsafe {
                        let _ =
                            windows::Win32::System::Environment::DestroyEnvironmentBlock(self.0);
                    }
                }
            }
        }
        let mut environment = Environment(std::ptr::null_mut());
        if role == "user-background" {
            unsafe {
                windows::Win32::System::Environment::CreateEnvironmentBlock(
                    &mut environment.0,
                    Some(primary.0),
                    false,
                )?;
            }
        }
        let mut info = PROCESS_INFORMATION::default();
        unsafe {
            CreateProcessAsUserW(
                Some(primary.0),
                PCWSTR(path.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
                (!environment.0.is_null()).then_some(environment.0.cast_const()),
                None,
                &startup,
                &mut info,
            )?;
        }
        let process = Handle(info.hProcess);
        let thread = Handle(info.hThread);
        if let Err(error) = unsafe { AssignProcessToJobObject(job.0, process.0) } {
            unsafe {
                let _ = TerminateProcess(process.0, 1);
            }
            return Err(error.into());
        }
        if unsafe { ResumeThread(thread.0) } == u32::MAX {
            anyhow::bail!("{}", t().system.service_input_session_start_failed)
        }
        Ok(Self {
            process,
            pid: info.dwProcessId,
            _job: job,
            stop,
        })
    }
    /// The resident as LocalService with the host SID, on the interactive
    /// desktop (displays, overlays).
    fn start_host(session: u32) -> Result<Self> {
        privilege(w!("SeTcbPrivilege"))?;
        privilege(w!("SeAssignPrimaryTokenPrivilege"))?;
        privilege(w!("SeIncreaseQuotaPrivilege"))?;
        let token = host_token(session)?;
        unsafe {
            // Overlays draw above other windows.
            let enabled = 1u32;
            if let Err(error) = SetTokenInformation(token.0, TokenUIAccess, (&enabled as *const u32).cast(), 4) {
                tracing::warn!(%error, "host overlay UIAccess unavailable; fullscreen annotations may be occluded");
            }
        }
        Self::start_role(session, "", "desktop-access")?.finish(Duration::from_secs(10))?;
        let job = Handle(unsafe { CreateJobObjectW(None, None)? });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )?;
        }
        let exe = std::env::current_exe()?;
        let exe = exe
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("{}", t().system.service_invalid_path))?;
        ensure!(!exe.contains('"'), "{}", t().system.service_invalid_arguments);
        // The service sets the stop event; the host may only wait on it.
        let stop = {
            use windows::Win32::Security::Authorization::*;
            let name: Vec<u16> = crate::product::global_object(&format!("Resident.Stop.{}.{session}", std::process::id()))
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let sddl: Vec<u16> = format!("D:P(A;;GA;;;SY)(A;;0x00100000;;;{})", super::vault::host_sid()?)
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(sddl.as_ptr()), SDDL_REVISION_1, &mut descriptor, None)?;
            }
            let attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: false.into(),
            };
            let event = unsafe { CreateEventW(Some(&attributes), true, false, PCWSTR(name.as_ptr())) };
            unsafe {
                let _ = windows::Win32::Foundation::LocalFree(Some(HLOCAL(descriptor.0)));
            }
            Handle(event?)
        };
        let mut command: Vec<u16> = format!("\"{exe}\" host-resident --parent {}", std::process::id())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let path: Vec<u16> = exe.encode_utf16().chain(Some(0)).collect();
        let mut desktop: Vec<u16> = r"winsta0\default".encode_utf16().chain(Some(0)).collect();
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut environment = std::ptr::null_mut();
        unsafe {
            windows::Win32::System::Environment::CreateEnvironmentBlock(&mut environment, Some(token.0), false)?;
        }
        // Its data lives beside the vault, not in LocalService's profile.
        let data = super::vault::host_data()?;
        let block = environment_with(
            environment,
            &[
                ("FARWING_SERVICE_PID", std::process::id().to_string()),
                ("LOCALAPPDATA", data.display().to_string()),
            ],
        );
        unsafe {
            let _ = windows::Win32::System::Environment::DestroyEnvironmentBlock(environment);
        }
        let mut info = PROCESS_INFORMATION::default();
        unsafe {
            CreateProcessAsUserW(
                Some(token.0),
                PCWSTR(path.as_ptr()),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
                Some(block.as_ptr().cast()),
                None,
                &startup,
                &mut info,
            )?;
        }
        let process = Handle(info.hProcess);
        let thread = Handle(info.hThread);
        if let Err(error) = unsafe { AssignProcessToJobObject(job.0, process.0) } {
            unsafe {
                let _ = TerminateProcess(process.0, 1);
            }
            return Err(error.into());
        }
        if unsafe { ResumeThread(thread.0) } == u32::MAX {
            anyhow::bail!("{}", t().system.service_input_session_start_failed)
        }
        Ok(Self {
            process,
            pid: info.dwProcessId,
            _job: job,
            stop: Some(stop),
        })
    }
    pub fn alive(&self) -> bool {
        unsafe { WaitForSingleObject(self.process.0, 0) == WAIT_TIMEOUT }
    }
    /// Wait for a one-shot role to succeed.
    fn finish(self, timeout: Duration) -> Result<()> {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        ensure!(
            unsafe { WaitForSingleObject(self.process.0, millis) } == WAIT_OBJECT_0,
            "{}",
            t().system.service_child_stopped
        );
        let mut code = 0u32;
        unsafe {
            GetExitCodeProcess(self.process.0, &mut code)?;
        }
        ensure!(code == 0, "{}: {code:#x}", t().system.service_child_failed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_rewritten_image_is_hashed_again() {
        let dir = std::env::temp_dir().join(format!("farwing-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("image.bin");
        std::fs::write(&path, b"first").unwrap();
        let first = super::image_hash(&path).unwrap();
        assert_eq!(super::image_hash(&path).unwrap(), first);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"other").unwrap();
        assert_ne!(super::image_hash(&path).unwrap(), first);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// `desktop-access`, run as SYSTEM in the session by the service: opens the
/// session's interactive window station and desktop, and its named-object
/// directory, to the background host's SID (`vault::host_sid`). The service
/// cannot: its own window station is session 0's. Only the service can put
/// that SID in a token.
pub(crate) fn desktop_access(parent: u32) -> Result<()> {
    use windows::Win32::Security::Authorization::SE_WINDOW_OBJECT;
    use windows::Win32::System::StationsAndDesktops::*;
    ensure!(
        super::vault::sid(std::process::id())? == "S-1-5-18",
        "{}",
        t().system.service_invalid_identity
    );
    super::install::verify_running(parent)?;
    let host = super::vault::host_sid()?;
    const READ_CONTROL_WRITE_DAC: u32 = 0x0002_0000 | 0x0004_0000;
    const GENERIC_ALL: u32 = 0x1000_0000;
    let station = unsafe { OpenWindowStationW(w!("WinSta0"), false, READ_CONTROL_WRITE_DAC)? };
    let granted = grant(HANDLE(station.0), SE_WINDOW_OBJECT, &host, GENERIC_ALL);
    unsafe {
        let _ = CloseWindowStation(station);
    }
    granted?;
    let desktop = unsafe { OpenDesktopW(w!("Default"), DESKTOP_CONTROL_FLAGS(0), false, READ_CONTROL_WRITE_DAC)? };
    let granted = grant(HANDLE(desktop.0), SE_WINDOW_OBJECT, &host, GENERIC_ALL);
    unsafe {
        let _ = CloseDesktop(desktop);
    }
    granted?;
    // Named objects (`Local\`, and `Global\` through it) resolve in the
    // session's directory, which admits only its logon SID: query, traverse,
    // create objects and read control, without the logon SID's inherited
    // access to the user's own objects.
    let directory = named_objects(session(std::process::id())?, READ_CONTROL_WRITE_DAC)?;
    grant(directory.0, windows::Win32::Security::Authorization::SE_KERNEL_OBJECT, &host, 0x0002_000F)
}

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    fn NtOpenDirectoryObject(handle: *mut HANDLE, access: u32, attributes: *const ObjectAttributes) -> i32;
}
#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root: HANDLE,
    name: *const UNICODE_STRING,
    attributes: u32,
    descriptor: *const std::ffi::c_void,
    quality: *const std::ffi::c_void,
}

/// The named-object directory of `session`.
fn named_objects(session: u32, access: u32) -> Result<Handle> {
    let path: Vec<u16> = if session == 0 {
        r"\BaseNamedObjects".to_owned()
    } else {
        format!(r"\Sessions\{session}\BaseNamedObjects")
    }
    .encode_utf16()
    .collect();
    let name = UNICODE_STRING {
        Length: (path.len() * 2) as u16,
        MaximumLength: (path.len() * 2) as u16,
        Buffer: PWSTR(path.as_ptr().cast_mut()),
    };
    let attributes = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root: HANDLE::default(),
        name: &name,
        attributes: 0x40, // OBJ_CASE_INSENSITIVE
        descriptor: std::ptr::null(),
        quality: std::ptr::null(),
    };
    let mut handle = HANDLE::default();
    let status = unsafe { NtOpenDirectoryObject(&mut handle, access, &attributes) };
    ensure!(status >= 0, "{}: {status:#x}", t().system.service_invalid_identity);
    Ok(Handle(handle))
}

/// Add an access-allowed entry for `sid` to the object's access list.
fn grant(
    handle: HANDLE,
    kind: windows::Win32::Security::Authorization::SE_OBJECT_TYPE,
    sid: &str,
    mask: u32,
) -> Result<()> {
    use windows::Win32::Security::Authorization::*;
    let wide: Vec<u16> = sid.encode_utf16().chain(Some(0)).collect();
    let mut sid = PSID::default();
    unsafe { ConvertStringSidToSidW(PCWSTR(wide.as_ptr()), &mut sid)? };
    let mut dacl = std::ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let result = (|| -> Result<()> {
        unsafe {
            GetSecurityInfo(handle, kind, DACL_SECURITY_INFORMATION, None, None, Some(&mut dacl), None, Some(&mut descriptor)).ok()?;
        }
        let entry = EXPLICIT_ACCESS_W {
            grfAccessPermissions: mask,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: TRUSTEE_W {
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: PWSTR(sid.0.cast()),
                ..Default::default()
            },
        };
        let mut updated = std::ptr::null_mut();
        unsafe { SetEntriesInAclW(Some(&[entry]), Some(dacl), &mut updated).ok()? };
        let applied = unsafe { SetSecurityInfo(handle, kind, DACL_SECURITY_INFORMATION, None, None, Some(updated), None) }.ok();
        unsafe {
            let _ = windows::Win32::Foundation::LocalFree(Some(HLOCAL(updated.cast())));
        }
        Ok(applied?)
    })();
    unsafe {
        let _ = windows::Win32::Foundation::LocalFree(Some(HLOCAL(descriptor.0)));
        let _ = windows::Win32::Foundation::LocalFree(Some(HLOCAL(sid.0)));
    }
    result
}

#[link(name = "advapi32", kind = "raw-dylib")]
unsafe extern "system" {
    /// `LogonUserExW` with extra token groups (TCB privilege required).
    fn LogonUserExExW(
        username: PCWSTR,
        domain: PCWSTR,
        password: PCWSTR,
        logon_type: u32,
        provider: u32,
        groups: *const TOKEN_GROUPS,
        token: *mut HANDLE,
        logon_sid: *mut PSID,
        profile: *mut *mut std::ffi::c_void,
        profile_length: *mut u32,
        quotas: *mut QUOTA_LIMITS,
    ) -> windows::core::BOOL;
}

/// The background host's token: LocalService with the service's own SID
/// (`vault::host_sid`) as an extra group, for `session`.
pub(crate) fn host_token(session: u32) -> Result<Handle> {
    use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
    privilege(w!("SeTcbPrivilege"))?;
    let host: Vec<u16> = super::vault::host_sid()?.encode_utf16().chain(Some(0)).collect();
    let mut sid = PSID::default();
    unsafe { ConvertStringSidToSidW(PCWSTR(host.as_ptr()), &mut sid)? };
    let groups = TOKEN_GROUPS {
        GroupCount: 1,
        Groups: [SID_AND_ATTRIBUTES {
            Sid: sid,
            Attributes: (SE_GROUP_MANDATORY | SE_GROUP_ENABLED_BY_DEFAULT | SE_GROUP_ENABLED) as u32,
        }],
    };
    let mut logon = HANDLE::default();
    let result = unsafe {
        LogonUserExExW(
            w!("LocalService"),
            w!("NT AUTHORITY"),
            PCWSTR::null(),
            LOGON32_LOGON_SERVICE.0,
            LOGON32_PROVIDER_DEFAULT.0,
            &groups,
            &mut logon,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    }
    .ok();
    unsafe {
        let _ = windows::Win32::Foundation::LocalFree(Some(HLOCAL(sid.0)));
    }
    result?;
    let logon = Handle(logon);
    let mut primary = HANDLE::default();
    unsafe {
        DuplicateTokenEx(logon.0, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut primary)?;
    }
    let primary = Handle(primary);
    unsafe {
        SetTokenInformation(primary.0, TokenSessionId, (&session as *const u32).cast(), 4)?;
    }
    Ok(primary)
}

/// A copy of an environment block (`CreateEnvironmentBlock`) with
/// `variables` set (replaced when present): e.g. `FARWING_SERVICE_PID`, by
/// which the child verifies the service.
fn environment_with(block: *mut std::ffi::c_void, variables: &[(&str, String)]) -> Vec<u16> {
    let mut copy = Vec::new();
    let mut at = block.cast::<u16>();
    // Variables end with an empty string.
    unsafe {
        loop {
            let mut length = 0;
            while *at.add(length) != 0 {
                length += 1;
            }
            if length == 0 {
                break;
            }
            let entry = std::slice::from_raw_parts(at, length + 1);
            let text = String::from_utf16_lossy(&entry[..length]);
            let replaced = variables.iter().any(|(name, _)| {
                text.len() > name.len()
                    && text[..name.len()].eq_ignore_ascii_case(name)
                    && text.as_bytes()[name.len()] == b'='
            });
            if !replaced {
                copy.extend_from_slice(entry);
            }
            at = at.add(length + 1);
        }
    }
    for (name, value) in variables {
        copy.extend(format!("{name}={value}").encode_utf16().chain(Some(0)));
    }
    copy.push(0);
    copy
}
