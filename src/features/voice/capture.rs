//! The microphone for voice (`docs/protocol/chat-design.md` §7.4).
//!
//! Windows' voice capture DSP takes the microphone and what the computer
//! plays (its reference) and removes the echo, with its noise suppression
//! and gain control; without echo cancellation, or when the DSP cannot run,
//! the microphone comes as it is. The "AI" noise level runs RNNoise
//! (nnnoiseless) on the result. Out come 10-ms mono blocks at 48 kHz.

use anyhow::{Context, Result, anyhow, ensure};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_queue::ArrayQueue;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) const RATE: u32 = 48_000;
/// 10 ms at 48 kHz.
pub(crate) const BLOCK: usize = 480;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Noise {
    Off,
    #[default]
    Standard,
    Ai,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CaptureConfig {
    /// An input endpoint's ID; `None`: the system's default.
    pub input: Option<String>,
    /// The output whose sound is the echo (the call's output).
    pub output: Option<String>,
    pub echo: bool,
    pub noise: Noise,
    pub gain: bool,
    /// A test tone instead of the microphone (Hz).
    pub tone: Option<f32>,
}

pub(crate) struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub blocks: Arc<ArrayQueue<[f32; BLOCK]>>,
    /// The last block's peak (f32 bits), for a meter.
    peak: Arc<AtomicU32>,
    error: Arc<Mutex<Option<String>>>,
}

/// Calls do not lower other programs' sound (§7.4): this process's audio
/// sessions on the voice devices opt out of Windows' ducking, before the
/// streams open.
pub(crate) fn keep_others_loud(input: Option<&str>, output: Option<&str>) {
    let (input, output) = (input.map(str::to_owned), output.map(str::to_owned));
    let worker = std::thread::spawn(move || {
        use windows::Win32::Media::Audio::{
            IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture,
            eCommunications, eConsole, eRender,
        };
        use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};
        use windows::core::{HSTRING, Interface};
        // SAFETY: COM on this thread only; every interface is released here.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            if let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) {
                let mut devices: Vec<IMMDevice> = Vec::new();
                for flow in [eRender, eCapture] {
                    for role in [eConsole, eCommunications] {
                        if let Ok(d) = enumerator.GetDefaultAudioEndpoint(flow, role) {
                            devices.push(d);
                        }
                    }
                }
                for id in [input, output].into_iter().flatten() {
                    if let Ok(d) = enumerator.GetDevice(&HSTRING::from(id.as_str())) {
                        devices.push(d);
                    }
                }
                for device in devices {
                    let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
                    let Ok(control) = manager.GetAudioSessionControl(None, 0) else { continue };
                    if let Ok(control) = control.cast::<IAudioSessionControl2>() {
                        let _ = control.SetDuckingPreference(true);
                    }
                }
            }
            CoUninitialize();
        }
    });
    let _ = worker.join();
}

/// The window in front belongs to an elevated process while this one is
/// not: keys pressed there do not reach push-to-talk (asked at most once
/// a second).
pub(crate) fn foreground_elevated() -> bool {
    static LAST: Mutex<Option<(std::time::Instant, bool)>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((at, answer)) = *last
        && at.elapsed() < Duration::from_secs(1)
    {
        return answer;
    }
    let answer = elevated_in_front();
    *last = Some((std::time::Instant::now(), answer));
    answer
}

fn elevated_in_front() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    fn elevated(process: HANDLE) -> Option<bool> {
        // SAFETY: plain queries on handles owned here.
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
            let mut e = TOKEN_ELEVATION::default();
            let mut length = 0;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                Some((&raw mut e).cast()),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut length,
            );
            let _ = CloseHandle(token);
            ok.ok()?;
            Some(e.TokenIsElevated != 0)
        }
    }
    // SAFETY: as above.
    unsafe {
        if elevated(GetCurrentProcess()).unwrap_or(false) {
            return false;
        }
        let window = GetForegroundWindow();
        let mut pid = 0;
        GetWindowThreadProcessId(window, Some(&mut pid));
        if pid == 0 || pid == std::process::id() {
            return false;
        }
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        // An elevated process's token is not open to this one: elevated too.
        let answer = elevated(process).unwrap_or(true);
        let _ = CloseHandle(process);
        answer
    }
}

