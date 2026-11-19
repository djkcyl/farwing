//! MMCSS 线程注册。采集线程用 Capture 任务而不是普通线程优先级，
//! 让桌面合成/驱动唤醒的抖动不被后台任务放大。
use anyhow::Result;
use windows::{
    Win32::{Foundation::HANDLE, System::Threading::*},
    core::w,
};

pub(crate) struct CapturePriority(HANDLE);

impl CapturePriority {
    pub(crate) fn enter() -> Result<Self> {
        let mut task = 0;
        Ok(Self(unsafe {
            AvSetMmThreadCharacteristicsW(w!("Capture"), &mut task)?
        }))
    }
}

impl Drop for CapturePriority {
    fn drop(&mut self) {
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(self.0);
        }
    }
}
