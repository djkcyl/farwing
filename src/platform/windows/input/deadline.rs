//! On-demand high-resolution wakeup; no polling thread or global timer period.
use anyhow::{Context as _, Result};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;
use tokio::sync::Notify;
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::Threading::*,
};

struct Callback {
    wake: Arc<Notify>,
    fired: AtomicBool,
}
pub(crate) struct Timer {
    timer: HANDLE,
    wait: PTP_WAIT,
    callback: Box<Callback>,
    armed: Option<Instant>,
}
// Native calls are serialized by RemoteInput's mutex. The callback only uses
// atomics and Notify in the stable Box; Drop drains callbacks before freeing it.
unsafe impl Send for Timer {}

unsafe extern "system" fn notify(
    _: PTP_CALLBACK_INSTANCE,
    context: *mut std::ffi::c_void,
    _: PTP_WAIT,
    _: u32,
) {
    // SAFETY: Timer owns the Box until WaitForThreadpoolWaitCallbacks completes.
    let callback = unsafe { &*context.cast::<Callback>() };
    callback.fired.store(true, Ordering::Release);
    callback.wake.notify_one();
}
impl Timer {
    pub fn new(wake: Arc<Notify>) -> Result<Self> {
        let timer = unsafe {
            CreateWaitableTimerExW(
                None,
                None,
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_MODIFY_STATE.0 | SYNCHRONIZATION_SYNCHRONIZE.0,
            )
        }
        .context("创建鼠标高精度定时器失败")?;
        let callback = Box::new(Callback {
            wake,
            fired: AtomicBool::new(false),
        });
        let context = (&*callback as *const Callback).cast_mut().cast();
        let wait = match unsafe { CreateThreadpoolWait(Some(notify), Some(context), None) } {
            Ok(wait) => wait,
            Err(error) => {
                unsafe {
                    let _ = CloseHandle(timer);
                }
                return Err(error).context("创建鼠标定时通知失败");
            }
        };
        Ok(Self {
            timer,
            wait,
            callback,
            armed: None,
        })
    }
    pub fn arm(&mut self, deadline: Instant) -> Result<()> {
        if self.armed == Some(deadline) && !self.callback.fired.load(Ordering::Acquire) {
            return Ok(());
        }
        let ticks = deadline
            .saturating_duration_since(Instant::now())
            .as_nanos()
            .div_ceil(100)
            .clamp(1, i64::MAX as u128) as i64;
        let due = -ticks;
        self.callback.fired.store(false, Ordering::Release);
        self.armed = Some(deadline);
        // A stale callback can only wake the consumer; it cannot send an event.
        // The consumer always rechecks the current deadline and input epoch.
        unsafe {
            SetThreadpoolWait(self.wait, Some(self.timer), None);
            SetWaitableTimerEx(self.timer, &due, 0, None, None, None, 0)
                .context("设置鼠标发送定时器失败")?;
        }
        Ok(())
    }
    pub fn disarm(&mut self) {
        if self.armed.take().is_some() {
            unsafe {
                SetThreadpoolWait(self.wait, None, None);
                let _ = CancelWaitableTimer(self.timer);
            }
        }
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        unsafe {
            SetThreadpoolWait(self.wait, None, None);
            let _ = CancelWaitableTimer(self.timer);
            WaitForThreadpoolWaitCallbacks(self.wait, true);
            CloseThreadpoolWait(self.wait);
            let _ = CloseHandle(self.timer);
        }
    }
}
