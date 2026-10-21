//! Process-bound recovery guard primitives. No display policy lives here.
use anyhow::{Context, Result, ensure};
use crate::i18n::t;
use windows::{
    Win32::{Foundation::*, System::Threading::*},
    core::PCWSTR,
};

pub(crate) struct Handle(pub HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}
pub(crate) struct Serial(Handle);
impl Serial {
    pub fn acquire() -> Result<Self> {
        let name = crate::product::wide(&crate::product::local_object("DisplayMutation"));
        let handle = Handle(unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }?);
        let result = unsafe { WaitForSingleObject(handle.0, INFINITE) };
        ensure!(
            result == WAIT_OBJECT_0 || result == WAIT_ABANDONED,
            "{}",
            t().system.display_lock_unavailable
        );
        Ok(Self(handle))
    }
}
impl Drop for Serial {
    fn drop(&mut self) {
        let _ = unsafe { ReleaseMutex(self.0.0) };
    }
}
pub(crate) fn birth(handle: HANDLE) -> Result<u64> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }?;
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}
pub(crate) fn current_birth() -> Result<u64> {
    birth(unsafe { GetCurrentProcess() })
}
pub(crate) fn owner_alive(pid: u32, created: u64) -> Result<bool> {
    let handle = match unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )
    } {
        Ok(handle) => Handle(handle),
        Err(error)
            if error.code() == windows::core::HRESULT::from_win32(ERROR_INVALID_PARAMETER.0) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    };
    Ok(birth(handle.0)? == created && !exited(&handle, 0)?)
}
pub(crate) fn parent(pid: u32, created: u64) -> Result<Handle> {
    let handle = Handle(
        unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
        }
        .context(t().system.display_owner_open_failed)?,
    );
    ensure!(birth(handle.0)? == created, "{}", t().system.display_owner_replaced);
    Ok(handle)
}
pub(crate) fn exited(handle: &Handle, timeout: u32) -> Result<bool> {
    let result = unsafe { WaitForSingleObject(handle.0, timeout) };
    ensure!(
        result == WAIT_OBJECT_0 || result == WAIT_TIMEOUT,
        "{}",
        t().system.display_owner_wait_failed
    );
    Ok(result == WAIT_OBJECT_0)
}
fn event_name(token: &str) -> Vec<u16> {
    crate::product::wide(&crate::product::local_object(&format!(
        "DisplayRecovery.{token}"
    )))
}
pub(crate) fn start_guard(token: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let name = event_name(token);
    let ready = Handle(unsafe { CreateEventW(None, true, false, PCWSTR(name.as_ptr())) }?);
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args(["display-recovery", token])
        .creation_flags(0x08000000)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let result = unsafe { WaitForSingleObject(ready.0, 10000) };
    if result != WAIT_OBJECT_0 {
        let status = child.try_wait()?;
        anyhow::bail!("{}", (t().system.display_guard_not_ready)(&format!("{status:?}")));
    }
    Ok(())
}
pub(crate) fn ready(token: &str) -> Result<()> {
    let name = event_name(token);
    let event = Handle(unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) }?);
    unsafe { SetEvent(event.0) }?;
    Ok(())
}
