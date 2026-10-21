//! Power availability belongs to a live capture, not to the persistent monitor.
use crate::i18n::t;
use anyhow::{Context, Result, ensure};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            Power::{
                ES_DISPLAY_REQUIRED, PowerClearRequest, PowerCreateRequest,
                PowerRequestDisplayRequired, PowerRequestSystemRequired, PowerSetRequest,
                SetThreadExecutionState,
            },
            Threading::{POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0},
        },
    },
    core::PWSTR,
};

pub(super) struct Request {
    handle: HANDLE,
    system: bool,
    display: bool,
}

impl Request {
    pub(super) fn new() -> Result<Self> {
        let mut reason: Vec<u16> = t()
            .system
            .capture_power_reason
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let context = REASON_CONTEXT {
            Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING,
            Reason: REASON_CONTEXT_0 {
                SimpleReasonString: PWSTR(reason.as_mut_ptr()),
            },
            ..Default::default()
        };
        let mut request = Self {
            handle: unsafe { PowerCreateRequest(&context) }
                .context(t().system.capture_power_request_failed)?,
            system: false,
            display: false,
        };
        // Handle-scoped requests compose across concurrent captures and source
        // replacement; releasing one cannot clear another thread's requirement.
        unsafe { PowerSetRequest(request.handle, PowerRequestSystemRequired) }
            .context(t().system.capture_keep_system_failed)?;
        request.system = true;
        unsafe { PowerSetRequest(request.handle, PowerRequestDisplayRequired) }
            .context(t().system.capture_keep_display_failed)?;
        request.display = true;
        // Explicitly wake an already idle display before opening duplication.
        // No CONTINUOUS thread state to leak into a reused blocking worker, and
        // no synthetic input, away mode or change to the user's power plan.
        ensure!(
            unsafe { SetThreadExecutionState(ES_DISPLAY_REQUIRED) }.0 != 0,
            "{}",
            t().system.capture_wake_display_failed
        );
        tracing::debug!("capture display power request acquired");
        Ok(request)
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        for (active, kind) in [
            (self.display, PowerRequestDisplayRequired),
            (self.system, PowerRequestSystemRequired),
        ] {
            if active && let Err(error) = unsafe { PowerClearRequest(self.handle, kind) } {
                tracing::warn!(%error, ?kind, "capture power request release failed");
            }
        }
        // Closing the unique handle also covers partially initialized requests.
        if let Err(error) = unsafe { CloseHandle(self.handle) } {
            tracing::warn!(%error, "capture power request handle close failed");
        }
        tracing::debug!("capture display power request released");
    }
}
