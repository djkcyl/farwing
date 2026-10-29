//! Private PCM bridge to the Farwing Audio driver. One privileged session owns the handle.
mod defaults;
mod defaults_watch;
pub(crate) mod install;
use super::host_service::pipe::Handle;
use anyhow::{Result, ensure};
use crate::i18n::t;
pub(crate) use defaults::{Defaults, Routing};
use windows::{
    Win32::{
        Foundation::*,
        Storage::FileSystem::*,
        System::{IO::*, Threading::*},
    },
    core::w,
};

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
    /// A handle to the bridge as this process may open it.
    pub(crate) fn raw() -> Result<Handle> {
        Ok(Handle(
            unsafe {
                CreateFileW(
                    w!(r"\\.\FarwingAudioBridge"),
                    GENERIC_READ.0 | GENERIC_WRITE.0,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    None,
                )
            }
            .map_err(install::bridge_open_error)?,
        ))
    }
    pub fn open() -> Result<Self> {
        // The background host gets its handle from the service.
        let handle = if crate::platform::windows::host_service::resident::is_owner() {
            crate::platform::windows::host_service::requests::device(
                crate::platform::windows::host_service::requests::Device::AudioBridge,
            )?
        } else {
            Self::raw()?
        };
        let event = Handle(unsafe { CreateEventW(None, true, false, None)? });
        let mut bridge = Self {
            handle,
            event,
            pcm: bytemuck::Zeroable::zeroed(),
        };
        ensure!(bridge.state()?.abi == ABI, "{}", t().system.audio_driver_abi_mismatch);
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
        ensure!(bytes == Some(size_of::<State>()), "{}", t().system.audio_state_incomplete);
        ensure!(state.abi == ABI, "{}", t().system.audio_driver_abi_mismatch);
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
            "{}",
            t().system.audio_enable_timeout
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
                    "{}",
                    t().system.audio_notification_invalid
                );
                Ok(Some(next))
            }
        }
    }
    pub fn write(&mut self, state: &State, samples: &[f32]) -> Result<()> {
        ensure!(
            !samples.is_empty() && samples.len() <= FRAMES * 2 && samples.len().is_multiple_of(2),
            "{}",
            t().system.audio_microphone_pcm_invalid
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
            "{}",
            t().system.audio_microphone_write_timeout
        );
        Ok(())
    }
}
// Closing the file resets the driver lease and clears both PCM queues, including
// process crashes. No shutdown IO is needed while unwinding an error.
