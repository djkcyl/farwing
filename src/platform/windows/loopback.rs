//! WASAPI render-endpoint loopback with explicit silence/discontinuity handling.
use crate::media::audio::{
    dsp::Resampler,
    sender::{BLOCK, RATE},
};
use anyhow::{Context, Result, ensure};
use crate::i18n::t;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Media::Audio::*,
        System::{Com::*, Performance::*},
    },
    core::GUID,
};
pub(crate) type Samples = Arc<dyn Fn([f32; BLOCK * 2], Instant) + Send + Sync>;
mod devices;
mod shared;
mod stream;
pub(crate) use devices::{Devices, Endpoint};
pub(crate) use shared::{Subscription, subscribe};
pub(crate) use stream::{Capture, open};
struct Apartment(bool, std::marker::PhantomData<std::rc::Rc<()>>);
impl Apartment {
    fn new() -> Result<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.0 == 0x80010106u32 as i32 {
            return Ok(Self(false, Default::default()));
        }
        hr.ok()?;
        Ok(Self(true, Default::default()))
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                CoUninitialize();
            }
        }
    }
}
struct Mix(*mut WAVEFORMATEX);
impl Drop for Mix {
    fn drop(&mut self) {
        unsafe {
            CoTaskMemFree(Some(self.0.cast()));
        }
    }
}

