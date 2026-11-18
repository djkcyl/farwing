//! Queue readiness only, as on Windows. Linux has no SYSTEM service handing
//! the clipboard to a user child, so the event never leaves this process and
//! there is no remote side to wait on.
use anyhow::Result;
use std::{
    sync::{Condvar, Mutex},
    time::Duration,
};

/// An auto-reset event, matching the Windows `CreateEventW(.., false, false, ..)`.
pub(super) struct Wake {
    signalled: Mutex<bool>,
    changed: Condvar,
}
impl Wake {
    pub fn new() -> Result<Self> {
        Ok(Self {
            signalled: Mutex::new(false),
            changed: Condvar::new(),
        })
    }
    pub fn notify(&self) {
        *self.signalled.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.changed.notify_one();
    }
    /// `remote` is always `None`: the agent's `Remote` cannot exist here.
    pub fn wait(&self, _remote: Option<&Self>) -> Result<()> {
        let signalled = self.signalled.lock().unwrap_or_else(|e| e.into_inner());
        let (mut signalled, _) = self
            .changed
            .wait_timeout_while(signalled, Duration::from_millis(10), |s| !*s)
            .unwrap_or_else(|e| e.into_inner());
        *signalled = false;
        Ok(())
    }
}
