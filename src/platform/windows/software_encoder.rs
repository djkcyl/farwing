//! BGRA readback for the Rust H.264 encoder. No codec algorithms here.
use super::format::{Encoded, Format, Rate};
use crate::i18n::t;
use anyhow::{Context, Result, ensure};
use farwing_codec::encoder::{Config, Encoder as Core};
use std::sync::{Arc, atomic::AtomicBool};
use windows::Win32::Graphics::{Direct3D11::*, Dxgi::Common::*};

pub(crate) struct Encoder {
    core: Core,
    staging: ID3D11Texture2D,
    context: ID3D11DeviceContext,
    size: (u32, u32),
    format: Format,
    rate: Rate,
    frame_rate: super::encoder_rate::Controller,
}
impl Encoder {
    pub fn new(
        device: &ID3D11Device,
        size: (u32, u32),
        format: Format,
        rate: Rate,
    ) -> Result<Self> {
        let core = Core::new(Config {
            width: size.0,
            height: size.1,
            format: format.software(),
            fps: rate.fps,
            bitrate: rate.target,
        })?;
        let mut desc = D3D11_TEXTURE2D_DESC {
            Width: size.0,
            Height: size.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.MiscFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        unsafe {
            device.CreateTexture2D(&desc, None, Some(&mut staging))?;
        }
        Ok(Self {
            core,
            staging: staging.context(t().system.encode_software_readback_texture)?,
            context: unsafe { device.GetImmediateContext()? },
            size,
            format,
            rate,
            frame_rate: super::encoder_rate::Controller::new(rate),
        })
    }
    pub fn request_keyframe(&mut self) {
        self.core.request_keyframe();
    }
    pub fn maximum_size(&self) -> (u32, u32) {
        farwing_codec::encoder::maximum_size(self.format.codec.media())
    }
    pub fn configure(&mut self, rate: Rate) -> Result<bool> {
        let Some(update) = self.frame_rate.decide(rate) else {
            return Ok(false);
        };
        self.core.configure(update.rate.fps, update.rate.target)?;
        let key = self.rate.quality != rate.quality;
        self.rate = rate;
        self.frame_rate.commit(update);
        tracing::debug!(
            requested_fps = rate.fps,
            configured_fps = update.rate.fps,
            target = update.rate.target,
            "software encoder rate applied"
        );
        Ok(key)
    }
    pub fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp: i64,
        key: bool,
        control: super::encoder::FrameControl,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Vec<Encoded>> {
        self.frame_rate.input(timestamp);
        if key {
            self.core.request_keyframe();
        }
        unsafe {
            self.context.CopyResource(&self.staging, texture);
        }
        let mapped = super::gpu_readback::map(&self.context, &self.staging, || {
            !cancel.load(std::sync::atomic::Ordering::Acquire)
        })?;
        let prepared = (|| {
            let (row, rows) = farwing_codec::PixelFormat::Bgra
                .layout(self.size.0 as usize, self.size.1 as usize)
                .context(t().system.encode_software_readback_overflow)?;
            let pitch = mapped.RowPitch as usize;
            ensure!(
                !mapped.pData.is_null() && pitch >= row,
                "{}",
                t().system.encode_software_invalid_pitch
            );
            let len = pitch
                .checked_mul(rows)
                .context(t().system.encode_software_readback_overflow)?;
            // SAFETY: Map owns the staging allocation until Unmap below. Pitch
            // and row count are checked against its configured pixel layout.
            let bytes = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), len) };
            self.core.prepare(bytes, pitch, cancel)
        })();
        unsafe {
            self.context.Unmap(&self.staging, 0);
        }
        prepared?;
        Ok(self
            .core
            .encode_with(
                timestamp,
                key,
                farwing_codec::encoder::References {
                    mark: control.mark_ltr,
                    use_long_term: control.use_ltr,
                },
                cancel,
            )?
            .into_iter()
            .map(|p| Encoded {
                data: p.data,
                keyframe: p.keyframe,
                timestamp_100ns: p.timestamp_100ns,
                is_new: true,
                timing: None,
                meta: None,
                ltr: crate::media::encoding::LtrOutput {
                    marked: p.marked,
                    used: p.used,
                },
                format: self.format,
                color: super::format::Color::Sdr.space(None),
            })
            .collect())
    }
}
