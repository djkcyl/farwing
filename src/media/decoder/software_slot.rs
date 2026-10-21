//! One software playback decoder across all Farwing processes of one profile
//! in this Windows login session. The owner stays on its decoder worker thread.
use anyhow::{Context, Result, bail};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

// Profile-specific so concurrent development profiles do not block each other.
static SLOT_NAME: crate::product::WideName =
    crate::product::WideName::new(|| crate::product::local_object("SoftwareVideoPlayback.v1"));

static OCCUPIED: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub(crate) struct SoftwarePlaybackBusy;
impl std::fmt::Display for SoftwarePlaybackBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(crate::i18n::t().session.software_playback_busy)
    }
}
impl std::error::Error for SoftwarePlaybackBusy {}

pub(crate) struct SoftwareSlot {
    handle: HANDLE,
    _thread_bound: PhantomData<Rc<()>>,
}
impl SoftwareSlot {
    pub(crate) fn acquire() -> Result<Rc<Self>> {
        // Also disallow Win32 mutex recursion on the same thread. Existing
        // playback explicitly shares an Rc across decoder reconfiguration.
        if OCCUPIED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(SoftwarePlaybackBusy.into());
        }
        let result = Self::acquire_global();
        if result.is_err() {
            OCCUPIED.store(false, Ordering::Release);
        }
        result
    }
    fn acquire_global() -> Result<Rc<Self>> {
        let handle =
            unsafe { CreateMutexW(None, false, SLOT_NAME.pcwstr()) }
                .context(crate::i18n::t().session.software_slot_create_failed)?;
        let status = unsafe { WaitForSingleObject(handle, 0) };
        if status == WAIT_OBJECT_0 || status == WAIT_ABANDONED {
            return Ok(Rc::new(Self {
                handle,
                _thread_bound: PhantomData,
            }));
        }
        let _ = unsafe { CloseHandle(handle) };
        if status == WAIT_TIMEOUT {
            return Err(SoftwarePlaybackBusy.into());
        }
        bail!("{}", (crate::i18n::t().session.software_slot_failed)(&format!("{status:?}")))
    }
}
impl Drop for SoftwareSlot {
    fn drop(&mut self) {
        // Rc/PhantomData keep final release on the acquiring worker. Win32
        // abandoned-mutex handling permits recovery if the owner process dies.
        let _ = unsafe { ReleaseMutex(self.handle) };
        let _ = unsafe { CloseHandle(self.handle) };
        OCCUPIED.store(false, Ordering::Release);
    }
}
