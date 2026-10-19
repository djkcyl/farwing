//! Local shutdown/reboot with a scoped duplicate token; no permanent privilege or policy changes.
use super::host_service::pipe::Handle;
use anyhow::{Context, Result, ensure};
use windows::{
    Win32::{
        Foundation::*,
        Security::*,
        System::{Shutdown::*, Threading::*},
    },
    core::w,
};
fn token() -> Result<Handle> {
    unsafe {
        let mut raw = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE, &mut raw)?;
        let original = Handle(raw);
        let mut raw = HANDLE::default();
        DuplicateTokenEx(
            original.0,
            TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES | TOKEN_IMPERSONATE,
            None,
            SecurityImpersonation,
            TokenImpersonation,
            &mut raw,
        )?;
        let token = Handle(raw);
        let mut luid = LUID::default();
        LookupPrivilegeValueW(None, SE_SHUTDOWN_NAME, &mut luid)?;
        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        SetLastError(ERROR_SUCCESS);
        AdjustTokenPrivileges(token.0, false, Some(&privileges), 0, None, None)?;
        ensure!(
            GetLastError() == ERROR_SUCCESS,
            "当前运行身份没有关机权限，请检查服务与系统策略"
        );
        Ok(token)
    }
}
pub(crate) fn probe() -> Result<()> {
    drop(token()?);
    Ok(())
}
pub(crate) fn execute(action: crate::features::host::power::Action) -> Result<()> {
    let current = token()?;
    unsafe {
        let mut previous = HANDLE::default();
        let old = match OpenThreadToken(
            GetCurrentThread(),
            TOKEN_IMPERSONATE | TOKEN_QUERY,
            true,
            &mut previous,
        ) {
            Ok(()) => Some(Handle(previous)),
            Err(e) if e.code() == ERROR_NO_TOKEN.to_hresult() => None,
            Err(e) => return Err(e.into()),
        };
        struct Restore(Option<Handle>);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    let _ = SetThreadToken(None, self.0.as_ref().map(|h| h.0));
                }
            }
        }
        SetThreadToken(None, Some(current.0))?;
        let _restore = Restore(old);
        InitiateSystemShutdownExW(
            None,
            w!("Farwing 远程电源请求"),
            0,
            false,
            action == crate::features::host::power::Action::Reboot,
            SHTDN_REASON_MAJOR_OTHER | SHTDN_REASON_MINOR_OTHER | SHTDN_REASON_FLAG_PLANNED,
        )
        .context("Windows 未接受电源请求")
    }
}