/// An audio endpoint: its ID and name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub id: String,
    pub name: String,
}

/// The active inputs (`input`) or outputs.
pub(crate) fn endpoints(input: bool) -> Vec<Endpoint> {
    let host = cpal::default_host();
    let devices = if input { host.input_devices() } else { host.output_devices() };
    devices
        .map(|list| {
            list.filter_map(|d| {
                Some(Endpoint {
                    id: d.id().ok()?.id().to_owned(),
                    name: d.description().ok()?.name().to_owned(),
                })
            })
            .collect()
        })
        .unwrap_or_default()
}

impl Capture {
    pub(crate) fn start(config: CaptureConfig) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let blocks = Arc::new(ArrayQueue::new(50));
        let peak = Arc::new(AtomicU32::new(0));
        let error = Arc::new(Mutex::new(None));
        let thread = {
            let (stop, blocks, peak, error) = (stop.clone(), blocks.clone(), peak.clone(), error.clone());
            std::thread::Builder::new()
                .name("voice-capture".into())
                .spawn(move || {
                    let mut out = Output::new(config.noise == Noise::Ai, blocks, peak);
                    let result = if let Some(hz) = config.tone {
                        tone(hz, &stop, &mut out)
                    } else if config.echo || config.noise == Noise::Standard || config.gain {
                        dsp::run(&config, &stop, &mut out).or_else(|e| {
                            tracing::warn!(error = %format!("{e:#}"), "voice processing unavailable; the microphone as it is");
                            plain(&config, &stop, &mut out)
                        })
                    } else {
                        plain(&config, &stop, &mut out)
                    };
                    if let Err(e) = result {
                        tracing::warn!(error = %format!("{e:#}"), "voice capture failed");
                        *error.lock().unwrap_or_else(|p| p.into_inner()) = Some(format!("{e:#}"));
                    }
                })
                .ok()
        };
        Self {
            stop,
            thread,
            blocks,
            peak,
            error,
        }
    }

    pub(crate) fn peak(&self) -> f32 {
        f32::from_bits(self.peak.load(Ordering::Relaxed))
    }

    pub(crate) fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Where captured samples go: resampled to 48 kHz, cut into blocks,
/// denoised when asked.
struct Output {
    resampler: Option<(u32, crate::media::audio::dsp::Resampler)>,
    pending: Vec<f32>,
    denoise: Option<Box<nnnoiseless::DenoiseState<'static>>>,
    blocks: Arc<ArrayQueue<[f32; BLOCK]>>,
    peak: Arc<AtomicU32>,
    stereo_in: Vec<f32>,
    stereo_out: Vec<f32>,
}

impl Output {
    fn new(denoise: bool, blocks: Arc<ArrayQueue<[f32; BLOCK]>>, peak: Arc<AtomicU32>) -> Self {
        Self {
            resampler: None,
            pending: Vec::with_capacity(BLOCK * 4),
            denoise: denoise.then(nnnoiseless::DenoiseState::new),
            blocks,
            peak,
            stereo_in: Vec::new(),
            stereo_out: vec![0.0; 8192],
        }
    }

