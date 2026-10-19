//! Nonblocking admission to one composition frame; never wait on the UI thread.
use anyhow::{Context, Result, ensure};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Graphics::Dxgi::{IDXGISwapChain1, IDXGISwapChain2},
        System::Threading::WaitForSingleObject,
    },
    core::Interface,
};

pub(super) struct Readiness {
    handle: HANDLE,
    acquired: bool,
}
impl Readiness {
    pub fn new(chain: &IDXGISwapChain1) -> Result<Self> {
        let chain: IDXGISwapChain2 = chain.cast()?;
        unsafe { chain.SetMaximumFrameLatency(1) }.context("set UI frame latency")?;
        let handle = unsafe { chain.GetFrameLatencyWaitableObject() };
        ensure!(
            !handle.is_invalid(),
            "UI frame readiness handle unavailable"
        );
        Ok(Self {
            handle,
            acquired: false,
        })
    }
    pub fn ready(&mut self) -> Result<bool> {
        if !self.acquired {
            match unsafe { WaitForSingleObject(self.handle, 0) } {
                WAIT_OBJECT_0 => self.acquired = true,
                WAIT_TIMEOUT => return Ok(false),
                _ => {
                    return Err(windows::core::Error::from_thread())
                        .context("poll UI frame readiness");
                }
            }
        }
        Ok(true)
    }
    pub fn presented(&mut self) {
        self.acquired = false;
    }
}
impl Drop for Readiness {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.handle) };
    }
}
