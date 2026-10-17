//! One software playback decoder across all OpenUUYC processes in this
//! desktop login session. The owner stays on its decoder worker thread.
use anyhow::Result;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use {
    anyhow::{Context, bail},
    windows::Win32::Foundation::{
        CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
    windows::core::w,
};

static OCCUPIED: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub(crate) struct SoftwarePlaybackBusy;
impl std::fmt::Display for SoftwarePlaybackBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("已有一个软解窗口正在播放，请先关闭它或切换到硬解。整个客户端最多允许一个软解播放窗口。")
    }
}
impl std::error::Error for SoftwarePlaybackBusy {}

pub(crate) struct SoftwareSlot {
    #[cfg(windows)]
    handle: HANDLE,
    #[cfg(not(windows))]
    lock: std::fs::File,
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
    #[cfg(windows)]
    fn acquire_global() -> Result<Rc<Self>> {
        let handle =
            unsafe { CreateMutexW(None, false, w!("Local\\OpenUUYC.SoftwareVideoPlayback.v1")) }
                .context("创建全客户端软解窗口限制失败")?;
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
        bail!("获取软解窗口名额失败：{status:?}")
    }

    /// flock releases automatically when the owning process exits, matching the
    /// abandoned-mutex recovery the Windows slot relies on.
    #[cfg(not(windows))]
    fn acquire_global() -> Result<Rc<Self>> {
        use anyhow::Context;
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let path = base.join(format!("openuuyc-software-playback-{}.lock", unsafe {
            libc::getuid()
        }));
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("创建全客户端软解窗口限制失败：{}", path.display()))?;
        match lock.try_lock() {
            Ok(()) => Ok(Rc::new(Self {
                lock,
                _thread_bound: PhantomData,
            })),
            Err(std::fs::TryLockError::WouldBlock) => Err(SoftwarePlaybackBusy.into()),
            Err(std::fs::TryLockError::Error(error)) => {
                Err(anyhow::Error::new(error).context("获取软解窗口名额失败"))
            }
        }
    }
}
impl Drop for SoftwareSlot {
    fn drop(&mut self) {
        // Rc/PhantomData keep final release on the acquiring worker. Win32
        // abandoned-mutex handling permits recovery if the owner process dies.
        #[cfg(windows)]
        {
            let _ = unsafe { ReleaseMutex(self.handle) };
            let _ = unsafe { CloseHandle(self.handle) };
        }
        #[cfg(not(windows))]
        {
            let _ = self.lock.unlock();
        }
        OCCUPIED.store(false, Ordering::Release);
    }
}