    /// Mono samples at `rate`.
    fn push(&mut self, samples: &[f32], rate: u32) -> Result<()> {
        if rate == RATE {
            self.pending.extend_from_slice(samples);
        } else {
            if self.resampler.as_ref().is_none_or(|(r, _)| *r != rate) {
                self.resampler = Some((rate, crate::media::audio::dsp::Resampler::with_rates(rate, RATE)?));
            }
            let (_, resampler) = self.resampler.as_mut().expect("set above");
            // The resampler takes interleaved stereo.
            self.stereo_in.clear();
            self.stereo_in.extend(samples.iter().flat_map(|s| [*s, *s]));
            let mut offset = 0;
            while offset < self.stereo_in.len() {
                let (used, made) = resampler.process(&self.stereo_in[offset..], &mut self.stereo_out)?;
                self.pending.extend(self.stereo_out[..made].chunks_exact(2).map(|p| p[0]));
                if used == 0 && made == 0 {
                    break;
                }
                offset += used;
            }
        }
        while self.pending.len() >= BLOCK {
            let mut block = [0f32; BLOCK];
            block.copy_from_slice(&self.pending[..BLOCK]);
            self.pending.drain(..BLOCK);
            if let Some(state) = self.denoise.as_mut() {
                // RNNoise works on 16-bit sample values.
                let input: Vec<f32> = block.iter().map(|s| s * 32768.0).collect();
                let mut output = [0f32; BLOCK];
                state.process_frame(&mut output, &input);
                for (b, o) in block.iter_mut().zip(output) {
                    *b = (o / 32768.0).clamp(-1.0, 1.0);
                }
            }
            let peak = block.iter().fold(0f32, |m, s| m.max(s.abs()));
            self.peak.store(peak.to_bits(), Ordering::Relaxed);
            self.blocks.force_push(block);
        }
        Ok(())
    }
}

