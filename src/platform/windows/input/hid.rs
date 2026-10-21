//! Private interface of the Farwing Input driver.
use anyhow::{Result, ensure};
use windows::{
    Win32::{
        Foundation::*,
        Storage::FileSystem::*,
        System::{IO::*, Threading::*},
    },
    core::w,
};

const BEGIN: u32 = 0x22a000;
const SUBMIT: u32 = 0x22a004;
const RESET: u32 = 0x22a008;
const ALIVE: u32 = 0x22a00c;
pub(crate) const MAGIC: u32 = 0x4657494e; // "FWIN"
pub(crate) const VERSION: u32 = 1;

pub(crate) struct Device {
    handle: HANDLE,
    epoch: u64,
    sequence: u64,
    alive: std::time::Instant,
}
impl Device {
    pub fn open() -> Result<Option<Self>> {
        let handle = match unsafe {
            CreateFileW(
                w!(r"\\.\FarwingInput"),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        } {
            Ok(h) => h,
            Err(e)
                if e.code() == ERROR_FILE_NOT_FOUND.to_hresult()
                    || e.code() == ERROR_PATH_NOT_FOUND.to_hresult() =>
            {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let mut device = Self {
            handle,
            epoch: rand::random::<u64>().max(1),
            sequence: 0,
            alive: std::time::Instant::now(),
        };
        let mut bytes = [0u8; 16];
        bytes[..4].copy_from_slice(&MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(&VERSION.to_le_bytes());
        bytes[8..].copy_from_slice(&device.epoch.to_le_bytes());
        device.ioctl(BEGIN, &bytes)?;
        Ok(Some(device))
    }
    fn ioctl(&mut self, code: u32, bytes: &[u8]) -> Result<()> {
        let mut count = 0;
        let event = super::super::host_service::pipe::Handle(unsafe {
            CreateEventW(None, true, false, None)?
        });
        let mut overlapped = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        unsafe {
            let result = DeviceIoControl(
                self.handle,
                code,
                Some(bytes.as_ptr().cast()),
                bytes.len() as u32,
                None,
                0,
                Some(&mut count),
                Some(&mut overlapped),
            );
            match result {
                Ok(()) => {}
                Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {
                    if WaitForSingleObject(event.0, 500) != WAIT_OBJECT_0 {
                        let _ = CancelIoEx(self.handle, Some(&overlapped));
                        let _ = GetOverlappedResult(self.handle, &overlapped, &mut count, true);
                        anyhow::bail!("输入驱动请求超时");
                    }
                    GetOverlappedResult(self.handle, &overlapped, &mut count, false)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
    pub fn report(&mut self, data: &[u8]) -> Result<()> {
        ensure!(data.len() <= 256 && !data.is_empty(), "HID报告长度无效");
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("输入序号已耗尽"))?;
        let mut bytes = Vec::with_capacity(28 + data.len());
        bytes.extend(MAGIC.to_le_bytes());
        bytes.extend(VERSION.to_le_bytes());
        bytes.extend(self.epoch.to_le_bytes());
        bytes.extend(self.sequence.to_le_bytes());
        bytes.extend((data.len() as u32).to_le_bytes());
        bytes.extend(data);
        self.ioctl(SUBMIT, &bytes)
    }
    pub fn reset(&mut self) -> Result<()> {
        self.ioctl(RESET, &self.epoch.to_le_bytes())
    }
    pub fn tick(&mut self) -> Result<()> {
        if self.alive.elapsed() >= std::time::Duration::from_millis(100) {
            self.ioctl(ALIVE, &self.epoch.to_le_bytes())?;
            self.alive = std::time::Instant::now();
        }
        Ok(())
    }
}
impl Drop for Device {
    fn drop(&mut self) {
        let _ = self.reset();
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Bit of the driver's consumer report for a consumer-page usage.
pub(crate) fn consumer(usage: u32) -> Option<u8> {
    if usage >> 16 != 0x0c {
        return None;
    }
    Some(match usage & 0xffff {
        0xb5 => 0, // Next Track
        0xb6 => 1, // Previous Track
        0xcd => 2, // Play/Pause
        0xb7 => 3, // Stop
        0xe2 => 4, // Mute
        0xe9 => 5, // Volume Up
        0xea => 6, // Volume Down
        _ => return None,
    })
}

/// Bit of the driver's keyboard report (usages 0x04..=0xE7 of page 0x07).
pub(crate) fn keyboard(usage: u32) -> Option<u8> {
    if usage >> 16 != 0x07 {
        return None;
    }
    u8::try_from(usage & 0xffff)
        .ok()
        .filter(|id| (0x04..=0xe7).contains(id))
}
