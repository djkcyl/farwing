//! Single-access-unit AV1 software session. References stay in the Rust core;
//! only complete, uncancelled pictures are transferred to the presenter.
use super::{Error, Frame};
use crate::PixelFormat;
use bytes::Bytes;
use farwing_av1::decoder::{Decoder, Picture, Rav1dError, Settings};
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) struct Session {
    decoder: Decoder,
    sequence: Bytes,
}
impl Session {
    pub fn new(extra: &[u8]) -> Result<Self, Error> {
        let mut settings = Settings::default();
        // Tile workers can run in parallel, but there is no frame lookahead.
        settings.set_n_threads(
            std::thread::available_parallelism()
                .map_or(1, |v| v.get())
                .min(4) as u32,
        );
        settings.set_max_frame_delay(1);
        settings.set_all_layers(false);
        settings.set_frame_size_limit(16384 * 16384);
        let mut this = Self {
            decoder: Decoder::with_settings(&settings).map_err(error)?,
            sequence: Bytes::new(),
        };
        if !extra.is_empty() {
            this.push(extra, 0, &AtomicBool::new(false))?;
        }
        Ok(this)
    }
    pub fn set_cancellation(&mut self, signal: std::sync::Arc<AtomicBool>) {
        self.decoder.set_cancellation(Some(signal));
    }
    pub fn reset(&mut self) -> Result<(), Error> {
        self.decoder.flush();
        if !self.sequence.is_empty() {
            self.decoder
                .send_data(self.sequence.to_vec().into_boxed_slice(), None, None, None)
                .map_err(error)?;
        }
        Ok(())
    }
    pub fn push(
        &mut self,
        data: &[u8],
        token: i64,
        cancel: &AtomicBool,
    ) -> Result<Option<Frame>, Error> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.push_inner(data, token, cancel)
        }))
        .unwrap_or(Err(Error::NeedKeyframe));
        let result = if cancel.load(Ordering::Acquire) {
            Err(Error::Closed)
        } else {
            result
        };
        if result.is_err() {
            let _ = self.reset();
        }
        result
    }
    fn push_inner(
        &mut self,
        data: &[u8],
        token: i64,
        cancel: &AtomicBool,
    ) -> Result<Option<Frame>, Error> {
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        let mut sequence = None;
        for unit in
            farwing_av1::bitstream::obu::Units::new(data).map_err(|_| Error::NeedKeyframe)?
        {
            let unit = unit.map_err(|_| Error::NeedKeyframe)?;
            if unit.spatial_id != 0 || unit.temporal_id != 0 {
                return Err(Error::Unsupported);
            }
            if unit.kind == 1 {
                let s = farwing_av1::bitstream::Sequence::parse(unit)
                    .map_err(|_| Error::NeedKeyframe)?;
                if !matches!(s.depth, 8 | 10)
                    || !matches!(
                        s.chroma,
                        farwing_av1::bitstream::Chroma::Yuv420
                            | farwing_av1::bitstream::Chroma::Yuv444
                    )
                {
                    return Err(Error::Unsupported);
                }
                sequence = Some(Bytes::copy_from_slice(unit.bytes));
            }
        }
        match self
            .decoder
            .send_data(data.to_vec().into_boxed_slice(), None, Some(token), None)
        {
            Ok(()) | Err(Rav1dError::TryAgain) => {}
            Err(e) => return Err(error(e)),
        }
        let mut output = None;
        loop {
            match self.decoder.get_picture() {
                Ok(p) => {
                    if output.is_some() || p.timestamp() != Some(token) {
                        return Err(Error::Unsupported);
                    }
                    output = Some(pack(&p, token, cancel)?);
                    match self.decoder.send_pending_data() {
                        Ok(()) | Err(Rav1dError::TryAgain) => {}
                        Err(e) => return Err(error(e)),
                    }
                }
                Err(Rav1dError::TryAgain) => break,
                Err(e) => return Err(error(e)),
            }
        }
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        if let Some(sequence) = sequence {
            self.sequence = sequence;
        }
        Ok(output)
    }
}
fn error(e: Rav1dError) -> Error {
    match e {
        Rav1dError::OutOfMemory => Error::Allocation,
        Rav1dError::UnsupportedBitstream => Error::Unsupported,
        _ => Error::NeedKeyframe,
    }
}
fn pack(p: &Picture, token: i64, cancel: &AtomicBool) -> Result<Frame, Error> {
    use farwing_av1::decoder::streaming::{PackError, PixelFormat as PackedFormat, pack_picture};
    let picture = pack_picture(p, cancel).map_err(|e| match e {
        PackError::Cancelled => Error::Closed,
        PackError::Unsupported => Error::Unsupported,
        PackError::Allocation => Error::Allocation,
    })?;
    Ok(Frame {
        pts: token,
        width: picture.width,
        height: picture.height,
        coded_width: picture.coded_width,
        coded_height: picture.coded_height,
        format: match picture.format {
            PackedFormat::Nv12 => PixelFormat::Nv12,
            PackedFormat::P010 => PixelFormat::P010,
            PackedFormat::Ayuv => PixelFormat::Ayuv,
            PackedFormat::Y410 => PixelFormat::Y410,
        },
        data: Bytes::from(picture.data),
    })
}
