//! Immediate-output software encoding; no input queue or future-frame lookahead.
mod h264;
use crate::{Codec, Format};
use anyhow::{Result, ensure};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Software streaming limits, also used by capability negotiation.
pub const fn maximum_size(codec: Codec) -> (u32, u32) {
    match codec {
        Codec::H264 => (3840, 2160),
        Codec::Av1 | Codec::H265 => (0, 0),
    }
}
pub const fn maximum_fps(codec: Codec) -> u32 {
    match codec {
        Codec::H264 => 144,
        Codec::Av1 | Codec::H265 => 0,
    }
}
#[derive(Clone, Copy)]
pub struct Config {
    pub width: u32,
    pub height: u32,
    pub format: Format,
    pub fps: u32,
    pub bitrate: u32,
}
impl Config {
    fn validate(self) -> Result<()> {
        ensure!(
            self.format.can_encode(),
            "unsupported software encoding format"
        );
        let maximum = maximum_size(self.format.codec);
        ensure!(
            self.width >= 2
                && self.height >= 2
                && self.width <= maximum.0
                && self.height <= maximum.1
                && self.width % 2 == 0
                && self.height % 2 == 0,
            "invalid software encoding dimensions"
        );
        ensure!(
            (1..=maximum_fps(self.format.codec)).contains(&self.fps)
                && (1..=500_000_000).contains(&self.bitrate),
            "invalid software encoding rate"
        );
        Ok(())
    }
}
pub struct Packet {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub timestamp_100ns: i64,
    /// Long-term reference slot this packet filled.
    pub marked: Option<u8>,
    /// Long-term reference slot this P picture predicts from.
    pub used: Option<u8>,
}

/// Long-term reference control of one frame (H.264 only).
pub use farwing_h264::encoder::References;
pub struct Encoder {
    kernel: h264::Encoder,
    config: Config,
    key_requested: bool,
    needs_reset: bool,
    prepared: bool,
}
impl Encoder {
    pub fn new(config: Config) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            kernel: h264::Encoder::new(config)?,
            config,
            key_requested: true,
            needs_reset: false,
            prepared: false,
        })
    }
    pub fn request_keyframe(&mut self) {
        self.key_requested = true;
    }
    pub fn configure(&mut self, fps: u32, bitrate: u32) -> Result<()> {
        let next = Config {
            fps,
            bitrate,
            ..self.config
        };
        next.validate()?;
        if self.needs_reset {
            self.kernel = h264::Encoder::new(next)?;
            self.needs_reset = false;
            self.key_requested = true;
        } else {
            self.kernel.configure(fps, bitrate)?;
        }
        self.config = next;
        Ok(())
    }
    /// Convert borrowed packed pixels into the reusable planes. The caller may
    /// unmap its GPU staging resource before the actual encoding starts.
    pub fn prepare(&mut self, data: &[u8], pitch: usize, cancel: &AtomicBool) -> Result<()> {
        self.prepared = false;
        ensure!(
            !cancel.load(Ordering::Acquire),
            "software encoding cancelled"
        );
        if self.needs_reset {
            self.kernel = h264::Encoder::new(self.config)?;
            self.needs_reset = false;
        }
        self.kernel.prepare(data, pitch)?;
        ensure!(
            !cancel.load(Ordering::Acquire),
            "software encoding cancelled"
        );
        self.prepared = true;
        Ok(())
    }
    pub fn encode(
        &mut self,
        timestamp: i64,
        key: bool,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Option<Packet>> {
        self.encode_with(timestamp, key, References::default(), cancel)
    }
    pub fn encode_with(
        &mut self,
        timestamp: i64,
        key: bool,
        refs: References,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Option<Packet>> {
        self.key_requested |= key;
        ensure!(
            std::mem::take(&mut self.prepared),
            "no prepared software frame"
        );
        ensure!(
            !cancel.load(Ordering::Acquire),
            "software encoding cancelled"
        );
        let result = self.kernel.encode(timestamp, self.key_requested, refs);
        if result.is_err() || cancel.load(Ordering::Acquire) {
            self.needs_reset = true;
            self.key_requested = true;
            ensure!(
                !cancel.load(Ordering::Acquire),
                "software encoding cancelled"
            );
        } else if result.as_ref().is_ok_and(Option::is_some) {
            self.key_requested = false;
        }
        result
    }
}