struct NativeCapture {
    input_watch: devices::SessionWatch,
    render_watch: devices::SessionWatch,
    clock: IAudioClock,
    progress: Progress,
    clock_check: Instant,
    capture: IAudioCaptureClient,
    input: IAudioClient,
    render: IAudioRenderClient,
    silence: IAudioClient,
    render_frames: u32,
    bytes_per_sample: usize,
    float: bool,
    channels: usize,
    rate: u32,
    convert: Convert,
    samples: Samples,
    decoded: Vec<f32>,
    _apartment: Apartment,
}
fn open_native(endpoint: &Endpoint, samples: Samples) -> Result<NativeCapture> {
    let apartment = Apartment::new()?;
    unsafe {
        let endpoint = &endpoint.device;
        let input: IAudioClient = endpoint.Activate(CLSCTX_ALL, None)?;
        let mix = Mix(input.GetMixFormat()?);
        ensure!(!mix.0.is_null(), "{}", t().system.loopback_format_missing);
        let format = *mix.0;
        let channels = usize::from(format.nChannels);
        let rate = format.nSamplesPerSec;
        ensure!(
            (1..=8).contains(&channels) && (8000..=192000).contains(&rate),
            "{}",
            t().system.loopback_format_unsupported
        );
        let mut mask = 0;
        let float = if format.wFormatTag == 0xfffe {
            ensure!(format.cbSize >= 22, "{}", t().system.loopback_extension_incomplete);
            let ext = *(mix.0.cast::<WAVEFORMATEXTENSIBLE>());
            mask = ext.dwChannelMask;
            let subformat = ext.SubFormat;
            ensure!(
                subformat == GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71)
                    || subformat == GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71),
                "{}",
                t().system.loopback_subformat_unsupported
            );
            subformat.data1 == 3
        } else {
            ensure!(
                matches!(format.wFormatTag, 1 | 3),
                "{}",
                t().system.loopback_encoding_unsupported
            );
            format.wFormatTag == 3
        };
        let bytes = usize::from(format.wBitsPerSample) / 8;
        ensure!(
            if float {
                matches!(bytes, 4 | 8)
            } else {
                matches!(bytes, 2 | 3 | 4)
            },
            "{}",
            t().system.loopback_bit_depth_unsupported
        );
        ensure!(
            usize::from(format.nBlockAlign) == channels * bytes,
            "{}",
            t().system.loopback_frame_size_invalid
        );
        input
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                100_000,
                0,
                mix.0,
                None,
            )
            .context(t().system.loopback_init_failed)?;
        let capture: IAudioCaptureClient = input.GetService()?;
        // Keep the engine running through silence, without changing volume or defaults.
        let silence: IAudioClient = endpoint.Activate(CLSCTX_ALL, None)?;
        silence.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 100_000, 0, mix.0, None)?;
        let render: IAudioRenderClient = silence.GetService()?;
        let render_frames = silence.GetBufferSize()?;
        render.GetBuffer(render_frames)?;
        render.ReleaseBuffer(render_frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
        let input_watch = devices::SessionWatch::new(&input)?;
        let render_watch = devices::SessionWatch::new(&silence)?;
        let clock = silence.GetService::<IAudioClock>()?;
        let now = Instant::now();
        let result = NativeCapture {
            input_watch,
            render_watch,
            clock,
            progress: Progress::new(now),
            clock_check: now,
            capture,
            input,
            render,
            silence,
            render_frames,
            bytes_per_sample: bytes,
            float,
            channels,
            rate: format.nSamplesPerSec,
            convert: Convert::new(format.nSamplesPerSec, channels, mask)?,
            samples,
            decoded: Vec::new(),
            _apartment: apartment,
        };
        result.input.Start()?;
        result.silence.Start()?;
        Ok(result)
    }
}
impl NativeCapture {
    pub fn producing(&self) -> bool {
        self.progress.delivered
    }
    pub fn poll(&mut self) -> Result<()> {
        self.input_watch.check()?;
        self.render_watch.check()?;
        unsafe {
            let available = self
                .render_frames
                .saturating_sub(self.silence.GetCurrentPadding()?);
            if available > 0 {
                self.render.GetBuffer(available)?;
                self.render
                    .ReleaseBuffer(available, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
            }
            // Bound a single poll, so a malfunctioning device cannot delay revocation.
            for _ in 0..32 {
                if self.capture.GetNextPacketSize()? == 0 {
                    break;
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0;
                let mut flags = 0;
                let mut position = 0;
                let mut qpc = 0;
                self.capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    Some(&mut position),
                    Some(&mut qpc),
                )?;
                let result = (|| -> Result<()> {
                    ensure!(
                        frames <= self.rate,
                        "{}",
                        t().system.loopback_buffer_length_invalid
                    );
                    let count = frames as usize * self.channels;
                    self.decoded.resize(count, 0.);
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                        self.decoded.fill(0.);
                    } else {
                        ensure!(
                            !data.is_null() || count == 0,
                            "{}",
                            t().system.loopback_buffer_empty
                        );
                        if count > 0 {
                            let bytes =
                                std::slice::from_raw_parts(data, count * self.bytes_per_sample);
                            for (dst, b) in self
                                .decoded
                                .iter_mut()
                                .zip(bytes.chunks_exact(self.bytes_per_sample))
                            {
                                let value = match (self.float, self.bytes_per_sample) {
                                    (true, 4) => f32::from_le_bytes(b.try_into().unwrap()),
                                    (true, 8) => f64::from_le_bytes(b.try_into().unwrap()) as f32,
                                    (false, 2) => {
                                        f32::from(i16::from_le_bytes(b.try_into().unwrap()))
                                            / 32768.
                                    }
                                    (false, 3) => {
                                        (i32::from_le_bytes([0, b[0], b[1], b[2]]) as f32)
                                            / 2147483648.
                                    }
                                    (false, 4) => {
                                        (i32::from_le_bytes(b.try_into().unwrap()) as f32)
                                            / 2147483648.
                                    }
                                    _ => unreachable!(),
                                };
                                *dst = if value.is_finite() {
                                    value.clamp(-1., 1.)
                                } else {
                                    0.
                                };
                            }
                        }
                    }
                    let now = Instant::now();
                    let at = if flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0 && qpc > 0
                    {
                        let mut ticks = 0;
                        let mut frequency = 0;
                        QueryPerformanceCounter(&mut ticks)?;
                        QueryPerformanceFrequency(&mut frequency)?;
                        let stamp = (u128::from(ticks as u64) * 10_000_000
                            / u128::from(frequency as u64))
                            as u64;
                        now.checked_sub(Duration::from_nanos(
                            stamp.saturating_sub(qpc).saturating_mul(100),
                        ))
                        .unwrap_or(now)
                    } else {
                        now.checked_sub(Duration::from_secs_f64(
                            f64::from(frames) / f64::from(self.rate),
                        ))
                        .unwrap_or(now)
                    };
                    if position != 0 && flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0
                    {
                        self.convert.reset()?;
                    }
                    self.convert.push(&self.decoded, at, &self.samples)
                })();
                self.capture.ReleaseBuffer(frames)?;
                result?;
                if frames > 0 {
                    self.progress.packet(
                        Instant::now(),
                        (flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0)
                            .then_some(position.saturating_add(u64::from(frames))),
                    );
                }
            }
            let now = Instant::now();
            if now >= self.clock_check {
                let mut position = 0;
                self.clock.GetPosition(&mut position, None)?;
                self.progress.clock(now, position);
                self.clock_check = now + Duration::from_millis(100);
            }
            self.progress.check(now)?;
        }
        Ok(())
    }
}
impl Drop for NativeCapture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.input.Stop();
            let _ = self.silence.Stop();
        }
    }
}

