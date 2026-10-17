//! Private PCM bridge to OpenUUYC Audio. One privileged session owns the handle.
mod defaults;
mod defaults_watch;
pub(crate) mod install;
use super::host_service::pipe::Handle;
use anyhow::{Result, ensure};
pub(crate) use defaults::{Defaults, Routing};
use windows::{
    Win32::{
        Foundation::*,
        Storage::FileSystem::*,
        System::{IO::*, Threading::*},
    },
    core::w,
};

/// Whether opening the bridge failed because the driver device is not there,
/// as opposed to a driver that is present but failed to start.
pub(crate) fn unavailable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<windows::core::Error>()
        .is_some_and(|e| {
            [
                ERROR_FILE_NOT_FOUND,
                ERROR_PATH_NOT_FOUND,
                ERROR_DEV_NOT_EXIST,
                ERROR_DEVICE_NOT_CONNECTED,
            ]
            .iter()
            .any(|code| e.code() == code.to_hresult())
        })
}

pub(crate) const FRAMES: usize = 480;
pub(crate) struct AudioPriority(HANDLE);
impl AudioPriority {
    pub fn enter() -> Result<Self> {
        let mut task = 0;
        Ok(Self(unsafe {
            AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task)?
        }))
    }
}
impl Drop for AudioPriority {
    fn drop(&mut self) {
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(self.0);
        }
    }
}
const ABI: u32 = 1;
const BASE: u32 = (0x22 << 16) | (3 << 14) | (0x900 << 2);
#[repr(C)]
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct State {
    pub abi: u32,
    pub flags: u32,
    pub speaker_running: u32,
    pub microphone_running: u32,
    pub speaker_frames: u32,
    pub microphone_frames: u32,
    reserved: [u32; 2],
    pub sequence: u64,
    pub generation: u64,
    pub speaker_dropped: u64,
    pub microphone_underrun: u64,
}
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Pcm {
    abi: u32,
    frames: u32,
    generation: u64,
    samples: [i16; FRAMES * 2],
}
pub(crate) struct Bridge {
    handle: Handle,
    event: Handle,
    pcm: Pcm,
}
impl Bridge {
    pub fn open() -> Result<Self> {
        let handle = Handle(
            unsafe {
                CreateFileW(
                    w!(r"\\.\OpenUUYCAudioBridge"),
                    GENERIC_READ.0 | GENERIC_WRITE.0,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    None,
                )
            }
            .map_err(install::bridge_open_error)?,
        );
        let event = Handle(unsafe { CreateEventW(None, true, false, None)? });
        let mut bridge = Self {
            handle,
            event,
            pcm: bytemuck::Zeroable::zeroed(),
        };
        ensure!(bridge.state()?.abi == ABI, "虚拟音频驱动接口版本不匹配");
        Ok(bridge)
    }
    fn call(
        handle: HANDLE,
        event: HANDLE,
        code: u32,
        input: &[u8],
        output: &mut [u8],
        timeout: u32,
    ) -> Result<Option<usize>> {
        let mut overlapped = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        let mut count = 0;
        unsafe {
            ResetEvent(event)?;
            match DeviceIoControl(
                handle,
                BASE + code * 4,
                (!input.is_empty()).then_some(input.as_ptr().cast()),
                input.len() as u32,
                (!output.is_empty()).then_some(output.as_mut_ptr().cast()),
                output.len() as u32,
                Some(&mut count),
                Some(&mut overlapped),
            ) {
                Ok(()) => {}
                Err(error) if error.code() == ERROR_IO_PENDING.to_hresult() => {
                    if WaitForSingleObject(event, timeout) != WAIT_OBJECT_0 {
                        let _ = CancelIoEx(handle, Some(&overlapped));
                        // Both buffers and OVERLAPPED remain alive until cancellation finishes.
                        match GetOverlappedResult(handle, &overlapped, &mut count, true) {
                            Ok(()) => return Ok(Some(count as usize)),
                            Err(error) if error.code() == ERROR_OPERATION_ABORTED.to_hresult() => {
                                return Ok(None);
                            }
                            Err(error) => return Err(error.into()),
                        }
                    }
                    GetOverlappedResult(handle, &overlapped, &mut count, false)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Some(count as usize))
    }
    pub fn state(&mut self) -> Result<State> {
        let mut state = State::default();
        let bytes = Self::call(
            self.handle.0,
            self.event.0,
            0,
            &[],
            bytemuck::bytes_of_mut(&mut state),
            500,
        )?;
        ensure!(bytes == Some(size_of::<State>()), "虚拟音频状态读取不完整");
        ensure!(state.abi == ABI, "虚拟音频驱动接口版本不匹配");
        Ok(state)
    }
    pub fn enable(&mut self, speaker: bool, microphone: bool) -> Result<State> {
        let fields = [ABI, u32::from(speaker) | (u32::from(microphone) << 1)];
        ensure!(
            Self::call(
                self.handle.0,
                self.event.0,
                1,
                bytemuck::cast_slice(&fields),
                &mut [],
                500
            )?
            .is_some(),
            "虚拟音频设置超时"
        );
        self.state()
    }
    pub fn wait(&mut self, previous: &State, timeout_ms: u32) -> Result<Option<State>> {
        let mut next = State::default();
        match Self::call(
            self.handle.0,
            self.event.0,
            4,
            bytemuck::bytes_of(previous),
            bytemuck::bytes_of_mut(&mut next),
            timeout_ms,
        )? {
            None => Ok(None),
            Some(bytes) => {
                ensure!(
                    bytes == size_of::<State>() && next.abi == ABI,
                    "虚拟音频通知无效"
                );
                Ok(Some(next))
            }
        }
    }
    pub fn write(&mut self, state: &State, samples: &[f32]) -> Result<()> {
        ensure!(
            !samples.is_empty() && samples.len() <= FRAMES * 2 && samples.len().is_multiple_of(2),
            "虚拟麦克风PCM长度无效"
        );
        self.pcm.abi = ABI;
        self.pcm.frames = (samples.len() / 2) as u32;
        self.pcm.generation = state.generation;
        for (dst, src) in self.pcm.samples.iter_mut().zip(samples) {
            *dst = (src.clamp(-1.0, 1.0) * 32768.0)
                .round()
                .clamp(-32768.0, 32767.0) as i16;
        }
        let input = &bytemuck::bytes_of(&self.pcm)[..16 + samples.len() * 2];
        ensure!(
            Self::call(self.handle.0, self.event.0, 3, input, &mut [], 500)?.is_some(),
            "虚拟麦克风写入超时"
        );
        Ok(())
    }
}
// Closing the file resets the driver lease and clears both PCM queues, including
// process crashes. No shutdown IO is needed while unwinding an error.
