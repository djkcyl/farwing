//! Privileged helper lifecycle. Executable identity and active session are checked
//! before attaching a channel; children belong to a kill-on-close job.
use super::pipe::Handle;
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
        "被控服务版本或身份不匹配"
    );
    Ok(())
}
pub(crate) fn image_hash(path: &std::path::Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut bytes = [0; 65536];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        digest.update(&bytes[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
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
            "被控服务缺少系统权限"
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
        ensure!(session != u32::MAX, "当前没有可用的Windows会话");
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
            .ok_or_else(|| anyhow::anyhow!("被控服务路径无效"))?;
        ensure!(
            !exe.contains('"') && !pipe.contains('"'),
            "被控服务参数无效"
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
            anyhow::bail!("无法启动输入会话进程")
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
