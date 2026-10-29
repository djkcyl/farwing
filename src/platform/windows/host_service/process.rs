//! Privileged helper lifecycle. Executable identity and active session are checked
//! before attaching a channel; children belong to a kill-on-close job.
use super::pipe::Handle;
use crate::i18n::t;
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use windows::{
    Win32::{
        Foundation::*,
        Security::*,
        System::{JobObjects::*, RemoteDesktop::*, Threading::*},
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
    pub fn start_resident(session: u32) -> Result<Self> {
        Self::start_role(session, "", "host-resident")
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
        let arguments = if matches!(role, "host-resident" | "display-agent") {
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
    pub fn alive(&self) -> bool {
        unsafe { WaitForSingleObject(self.process.0, 0) == WAIT_TIMEOUT }
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

/// A primary token of `NT AUTHORITY\LocalService` for `session`: the
/// identity of the background host's work that faces the network. Needs
/// the SYSTEM caller's TCB privilege.
pub(crate) fn local_service_token(session: u32) -> Result<Handle> {
    privilege(w!("SeTcbPrivilege"))?;
    let mut logon = HANDLE::default();
    unsafe {
        LogonUserW(
            w!("LocalService"),
            w!("NT AUTHORITY"),
            PCWSTR::null(),
            LOGON32_LOGON_SERVICE,
            LOGON32_PROVIDER_DEFAULT,
            &mut logon,
        )?;
    }
    let logon = Handle(logon);
    let mut primary = HANDLE::default();
    unsafe {
        DuplicateTokenEx(
            logon.0,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )?;
    }
    let primary = Handle(primary);
    unsafe {
        SetTokenInformation(primary.0, TokenSessionId, (&session as *const u32).cast(), 4)?;
    }
    Ok(primary)
}

/// `token`'s logon SID (`S-1-5-5-…`): unique to this logon of the account.
fn logon_sid(token: HANDLE) -> Result<String> {
    let mut needed = 0;
    unsafe {
        let _ = GetTokenInformation(token, TokenLogonSid, None, 0, &mut needed);
    }
    ensure!(needed > 0 && needed < 4096, "{}", t().system.vault_invalid_identity_length);
    let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
    unsafe {
        GetTokenInformation(token, TokenLogonSid, Some(buffer.as_mut_ptr().cast()), needed, &mut needed)?;
    }
    let groups = unsafe { &*buffer.as_ptr().cast::<TOKEN_GROUPS>() };
    ensure!(groups.GroupCount == 1, "{}", t().system.vault_invalid_identity_length);
    let mut text = PWSTR::null();
    unsafe {
        windows::Win32::Security::Authorization::ConvertSidToStringSidW(groups.Groups[0].Sid, &mut text)?;
    }
    let logon = unsafe { text.to_string() };
    unsafe {
        let _ = windows::Win32::Foundation::LocalFree(Some(HLOCAL(text.0.cast())));
    }
    Ok(logon?)
}

/// Access to the session's interactive window station and its default
/// desktop for a logon SID, withdrawn on drop: a LocalService child needs
/// it to change the display configuration and draw overlays.
pub(crate) struct InteractiveAccess {
    sid: String,
}
impl InteractiveAccess {
    pub(crate) fn grant(token: HANDLE) -> Result<Self> {
        let access = Self { sid: logon_sid(token)? };
        access.apply(true)?;
        Ok(access)
    }
    fn apply(&self, grant: bool) -> Result<()> {
        use windows::Win32::Security::Authorization::*;
        use windows::Win32::System::StationsAndDesktops::*;
        let wide: Vec<u16> = self.sid.encode_utf16().chain(Some(0)).collect();
        let mut sid = PSID::default();
        unsafe { ConvertStringSidToSidW(PCWSTR(wide.as_ptr()), &mut sid)? };
        let result = (|| -> Result<()> {
            // READ_CONTROL | WRITE_DAC
            let station = unsafe { OpenWindowStationW(w!("WinSta0"), false, 0x0002_0000 | 0x0004_0000)? };
            let desktop = unsafe { OpenDesktopW(w!("Default"), DESKTOP_CONTROL_FLAGS(0), false, 0x0002_0000 | 0x0004_0000) };
            let desktop = match desktop {
                Ok(desktop) => desktop,
                Err(error) => {
                    unsafe {
                        let _ = CloseWindowStation(station);
                    }
                    return Err(error.into());
                }
            };
            let outcome = (|| -> Result<()> {
                for handle in [HANDLE(station.0), HANDLE(desktop.0)] {
                    let mut dacl = std::ptr::null_mut();
                    let mut descriptor = PSECURITY_DESCRIPTOR::default();
                    unsafe {
                        GetSecurityInfo(handle, SE_WINDOW_OBJECT, DACL_SECURITY_INFORMATION, None, None, Some(&mut dacl), None, Some(&mut descriptor)).ok()?;
                    }
                    let entry = EXPLICIT_ACCESS_W {
                        grfAccessPermissions: 0x1000_0000, // GENERIC_ALL
                        grfAccessMode: if grant { GRANT_ACCESS } else { REVOKE_ACCESS },
                        grfInheritance: NO_INHERITANCE,
                        Trustee: TRUSTEE_W {
                            TrusteeForm: TRUSTEE_IS_SID,
                            TrusteeType: TRUSTEE_IS_UNKNOWN,
                            ptstrName: PWSTR(sid.0.cast()),
                            ..Default::default()
                        },
                    };
                    let mut updated = std::ptr::null_mut();
                    let set = unsafe { SetEntriesInAclW(Some(&[entry]), Some(dacl), &mut updated) };
                    let applied = if set.is_ok() {
                        unsafe { SetSecurityInfo(handle, SE_WINDOW_OBJECT, DACL_SECURITY_INFORMATION, None, None, Some(updated), None) }.ok()
                    } else {
                        set.ok()
                    };
                    unsafe {
                        let _ = windows::Win32::Foundation::LocalFree(Some(HLOCAL(updated.cast())));
                        let _ = windows::Win32::Foundation::LocalFree(Some(HLOCAL(descriptor.0)));
                    }
                    applied?;
                }
                Ok(())
            })();
            unsafe {
                let _ = CloseDesktop(desktop);
                let _ = CloseWindowStation(station);
            }
            outcome
        })();
        unsafe {
            let _ = windows::Win32::Foundation::LocalFree(Some(HLOCAL(sid.0)));
        }
        result
    }
}
impl Drop for InteractiveAccess {
    fn drop(&mut self) {
        if let Err(error) = self.apply(false) {
            tracing::warn!(%error, "interactive desktop access not withdrawn");
        }
    }
}

/// A window station and desktop of their own for a child running as
/// `token`'s logon session: user32 needs one, and the user's desktop stays
/// out of its reach. Open while the child runs.
pub(crate) struct PrivateDesktop {
    station: windows::Win32::System::StationsAndDesktops::HWINSTA,
    desktop: windows::Win32::System::StationsAndDesktops::HDESK,
    /// `station\desktop`, for `STARTUPINFOW::lpDesktop`.
    pub name: Vec<u16>,
}
impl PrivateDesktop {
    pub(crate) fn new(token: HANDLE) -> Result<Self> {
        use windows::Win32::Security::Authorization::*;
        use windows::Win32::System::StationsAndDesktops::*;
        let logon = logon_sid(token)?;
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;SY)(A;;GA;;;{logon})").encode_utf16().chain(Some(0)).collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )?;
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let station_name = format!("{}.{}", crate::product::object_name("Host"), uuid::Uuid::new_v4().simple());
        let wide: Vec<u16> = station_name.encode_utf16().chain(Some(0)).collect();
        let result = (|| -> Result<Self> {
            let station = unsafe { CreateWindowStationW(PCWSTR(wide.as_ptr()), 0, 0x037F | 0x000F_0000, Some(&attributes))? };
            // A desktop is created on the calling process's window station.
            let previous = unsafe { GetProcessWindowStation()? };
            unsafe { SetProcessWindowStation(station)? };
            let desktop = unsafe {
                CreateDesktopW(w!("Default"), PCWSTR::null(), None, DESKTOP_CONTROL_FLAGS(0), 0x01FF | 0x000F_0000, Some(&attributes))
            };
            unsafe {
                let _ = SetProcessWindowStation(previous);
            }
            let desktop = match desktop {
                Ok(desktop) => desktop,
                Err(error) => {
                    unsafe {
                        let _ = CloseWindowStation(station);
                    }
                    return Err(error.into());
                }
            };
            Ok(Self {
                station,
                desktop,
                name: format!(r"{station_name}\Default").encode_utf16().chain(Some(0)).collect(),
            })
        })();
        unsafe {
            let _ = windows::Win32::Foundation::LocalFree(Some(HLOCAL(descriptor.0)));
        }
        result
    }
}
impl Drop for PrivateDesktop {
    fn drop(&mut self) {
        use windows::Win32::System::StationsAndDesktops::*;
        unsafe {
            let _ = CloseDesktop(self.desktop);
            let _ = CloseWindowStation(self.station);
        }
    }
}

/// Run `Farwing.exe <arguments>` as LocalService in `session`, on a
/// window station of its own (it shows nothing): `input` goes to its
/// standard input, its standard output comes back. Ends it after
/// `timeout` or once `active` turns false.
pub(crate) fn run_as_local_service(
    session: u32,
    interactive: bool,
    arguments: &str,
    input: &[u8],
    timeout: std::time::Duration,
    active: &dyn Fn() -> bool,
) -> Result<Vec<u8>> {
    use std::io::{Read, Write};
    use std::os::windows::io::FromRawHandle;
    use windows::Win32::System::Pipes::CreatePipe;
    let token = local_service_token(session)?;
    privilege(w!("SeAssignPrimaryTokenPrivilege"))?;
    privilege(w!("SeIncreaseQuotaPrivilege"))?;
    let inherit = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        bInheritHandle: true.into(),
        ..Default::default()
    };
    let (mut child_in, mut parent_in) = (HANDLE::default(), HANDLE::default());
    let (mut parent_out, mut child_out) = (HANDLE::default(), HANDLE::default());
    unsafe {
        CreatePipe(&mut child_in, &mut parent_in, Some(&inherit), 0)?;
        CreatePipe(&mut parent_out, &mut child_out, Some(&inherit), 0)?;
    }
    let (child_in, child_out) = (Handle(child_in), Handle(child_out));
    // Only the child's ends are inherited.
    unsafe {
        SetHandleInformation(parent_in, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))?;
        SetHandleInformation(parent_out, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))?;
    }
    let mut writer = unsafe { std::fs::File::from_raw_handle(parent_in.0) };
    let mut reader = unsafe { std::fs::File::from_raw_handle(parent_out.0) };
    let exe = std::env::current_exe()?;
    let exe = exe
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("{}", t().system.service_invalid_path))?;
    ensure!(!exe.contains('"'), "{}", t().system.service_invalid_arguments);
    let mut command: Vec<u16> = format!("\"{exe}\" {arguments}").encode_utf16().chain(Some(0)).collect();
    let path: Vec<u16> = exe.encode_utf16().chain(Some(0)).collect();
    // The interactive desktop when the child changes displays or draws;
    // otherwise one of its own.
    let (_access, _private, mut desktop) = if interactive {
        let access = InteractiveAccess::grant(token.0)?;
        (Some(access), None, r"winsta0\default".encode_utf16().chain(Some(0)).collect::<Vec<u16>>())
    } else {
        let private = PrivateDesktop::new(token.0)?;
        let name = private.name.clone();
        (None, Some(private), name)
    };
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        dwFlags: STARTF_USESTDHANDLES,
        hStdInput: child_in.0,
        hStdOutput: child_out.0,
        hStdError: child_out.0,
        ..Default::default()
    };
    let mut environment = std::ptr::null_mut();
    unsafe {
        windows::Win32::System::Environment::CreateEnvironmentBlock(&mut environment, Some(token.0), false)?;
    }
    let environment_block = with_service_pid(environment);
    let mut info = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessAsUserW(
            Some(token.0),
            PCWSTR(path.as_ptr()),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            Some(environment_block.as_ptr().cast()),
            None,
            &startup,
            &mut info,
        )
    };
    unsafe {
        let _ = windows::Win32::System::Environment::DestroyEnvironmentBlock(environment);
    }
    created?;
    let process = Handle(info.hProcess);
    drop(Handle(info.hThread));
    drop(child_in);
    drop(child_out);
    let collected = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).map(|_| bytes)
    });
    let written = writer.write_all(input);
    drop(writer);
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if unsafe { WaitForSingleObject(process.0, 50) } == WAIT_OBJECT_0 {
            break;
        }
        if !active() || std::time::Instant::now() >= deadline {
            unsafe {
                let _ = TerminateProcess(process.0, 1);
                let _ = WaitForSingleObject(process.0, 5000);
            }
            anyhow::bail!("{}", t().system.service_child_stopped);
        }
    }
    written?;
    let mut code = 0u32;
    unsafe {
        GetExitCodeProcess(process.0, &mut code)?;
    }
    let bytes = collected
        .join()
        .map_err(|_| anyhow::anyhow!("{}", t().system.service_child_stopped))??;
    ensure!(code == 0, "{}: {code:#x}", t().system.service_child_failed);
    Ok(bytes)
}

/// A copy of an environment block (`CreateEnvironmentBlock`) with
/// `FARWING_SERVICE_PID` added: the child verifies the service by it.
fn with_service_pid(block: *mut std::ffi::c_void) -> Vec<u16> {
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
            copy.extend_from_slice(std::slice::from_raw_parts(at, length + 1));
            at = at.add(length + 1);
        }
    }
    if let Some(pid) = super::resident::service_pid() {
        copy.extend(format!("FARWING_SERVICE_PID={pid}").encode_utf16().chain(Some(0)));
    }
    copy.push(0);
    copy
}
