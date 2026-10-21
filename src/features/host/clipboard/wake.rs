//! Queue readiness only; permissions and payloads stay on the authenticated pipe.
use crate::platform::windows::host_service::pipe::Handle;
use crate::i18n::t;
use anyhow::{Result, ensure};
use windows::{
    Win32::{Foundation::*, System::Threading::*},
    core::PCWSTR,
};
pub(super) struct Wake(Handle);
// Event handles support concurrent SetEvent/wait; Arc owners keep them alive.
unsafe impl Send for Wake {}
unsafe impl Sync for Wake {}
static PREFIX: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::product::global_object("Clipboard.Wake."));
impl Wake {
    pub fn new() -> Result<Self> {
        Ok(Self(Handle(unsafe {
            CreateEventW(None, false, false, None)?
        })))
    }
    pub fn named() -> Result<(Self, String)> {
        let name = format!("{}{}", *PREFIX, uuid::Uuid::new_v4().simple());
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let handle = Handle(unsafe { CreateEventW(None, false, false, PCWSTR(wide.as_ptr()))? });
        ensure!(
            unsafe { GetLastError() } != ERROR_ALREADY_EXISTS,
            "剪贴板唤醒事件已存在"
        );
        Ok((Self(handle), name))
    }
    pub fn open(name: &str) -> Result<Self> {
        let suffix = name
            .strip_prefix(PREFIX.as_str())
            .filter(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()));
        ensure!(suffix.is_some(), "剪贴板唤醒事件名称无效");
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        Ok(Self(Handle(unsafe {
            OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(wide.as_ptr()))?
        })))
    }
    pub fn notify(&self) {
        let _ = unsafe { SetEvent(self.0.0) };
    }
    pub fn wait(&self, remote: Option<&Self>) -> Result<()> {
        let handles = [self.0.0, remote.map_or(self.0.0, |r| r.0.0)];
        let handles = if remote.is_some() {
            &handles[..]
        } else {
            &handles[..1]
        };
        let result = unsafe { WaitForMultipleObjects(handles, false, 10) };
        ensure!(
            result == WAIT_TIMEOUT
                || (result.0 >= WAIT_OBJECT_0.0
                    && result.0 < WAIT_OBJECT_0.0 + handles.len() as u32),
            "{}",
            t().features.clipboard_wait_failed
        );
        Ok(())
    }
}
