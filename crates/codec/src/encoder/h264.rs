use super::{Config, Packet};
use anyhow::{Result, ensure};
use farwing_h264::encoder::{Config as CoreConfig, Encoder as Core};

pub(super) struct Encoder {
    core: Core,
    size: (u32, u32),
    planar: Vec<u8>,
}
impl Encoder {
    pub fn new(config: Config) -> Result<Self> {
        Ok(Self {
            core: Core::new(CoreConfig {
                width: config.width,
                height: config.height,
                fps: config.fps,
                bitrate: config.bitrate,
            })?,
            size: (config.width, config.height),
            planar: vec![0; config.width as usize * config.height as usize * 3 / 2],
        })
    }
    pub fn configure(&mut self, fps: u32, bitrate: u32) -> Result<()> {
        self.core.configure(fps, bitrate)?;
        Ok(())
    }
    pub fn prepare(&mut self, bgra: &[u8], pitch: usize) -> Result<()> {
        let (width, height) = self.size;
        ensure!(
            pitch >= width as usize * 4
                && bgra.len()
                    >= pitch
                        .checked_mul(height as usize)
                        .ok_or_else(|| anyhow::anyhow!("BGRA size overflow"))?,
            "invalid BGRA input"
        );
        let luma = width as usize * height as usize;
        let (y, uv) = self.planar.split_at_mut(luma);
        let (u, v) = uv.split_at_mut(luma / 4);
        yuv::bgra_to_yuv420(
            &mut yuv::YuvPlanarImageMut {
                y_plane: yuv::BufferStoreMut::Borrowed(y),
                y_stride: width,
                u_plane: yuv::BufferStoreMut::Borrowed(u),
                u_stride: width / 2,
                v_plane: yuv::BufferStoreMut::Borrowed(v),
                v_stride: width / 2,
                width,
                height,
            },
            bgra,
            u32::try_from(pitch)?,
            yuv::YuvRange::Limited,
            yuv::YuvStandardMatrix::Bt601,
            yuv::YuvConversionMode::Balanced,
        )?;
        Ok(())
    }
    pub fn encode(
        &mut self,
        timestamp: i64,
        key: bool,
        refs: farwing_h264::encoder::References,
    ) -> Result<Option<Packet>> {
        let luma = self.size.0 as usize * self.size.1 as usize;
        Ok(self
            .core
            .encode_with(
                &self.planar[..luma],
                &self.planar[luma..luma + luma / 4],
                &self.planar[luma + luma / 4..],
                timestamp,
                key,
                refs,
            )?
            .map(|unit| Packet {
                data: unit.data,
                keyframe: unit.keyframe,
                timestamp_100ns: unit.timestamp_100ns,
                marked: unit.marked,
                used: unit.used,
            }))
    }
}
