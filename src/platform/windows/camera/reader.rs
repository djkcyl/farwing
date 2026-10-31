//! The device side of a camera capture: a Media Foundation source reader in
//! asynchronous mode (it decodes and converts to NV12) that keeps the newest
//! picture in system memory. Nothing blocks on the device: closing flushes
//! the reader and shuts the source down even when the camera stopped
//! delivering.
use super::{Apartment, Picture, attributes, failure, mode_of, native_types, split, startup};
use crate::i18n::t;
use crate::media::camera::{Matrix, PixelFormat, Spec};
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Media::MediaFoundation::*;
use windows::core::{HSTRING, Interface, implement};

const STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

/// The camera's media source, for its picture adjustments on other threads
/// (device sources are free-threaded).
pub(super) struct SharedSource(pub IMFMediaSource);
// SAFETY: Media Foundation device sources are free-threaded COM objects.
unsafe impl Send for SharedSource {}
unsafe impl Sync for SharedSource {}

#[derive(Default)]
struct State {
    picture: Option<Arc<Picture>>,
    sequence: u64,
    error: Option<String>,
    flushed: bool,
}

/// How the reader's pictures are laid out (changes with the media type).
#[derive(Clone, Copy)]
struct Layout {
    width: u32,
    height: u32,
    matrix: Matrix,
    full_range: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    stop: AtomicBool,
    layout: Mutex<Layout>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn fail(&self, error: String) {
        let mut state = lock(&self.state);
        state.error.get_or_insert(error);
        self.changed.notify_all();
    }
}

/// The reader's callback: stores each picture and asks for the next.
#[implement(IMFSourceReaderCallback)]
struct Callback {
    shared: Arc<Shared>,
    /// Cleared on close (the reader holds this callback).
    reader: Arc<Mutex<Option<IMFSourceReader>>>,
}

impl IMFSourceReaderCallback_Impl for Callback_Impl {
    fn OnReadSample(
        &self,
        status: windows::core::HRESULT,
        _stream: u32,
        flags: u32,
        _timestamp: i64,
        sample: windows::core::Ref<IMFSample>,
    ) -> windows::core::Result<()> {
        let shared = &self.shared;
        if shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if status.is_err() {
            shared.fail(format!("{:#}", failure(windows::core::Error::from_hresult(status))));
            return Ok(());
        }
        if flags & (MF_SOURCE_READERF_ERROR.0 | MF_SOURCE_READERF_ENDOFSTREAM.0) as u32 != 0 {
            shared.fail(t().camera.removed.to_owned());
            return Ok(());
        }
        let reader = lock(&self.reader).clone();
        if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0
            && let Some(size) = reader.as_ref().and_then(|r| unsafe {
                r.GetCurrentMediaType(STREAM)
                    .and_then(|t| t.GetUINT64(&MF_MT_FRAME_SIZE))
                    .ok()
            })
        {
            let mut layout = lock(&shared.layout);
            (layout.width, layout.height) = split(size);
        }
        if let Some(sample) = sample.as_ref() {
            let layout = *lock(&shared.layout);
            match copy(sample, layout.width, layout.height) {
                Ok(data) => {
                    let picture = Arc::new(Picture {
                        data,
                        width: layout.width,
                        height: layout.height,
                        captured: Instant::now(),
                        matrix: layout.matrix,
                        full_range: layout.full_range,
                    });
                    let mut state = lock(&shared.state);
                    state.picture = Some(picture);
                    state.sequence = state.sequence.wrapping_add(1).max(1);
                    shared.changed.notify_all();
                }
                Err(error) => {
                    shared.fail(format!("{}: {error:#}", t().camera.read_failed));
                    return Ok(());
                }
            }
        }
        if let Some(reader) = reader
            && let Err(error) = unsafe { reader.ReadSample(STREAM, 0, None, None, None, None) }
        {
            shared.fail(format!("{:#}", failure(error)));
        }
        Ok(())
    }

    fn OnFlush(&self, _stream: u32) -> windows::core::Result<()> {
        let mut state = lock(&self.shared.state);
        state.flushed = true;
        self.shared.changed.notify_all();
        Ok(())
    }

    fn OnEvent(&self, _stream: u32, _event: windows::core::Ref<IMFMediaEvent>) -> windows::core::Result<()> {
        Ok(())
    }
}