fn tone(hz: f32, stop: &AtomicBool, out: &mut Output) -> Result<()> {
    let start = Instant::now();
    let mut sent: u64 = 0;
    let mut phase = 0f32;
    let step = hz * std::f32::consts::TAU / RATE as f32;
    while !stop.load(Ordering::Acquire) {
        let due = (start.elapsed().as_millis() / 10) as u64;
        while sent < due {
            let block: Vec<f32> = (0..BLOCK)
                .map(|_| {
                    phase = (phase + step) % std::f32::consts::TAU;
                    phase.sin() * 0.25
                })
                .collect();
            out.push(&block, RATE)?;
            sent += 1;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

/// The microphone without processing (cpal, shared mode).
fn plain(config: &CaptureConfig, stop: &AtomicBool, out: &mut Output) -> Result<()> {
    let host = cpal::default_host();
    let device = match &config.input {
        Some(id) => host
            .input_devices()?
            .find(|d| d.id().ok().is_some_and(|i| i.id() == id))
            .or_else(|| host.default_input_device()),
        None => host.default_input_device(),
    }
    .ok_or_else(|| anyhow!("{}", crate::i18n::t().voice.no_microphone))?;
    let supported = device.default_input_config().context("microphone format")?;
    let config = supported.config();
    let channels = config.channels as usize;
    let rate = config.sample_rate;
    ensure!(channels > 0, "microphone without channels");
    let queue: Arc<ArrayQueue<Vec<f32>>> = Arc::new(ArrayQueue::new(64));
    let failed = Arc::new(AtomicBool::new(false));
    let stream = {
        let (queue, failed) = (queue.clone(), failed.clone());
        let on_error = move |e: cpal::Error| {
            if e.kind() != cpal::ErrorKind::Xrun {
                failed.store(true, Ordering::Release);
            }
        };
        macro_rules! build {
            ($t:ty) => {
                device.build_input_stream(
                    config.clone(),
                    move |data: &[$t], _| {
                        let mono: Vec<f32> = data
                            .chunks_exact(channels)
                            .map(|f| {
                                f.iter().map(|s| <f32 as cpal::FromSample<$t>>::from_sample_(*s)).sum::<f32>()
                                    / channels as f32
                            })
                            .collect();
                        queue.force_push(mono);
                    },
                    on_error,
                    None,
                )
            };
        }
        match supported.sample_format() {
            cpal::SampleFormat::F32 => build!(f32),
            cpal::SampleFormat::I16 => build!(i16),
            cpal::SampleFormat::I32 => build!(i32),
            cpal::SampleFormat::F64 => build!(f64),
            other => return Err(anyhow!("microphone format {other}")),
        }?
    };
    stream.play()?;
    tracing::info!(rate, channels, "voice microphone started (unprocessed)");
    while !stop.load(Ordering::Acquire) {
        ensure!(!failed.load(Ordering::Acquire), "{}", crate::i18n::t().voice.microphone_lost);
        while let Some(samples) = queue.pop() {
            out.push(&samples, rate)?;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

/// Windows' voice capture DSP in source mode: it opens the microphone and
/// the output's loopback itself and gives processed audio.
mod dsp {
    use super::{CaptureConfig, Noise, Output};
    use anyhow::{Context, Result, ensure};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use windows::Win32::Media::Audio::{
        DEVICE_STATE_ACTIVE, EDataFlow, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eRender,
        WAVEFORMATEX, WAVE_FORMAT_PCM,
    };
    use windows::Win32::Media::DxMediaObjects::{
        DMO_MEDIA_TYPE, DMO_OUTPUT_DATA_BUFFER, IMediaBuffer, IMediaBuffer_Impl, IMediaObject, MoFreeMediaType,
        MoInitMediaType,
    };
    use windows::Win32::Media::MediaFoundation::{
        CWMAudioAEC, FORMAT_WaveFormatEx, MEDIASUBTYPE_PCM, MEDIATYPE_Audio, SINGLE_CHANNEL_AEC,
        SINGLE_CHANNEL_NSAGC,
    };
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{CLSCTX_ALL, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    use windows::core::{GUID, Interface, implement};

    /// The DSP's properties (wmcodecdsp.h).
    const KEYS: GUID = GUID::from_u128(0x6f52c567_0360_4bd2_9617_ccbf1421c939);
    const SYSTEM_MODE: u32 = 2;
    const DMO_SOURCE_MODE: u32 = 3;
    const DEVICE_INDEXES: u32 = 4;
    const FEATURE_MODE: u32 = 5;
    const FEATR_NS: u32 = 8;
    const FEATR_AGC: u32 = 9;
    const S_FALSE_INCOMPLETE: u32 = 0x0100_0000;

    fn key(pid: u32) -> PROPERTYKEY {
        PROPERTYKEY { fmtid: KEYS, pid }
    }

    #[implement(IMediaBuffer)]
    struct Buffer {
        data: std::cell::UnsafeCell<Vec<u8>>,
        length: std::cell::Cell<u32>,
    }

    impl IMediaBuffer_Impl for Buffer_Impl {
        fn SetLength(&self, length: u32) -> windows::core::Result<()> {
            // SAFETY: only the DSP's thread (this one) touches the buffer.
            let max = unsafe { (*self.data.get()).len() } as u32;
            if length > max {
                return Err(windows::Win32::Foundation::E_INVALIDARG.into());
            }
            self.length.set(length);
            Ok(())
        }
        fn GetMaxLength(&self) -> windows::core::Result<u32> {
            // SAFETY: as above.
            Ok(unsafe { (*self.data.get()).len() } as u32)
        }
        fn GetBufferAndLength(&self, buffer: *mut *mut u8, length: *mut u32) -> windows::core::Result<()> {
            // SAFETY: the pointers are the DSP's out parameters; the data
            // lives as long as the buffer object.
            unsafe {
                if !buffer.is_null() {
                    *buffer = (*self.data.get()).as_mut_ptr();
                }
                if !length.is_null() {
                    *length = self.length.get();
                }
            }
            Ok(())
        }
    }

    /// The index of endpoint `id` among the active ones of `flow`; -1: default.
    fn index(enumerator: &IMMDeviceEnumerator, flow: EDataFlow, id: Option<&str>) -> Result<i32> {
        let Some(id) = id else { return Ok(-1) };
        // SAFETY: plain COM calls on interfaces we hold.
        unsafe {
            let all = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
            for i in 0..all.GetCount()? {
                let device = all.Item(i)?;
                let name = device.GetId()?;
                let text = name.to_string().unwrap_or_default();
                windows::Win32::System::Com::CoTaskMemFree(Some(name.0 as *const _));
                if text == id {
                    return Ok(i as i32);
                }
            }
        }
        Ok(-1)
    }

    pub(super) fn run(config: &CaptureConfig, stop: &AtomicBool, out: &mut Output) -> Result<()> {
        // SAFETY: COM on this thread for the DSP's lifetime; every interface
        // comes from the system and is released by its wrapper.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let result = (|| -> Result<()> {
                let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                let capture = index(&enumerator, eCapture, config.input.as_deref())?;
                let render = index(&enumerator, eRender, config.output.as_deref())?;
                let dmo: IMediaObject =
                    CoCreateInstance(&CWMAudioAEC, None, CLSCTX_INPROC_SERVER).context("voice capture DSP")?;
                let props: IPropertyStore = dmo.cast()?;
                let mode = if config.echo { SINGLE_CHANNEL_AEC } else { SINGLE_CHANNEL_NSAGC };
                props.SetValue(&key(SYSTEM_MODE), &PROPVARIANT::from(mode.0))?;
                props.SetValue(&key(DMO_SOURCE_MODE), &PROPVARIANT::from(true))?;
                // Low word: the microphone; high word: the output (the
                // defaults unless one is chosen).
                if capture >= 0 || render >= 0 {
                    let capture = capture.max(0);
                    let render = render.max(0);
                    props.SetValue(&key(DEVICE_INDEXES), &PROPVARIANT::from((render << 16) | capture))?;
                }
                props.SetValue(&key(FEATURE_MODE), &PROPVARIANT::from(true))?;
                props.SetValue(&key(FEATR_NS), &PROPVARIANT::from(i32::from(config.noise == Noise::Standard)))?;
                props.SetValue(&key(FEATR_AGC), &PROPVARIANT::from(config.gain))?;
                // 16-bit mono; the highest rate it takes.
                let mut rate = 0;
                for candidate in [22_050u32, 16_000] {
                    let mut mt = DMO_MEDIA_TYPE::default();
                    MoInitMediaType(&mut mt, std::mem::size_of::<WAVEFORMATEX>() as u32)?;
                    mt.majortype = MEDIATYPE_Audio;
                    mt.subtype = MEDIASUBTYPE_PCM;
                    mt.lSampleSize = 0;
                    mt.bFixedSizeSamples = true.into();
                    mt.bTemporalCompression = false.into();
                    mt.formattype = FORMAT_WaveFormatEx;
                    let format = mt.pbFormat as *mut WAVEFORMATEX;
                    *format = WAVEFORMATEX {
                        wFormatTag: WAVE_FORMAT_PCM as u16,
                        nChannels: 1,
                        nSamplesPerSec: candidate,
                        nAvgBytesPerSec: candidate * 2,
                        nBlockAlign: 2,
                        wBitsPerSample: 16,
                        cbSize: 0,
                    };
                    let set = dmo.SetOutputType(0, Some(&mt), 0);
                    MoFreeMediaType(&mut mt)?;
                    if set.is_ok() {
                        rate = candidate;
                        break;
                    }
                }
                ensure!(rate > 0, "the voice capture DSP took no output format");
                dmo.AllocateStreamingResources()?;
                tracing::info!(rate, echo = config.echo, noise = ?config.noise, "voice processing started");
                let buffer: IMediaBuffer = Buffer {
                    data: std::cell::UnsafeCell::new(vec![0u8; rate as usize * 2]),
                    length: std::cell::Cell::new(0),
                }
                .into();
                let mut samples = Vec::with_capacity(rate as usize / 10);
                while !stop.load(Ordering::Acquire) {
                    loop {
                        buffer.SetLength(0)?;
                        let mut data = [DMO_OUTPUT_DATA_BUFFER {
                            pBuffer: std::mem::ManuallyDrop::new(Some(buffer.clone())),
                            ..Default::default()
                        }];
                        let mut status = 0u32;
                        let result = dmo.ProcessOutput(0, &mut data, &mut status);
                        let incomplete = data[0].dwStatus & S_FALSE_INCOMPLETE != 0;
                        std::mem::ManuallyDrop::drop(&mut data[0].pBuffer);
                        result?;
                        let mut ptr = std::ptr::null_mut();
                        let mut len = 0u32;
                        buffer.GetBufferAndLength(Some(&mut ptr), Some(&mut len))?;
                        if len > 0 {
                            let bytes = std::slice::from_raw_parts(ptr, len as usize);
                            samples.clear();
                            samples.extend(
                                bytes
                                    .chunks_exact(2)
                                    .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32768.0),
                            );
                            out.push(&samples, rate)?;
                        }
                        if !incomplete {
                            break;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let _ = dmo.FreeStreamingResources();
                Ok(())
            })();
            CoUninitialize();
            result
        }
    }
}
