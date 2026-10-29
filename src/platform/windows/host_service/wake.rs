//! Readiness events of the user agents (clipboard, files): a worker sleeps
//! until something is queued for it, instead of polling. Only readiness is
//! signalled; permissions and payloads stay on the authenticated pipe.
use super::pipe::Handle;
use crate::i18n::t;
use anyhow::{Result, ensure};
use std::time::Duration;
use windows::{
    Win32::{Foundation::*, System::Threading::*},
    core::PCWSTR,
};

pub(crate) struct Wake(Handle);
// Event handles support concurrent SetEvent/wait; Arc owners keep them alive.
unsafe impl Send for Wake {}
unsafe impl Sync for Wake {}

static PREFIX: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::product::global_object("Agent.Wake."));

impl Wake {
    /// An unnamed auto-reset event, for one process.
    pub fn new() -> Result<Self> {
        Ok(Self(Handle(unsafe { CreateEventW(None, false, false, None)? })))
    }
    /// A named event the agent creates and tells its privileged parent about
    /// (the background host may wait on it).
    pub fn named() -> Result<(Self, String)> {
        use windows::Win32::Security::{Authorization::*, *};
        let name = format!("{}{}", *PREFIX, uuid::Uuid::new_v4().simple());
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;SY)(A;;GA;;;OW)(A;;0x00100000;;;{})", super::vault::host_sid()?)
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(sddl.as_ptr()), SDDL_REVISION_1, &mut descriptor, None)?;
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let event = unsafe { CreateEventW(Some(&attributes), false, false, PCWSTR(wide.as_ptr())) };
        unsafe {
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        }
        let handle = Handle(event?);
        ensure!(
            unsafe { GetLastError() } != ERROR_ALREADY_EXISTS,
            "{}",
            t().system.agent_wake_invalid
        );
        Ok((Self(handle), name))
    }
    pub fn open(name: &str) -> Result<Self> {
        let suffix = name
            .strip_prefix(PREFIX.as_str())
            .filter(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()));
        ensure!(suffix.is_some(), "{}", t().system.agent_wake_invalid);
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        Ok(Self(Handle(unsafe {
            OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(wide.as_ptr()))?
        })))
    }
    pub fn notify(&self) {
        let _ = unsafe { SetEvent(self.0.0) };
    }
    /// Until this or `remote` is signalled, at most `timeout`.
    pub fn wait(&self, remote: Option<&Self>, timeout: Duration) -> Result<()> {
        let handles = [self.0.0, remote.map_or(self.0.0, |r| r.0.0)];
        let handles = if remote.is_some() {
            &handles[..]
        } else {
            &handles[..1]
        };
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let result = unsafe { WaitForMultipleObjects(handles, false, millis) };
        ensure!(
            result == WAIT_TIMEOUT
                || (result.0 >= WAIT_OBJECT_0.0
                    && result.0 < WAIT_OBJECT_0.0 + handles.len() as u32),
            "{}",
            t().system.service_pipe_wait_failed
        );
        Ok(())
    }
}