pub(super) struct Reader {
    shared: Arc<Shared>,
    /// What the callback reads from; emptied on close to break the cycle.
    pending: Arc<Mutex<Option<IMFSourceReader>>>,
    reader: IMFSourceReader,
    source: Arc<SharedSource>,
}

// SAFETY: the source reader and its callback are free-threaded; the
// capture only moves between threads, never shares them.
unsafe impl Send for Reader {}

impl Reader {
    pub(super) fn open(spec: &Spec) -> Result<Self> {
        let _apartment = Apartment::enter()?;
        startup()?;
        let device = attributes(2)?;
        let source = unsafe {
            device.SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )?;
            device.SetString(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                &HSTRING::from(spec.camera.as_str()),
            )?;
            MFCreateDeviceSource(&device).map_err(failure)?
        };
        let shared = Arc::new(Shared {
            state: Mutex::default(),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            layout: Mutex::new(Layout {
                width: spec.mode.width,
                height: spec.mode.height,
                matrix: Matrix::Bt601,
                full_range: false,
            }),
        });
        let pending = Arc::new(Mutex::new(None));
        let callback: IMFSourceReaderCallback = Callback {
            shared: shared.clone(),
            reader: pending.clone(),
        }
        .into();
        let opened = Self::configure(spec, &source, &callback, &shared, &pending);
        match opened {
            Ok(reader) => {
                tracing::info!(camera = %spec.name, width = spec.mode.width, height = spec.mode.height,
                    fps = spec.mode.fps(), format = spec.mode.format.label(), "camera capture started");
                Ok(Self {
                    shared,
                    pending,
                    reader,
                    source: Arc::new(SharedSource(source)),
                })
            }
            Err(error) => {
                unsafe {
                    let _ = source.Shutdown();
                }
                Err(error)
            }
        }
    }

    fn configure(
        spec: &Spec,
        source: &IMFMediaSource,
        callback: &IMFSourceReaderCallback,
        shared: &Shared,
        pending: &Mutex<Option<IMFSourceReader>>,
    ) -> Result<IMFSourceReader> {
        let options = attributes(3)?;
        let reader = unsafe {
            options.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;
            options.SetUINT32(&MF_LOW_LATENCY, 1)?;
            options.SetUnknown(&MF_SOURCE_READER_ASYNC_CALLBACK, callback)?;
            MFCreateSourceReaderFromMediaSource(source, &options).map_err(failure)?
        };
        // The device's format first, then NV12 out of the reader (its
        // decoder and converter) at the same size and rate.
        let native = native_types(&reader)
            .into_iter()
            .find(|t| mode_of(t) == Some(spec.mode))
            .context(t().camera.no_mode)?;
        let mode = spec.mode;
        unsafe {
            reader.SetCurrentMediaType(STREAM, None, &native).map_err(failure)?;
            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            output.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(mode.width) << 32) | u64::from(mode.height))?;
            output.SetUINT64(
                &MF_MT_FRAME_RATE,
                (u64::from(mode.fps_numerator) << 32) | u64::from(mode.fps_denominator),
            )?;
            reader.SetCurrentMediaType(STREAM, None, &output).map_err(failure)?;
            let current = reader.GetCurrentMediaType(STREAM)?;
            let (width, height) = split(current.GetUINT64(&MF_MT_FRAME_SIZE)?);
            anyhow::ensure!(
                width >= 2 && height >= 2 && width % 2 == 0 && height % 2 == 0,
                "{}",
                t().camera.no_mode
            );
            *lock(&shared.layout) = Layout {
                width,
                height,
                matrix: match current.GetUINT32(&MF_MT_YUV_MATRIX) {
                    Ok(value) if value == MFVideoTransferMatrix_BT709.0 as u32 => Matrix::Bt709,
                    _ => Matrix::Bt601,
                },
                // JPEG pictures are full range unless the decoder says otherwise.
                full_range: current
                    .GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE)
                    .map(|value| value == MFNominalRange_0_255.0 as u32)
                    .unwrap_or(mode.format == PixelFormat::Mjpeg),
            };
        }
        *lock(pending) = Some(reader.clone());
        unsafe { reader.ReadSample(STREAM, 0, None, None, None, None) }.map_err(failure)?;
        Ok(reader)
    }

    pub(super) fn media_source(&self) -> Arc<SharedSource> {
        self.source.clone()
    }

    /// The newest picture after `seen`, waiting at most `timeout` ms.
    pub(super) fn next(&mut self, seen: u64, timeout: u32) -> Result<Option<(u64, Arc<Picture>)>> {
        let state = lock(&self.shared.state);
        let (state, _) = self
            .shared
            .changed
            .wait_timeout_while(state, Duration::from_millis(timeout.into()), |s| {
                s.sequence == seen && s.error.is_none()
            })
            .unwrap_or_else(|e| e.into_inner());
        if let Some(error) = &state.error {
            anyhow::bail!("{error}");
        }
        Ok((state.sequence != seen)
            .then(|| state.picture.clone().map(|p| (state.sequence, p)))
            .flatten())
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        lock(&self.pending).take();
        // A pending read ends with the flush; wait for it briefly.
        if unsafe { self.reader.Flush(STREAM) }.is_ok() {
            let state = lock(&self.shared.state);
            let _ = self
                .shared
                .changed
                .wait_timeout_while(state, Duration::from_millis(500), |s| !s.flushed);
        }
        unsafe {
            let _ = self.source.0.Shutdown();
        }
    }
}

