//! OpenUUYC Audio is a Windows driver. Linux has no virtual microphone yet
//! (a PipeWire virtual source would be the counterpart), so the bridge cannot
//! be opened and changing default devices is refused with a reason.
use anyhow::{Result, bail};

const UNSUPPORTED: &str = "Linux 被控端暂不支持虚拟麦克风";

/// No real-time audio class on Linux; the thread keeps its scheduling.
pub(crate) struct AudioPriority;
impl AudioPriority {
    pub fn enter() -> Result<Self> {
        bail!("Linux 未设置音频线程优先级")
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct State {
    pub speaker_running: u32,
    pub microphone_running: u32,
    pub microphone_frames: u32,
    pub generation: u64,
    pub microphone_underrun: u64,
}

pub(crate) enum Bridge {}
impl Bridge {
    pub fn open() -> Result<Self> {
        bail!(UNSUPPORTED)
    }
    pub fn state(&mut self) -> Result<State> {
        match *self {}
    }
    pub fn enable(&mut self, _speaker: bool, _microphone: bool) -> Result<State> {
        match *self {}
    }
    pub fn wait(&mut self, _previous: &State, _timeout_ms: u32) -> Result<Option<State>> {
        match *self {}
    }
    pub fn write(&mut self, _state: &State, _samples: &[f32]) -> Result<()> {
        match *self {}
    }
}

/// The bridge only fails to open because the component does not exist here.
pub(crate) fn unavailable(_error: &anyhow::Error) -> bool {
    true
}

#[derive(Default)]
pub(crate) struct Routing;
impl Routing {
    pub fn apply(&mut self, speakers: bool, microphone: bool) -> Result<()> {
        if speakers || microphone {
            bail!(UNSUPPORTED);
        }
        Ok(())
    }
    pub fn maintain(&mut self) -> Result<()> {
        Ok(())
    }
}