struct Convert {
    rate: u32,
    channels: usize,
    weights: Vec<[f32; 2]>,
    resampler: Resampler,
    frame: [f32; BLOCK * 2],
    filled: usize,
    next: Option<Instant>,
    expected: Option<Instant>,
}
impl Convert {
    fn new(rate: u32, channels: usize, mask: u32) -> Result<Self> {
        Ok(Self {
            rate,
            channels,
            weights: weights(channels, mask),
            resampler: Resampler::with_rates(rate, RATE)?,
            frame: [0.; BLOCK * 2],
            filled: 0,
            next: None,
            expected: None,
        })
    }
    fn reset(&mut self) -> Result<()> {
        self.next = None;
        self.expected = None;
        self.filled = 0;
        self.resampler = Resampler::with_rates(self.rate, RATE)?;
        Ok(())
    }
    fn push<T: cpal::Sample>(&mut self, data: &[T], at: Instant, output: &Samples) -> Result<()>
    where
        f32: cpal::FromSample<T>,
    {
        let gap = self.expected.is_some_and(|expected| {
            at.saturating_duration_since(expected)
                .max(expected.saturating_duration_since(at))
                > Duration::from_millis(50)
        });
        if self.next.is_none() || gap {
            self.next = Some(at);
            self.filled = 0;
            if gap {
                self.resampler = Resampler::with_rates(self.rate, RATE)?;
            }
        }
        // Anchor each buffer to WASAPI's capture clock. Sample counts alone drift
        // from wall time on real oscillators; keep partial-frame age across buffers.
        self.next = Some(
            at.checked_sub(Duration::from_secs_f64(
                self.filled as f64 / (2. * f64::from(RATE)),
            ))
            .unwrap_or(at),
        );
        self.expected = Some(
            at + Duration::from_secs_f64(
                (data.len() / self.channels) as f64 / f64::from(self.rate),
            ),
        );
        let mut stereo = [0f32; 320];
        let mut normalized = [0f32; 1920];
        for chunk in data.chunks(self.channels * 160) {
            let frames = chunk.len() / self.channels;
            for (i, src) in chunk.chunks_exact(self.channels).enumerate() {
                let mut values = [0f32; 8];
                for (to, from) in values.iter_mut().zip(src) {
                    let v = <f32 as cpal::FromSample<T>>::from_sample_(*from);
                    *to = if v.is_finite() { v.clamp(-1., 1.) } else { 0. };
                }
                let (l, r) = values
                    .iter()
                    .zip(&self.weights)
                    .fold((0., 0.), |(l, r), (v, w)| (l + v * w[0], r + v * w[1]));
                stereo[i * 2] = l;
                stereo[i * 2 + 1] = r;
            }
            let mut consumed = 0;
            while consumed < frames {
                let (used, produced) = if self.rate == RATE {
                    normalized[..frames * 2].copy_from_slice(&stereo[..frames * 2]);
                    (frames, frames)
                } else {
                    let (u, p) = self
                        .resampler
                        .process(&stereo[consumed * 2..frames * 2], &mut normalized)?;
                    (u / 2, p / 2)
                };
                ensure!(
                    used != 0 || produced != 0,
                    "{}",
                    t().system.loopback_resampler_stalled
                );
                consumed += used;
                for sample in &normalized[..produced * 2] {
                    self.frame[self.filled] = *sample;
                    self.filled += 1;
                    if self.filled == self.frame.len() {
                        let timestamp = self.next.unwrap();
                        output(self.frame, timestamp);
                        self.next = Some(timestamp + Duration::from_millis(10));
                        self.filled = 0;
                    }
                }
            }
        }
        Ok(())
    }
}

// WAVEFORMATEXTENSIBLE speaker-mask order, with separate left/right normalization.
fn weights(channels: usize, mask: u32) -> Vec<[f32; 2]> {
    if channels == 1 {
        return vec![[1., 1.]];
    }
    let mask = if mask.count_ones() as usize == channels {
        mask
    } else {
        match channels {
            2 => 3,
            3 => 7,
            4 => 0x33,
            5 => 0x37,
            6 => 0x3f,
            7 => 0x70f,
            8 => 0x63f,
            _ => 0,
        }
    };
    let mut weights: Vec<_> = (0..32)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| match bit {
            0 => [1., 0.],
            1 => [0., 1.],
            2 => [0.707, 0.707],
            3 => [0., 0.],
            4 | 6 | 9 => [0.707, 0.],
            5 | 7 | 10 => [0., 0.707],
            _ => [0.5, 0.5],
        })
        .collect();
    for side in 0..2 {
        let total = weights.iter().map(|w| w[side]).sum::<f32>().max(1.);
        for w in &mut weights {
            w[side] /= total;
        }
    }
    weights
}

// A silent keepalive stream is active, so healthy silence still advances both
// the device clock and capture-buffer positions. A stalled API returning Ok(0)
// cannot leave the UI claiming that capture is healthy forever.
struct Progress {
    delivered: bool,
    packet_position: Option<u64>,
    clock_position: Option<u64>,
    packet_at: Instant,
    clock_at: Instant,
}
impl Progress {
    fn new(now: Instant) -> Self {
        Self {
            delivered: false,
            packet_position: None,
            clock_position: None,
            packet_at: now,
            clock_at: now,
        }
    }
    fn packet(&mut self, now: Instant, position: Option<u64>) {
        self.delivered = true;
        // TIMESTAMP_ERROR invalidates both supplied positions, not the PCM.
        if position.is_none() || self.packet_position != position {
            self.packet_position = position;
            self.packet_at = now;
        }
    }
    fn clock(&mut self, now: Instant, position: u64) {
        if self.clock_position != Some(position) {
            self.clock_position = Some(position);
            self.clock_at = now;
        }
    }
    fn check(&self, now: Instant) -> Result<()> {
        ensure!(
            now.saturating_duration_since(self.clock_at) < Duration::from_secs(1),
            "{}",
            t().system.loopback_clock_stopped
        );
        ensure!(
            now.saturating_duration_since(self.packet_at) < Duration::from_secs(1),
            "{}",
            t().system.loopback_delivery_stopped
        );
        Ok(())
    }
}