/// The NV12 picture of `sample` as rows of `width` bytes.
fn copy(sample: &IMFSample, width: u32, height: u32) -> Result<Vec<u8>> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
    if let Ok(planar) = buffer.cast::<IMF2DBuffer2>() {
        let (mut first, mut pitch, mut start, mut length) =
            (std::ptr::null_mut(), 0i32, std::ptr::null_mut(), 0u32);
        unsafe {
            planar.Lock2DSize(
                MF2DBuffer_LockFlags_Read,
                &mut first,
                &mut pitch,
                &mut start,
                &mut length,
            )?;
        }
        let available = (length as usize).saturating_sub(first as usize - start as usize);
        let result = repack(first, pitch as isize, available, width, height);
        unsafe {
            let _ = planar.Unlock2D();
        }
        result
    } else {
        let (mut data, mut length) = (std::ptr::null_mut(), 0u32);
        unsafe { buffer.Lock(&mut data, None, Some(&mut length))? };
        let result = repack(data, width as isize, length as usize, width, height);
        unsafe {
            let _ = buffer.Unlock();
        }
        result
    }
}

/// Copy NV12 rows of `pitch` bytes into rows of `width` bytes. The chroma
/// rows follow the luma rows, which a decoder may pad to a multiple of 16
/// (1080 → 1088).
fn repack(base: *const u8, pitch: isize, available: usize, width: u32, height: u32) -> Result<Vec<u8>> {
    let (width, height) = (width as usize, height as usize);
    anyhow::ensure!(pitch > 0 && pitch as usize >= width, "{}", t().camera.read_failed);
    let pitch = pitch as usize;
    let padded = height.next_multiple_of(16);
    let luma_rows = if padded != height && available >= pitch * padded * 3 / 2 {
        padded
    } else {
        height
    };
    anyhow::ensure!(
        available >= pitch * (luma_rows + height / 2 - 1) + width,
        "{}",
        t().camera.read_failed
    );
    let mut out = vec![0u8; width * height * 3 / 2];
    // SAFETY: every row read lies within the locked `available` bytes.
    unsafe {
        for row in 0..height {
            std::ptr::copy_nonoverlapping(base.add(row * pitch), out.as_mut_ptr().add(row * width), width);
        }
        let chroma = base.add(luma_rows * pitch);
        for row in 0..height / 2 {
            std::ptr::copy_nonoverlapping(
                chroma.add(row * pitch),
                out.as_mut_ptr().add((height + row) * width),
                width,
            );
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::repack;

    #[test]
    fn padded_luma_rows_are_skipped() {
        // 4×2 picture in 6-byte rows, luma padded to 16 rows.
        let mut buffer = vec![0u8; 6 * 24];
        buffer[..4].copy_from_slice(&[1, 2, 3, 4]);
        buffer[6..10].copy_from_slice(&[5, 6, 7, 8]);
        buffer[96..100].copy_from_slice(&[9, 9, 9, 9]);
        let out = repack(buffer.as_ptr(), 6, buffer.len(), 4, 2).unwrap();
        assert_eq!(out, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 9, 9, 9]);
        // Unpadded, a little longer than needed.
        let mut buffer = vec![0u8; 6 * 3 + 5];
        buffer[12..16].copy_from_slice(&[7, 7, 7, 7]);
        let out = repack(buffer.as_ptr(), 6, buffer.len(), 4, 2).unwrap();
        assert_eq!(&out[8..], &[7, 7, 7, 7]);
    }
}
