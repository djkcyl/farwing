//! Rust D3D11 decoding for Windows 4:2:0 / HEVC and AV1 4:4:4 playback.
//! Hardware preparation never calls the native video bridge.
mod av1;
mod av1_params;
mod avc;
pub(super) mod dxva;
mod hevc;
mod output;
mod params;

use super::*;

/// Decoded pictures dropped from display because every display surface was
/// in use.
pub(super) static DISPLAY_BUSY_DROPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

enum Decoder {
    Avc(avc::Avc),
    Hevc(hevc::Hevc),
    Av1(av1::Av1),
}
pub(super) struct Session {
    decoder: Decoder,
    output: output::Output,
    extra: Option<Bytes>,
    dump: Option<std::fs::File>,
}
/// `FARWING_DECODER_DUMP=<directory>`: every access unit given to the
/// decoder, appended to one file per session (for checking a stream with
/// another decoder).
fn dump_file(codec: VideoCodec) -> Option<std::fs::File> {
    let directory = std::env::var_os("FARWING_DECODER_DUMP")?;
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let extension = match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::H265 => "h265",
        VideoCodec::Av1 => "obu",
    };
    let path = std::path::Path::new(&directory).join(format!(
        "{}-{}.{extension}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::File::create(path).ok()
}
impl Session {
    pub(super) fn open(config: &VideoDecoderConfig) -> Result<Self, DecodeError> {
        if config.width == 0 || config.height == 0 || config.width > 16384 || config.height > 16384
        {
            return Err(DecodeError::InvalidInput);
        }
        let device = config
            .gpu_device
            .as_ref()
            .ok_or(DecodeError::InvalidInput)?
            .clone();
        if config.codec != VideoCodec::Av1
            && !config.extra_data.is_empty()
            && !config.extra_data.starts_with(&[0, 0, 1])
            && !config.extra_data.starts_with(&[0, 0, 0, 1])
        {
            return Err(DecodeError::Unsupported);
        }
        let decoder = match config.codec {
            VideoCodec::H264 => Decoder::Avc(avc::Avc::new(device)),
            VideoCodec::H265 => Decoder::Hevc(hevc::Hevc::new(device)),
            VideoCodec::Av1 => Decoder::Av1(av1::Av1::new(device)),
        };
        tracing::info!(codec=?config.codec, "Rust DXVA session created");
        Ok(Self {
            decoder,
            output: output::Output::default(),
            extra: Some(config.extra_data.clone()),
            dump: dump_file(config.codec),
        })
    }
    pub(super) fn probe(
        handle: ID3D11Device,
        codec: VideoCodec,
        w: u32,
        h: u32,
        depth: u8,
        chroma: u8,
    ) -> bool {
        Self::check_format(handle, codec, w, h, depth, chroma).is_ok()
    }
    pub(super) fn check_format(
        handle: ID3D11Device,
        codec: VideoCodec,
        w: u32,
        h: u32,
        depth: u8,
        chroma: u8,
    ) -> anyhow::Result<()> {
        let device = &handle;
        let codec = match codec {
            VideoCodec::H264 => dxva::Codec::H264,
            VideoCodec::H265 => dxva::Codec::Hevc,
            VideoCodec::Av1 => dxva::Codec::Av1,
        };
        dxva::Pool::probe(device, codec, w, h, depth, chroma)
    }
    pub(super) fn push(
        &mut self,
        payload: &[u8],
        token: i64,
        notification: &DecoderNotification,
    ) -> Result<(), DecodeError> {
        if notification.is_cancelled() {
            return Err(DecodeError::Closed);
        }
        let extra = self.extra.take().filter(|e| !e.is_empty());
        let combined;
        let data = if let Some(extra) = extra {
            combined = [extra.as_ref(), payload].concat();
            &combined[..]
        } else {
            payload
        };
        if let Some(file) = self.dump.as_mut() {
            use std::io::Write;
            let _ = file.write_all(data);
        }
        let decoded = match &mut self.decoder {
            Decoder::Avc(d) => d.decode(data, notification.cancellation()).map(Some),
            Decoder::Hevc(d) => d.decode(data, notification.cancellation()),
            Decoder::Av1(d) => d.decode(data, notification.cancellation()),
        };
        let result = decoded.and_then(|picture| {
            if let Some(picture) = picture {
                self.output.push(output::Frame {
                    picture,
                    token: token,
                })
            } else {
                self.output.discard_token(token);
                Ok(())
            }
        });
        result.map_err(|error| {
            self.reset();
            if notification.is_cancelled() {
                return DecodeError::Closed;
            }
            tracing::warn!(detail=%error, "Rust DXVA decode failed");
            match error.downcast_ref::<dxva::Failure>() {
                Some(dxva::Failure::Unsupported) => DecodeError::Unsupported,
                Some(dxva::Failure::HardwareFailure) => DecodeError::HardwareFailure,
                Some(dxva::Failure::OutputBusy) => DecodeError::NeedKeyframe,
                None if error.downcast_ref::<windows::core::Error>().is_some() => {
                    DecodeError::HardwareFailure
                }
                None => DecodeError::NeedKeyframe,
            }
        })
    }
    pub(super) fn poll(&mut self) -> Result<Option<WindowsDecodedFrame>, DecodeError> {
        loop {
            let Some(frame) = self.output.poll() else {
                return Ok(None);
            };
            let picture = frame.picture;
            // Keep queued pictures in the decode DPB. The smaller shared display
            // pool is acquired only once the picture is actually ready for output.
            let pool = std::sync::Arc::clone(&picture.surface.pool);
            let surface = match pool.display(picture.surface) {
                Ok(surface) => surface,
                Err(error)
                    if matches!(
                        error.downcast_ref::<dxva::Failure>(),
                        Some(dxva::Failure::OutputBusy)
                    ) =>
                {
                    // Display pressure may drop a presentation, never a decoder
                    // reference. Retire its token and keep the DPB/decoder intact.
                    DISPLAY_BUSY_DROPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    self.output.discard_token(frame.token);
                    continue;
                }
                Err(error) => {
                    tracing::warn!(detail=%error, "Rust DXVA output transfer failed");
                    self.reset();
                    return Err(DecodeError::HardwareFailure);
                }
            };
            return Ok(Some(WindowsDecodedFrame::Gpu(WindowsGpuVideoFrame {
                texture: surface.texture().clone(),
                subresource: surface.subresource(),
                pts: frame.token,
                visible_x: picture.left,
                visible_y: picture.top,
                width: picture.width,
                height: picture.height,
                _lease: surface,
            })));
        }
    }
    pub(super) fn reset(&mut self) {
        self.output.reset();
        match &mut self.decoder {
            Decoder::Avc(d) => d.reset(),
            Decoder::Hevc(d) => d.reset(),
            Decoder::Av1(d) => d.reset(),
        }
    }
    pub(super) fn poll_dropped(&mut self) -> Option<i64> {
        self.output.poll_dropped()
    }
}
