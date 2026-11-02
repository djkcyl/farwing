//! The sound some programs play (`docs/protocol/chat-design.md` §9): all of
//! it but this program's (a screen share without the call in it), or one
//! program's only (a window share). Windows 10 build 20348 and later
//! capture that per process; older systems get everything the default
//! output plays. Delivered as 48 kHz stereo float, 10 ms at a time.

use anyhow::{Context, Result, bail};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{Interface, implement};

/// Samples per channel in one delivery (10 ms at 48 kHz).
pub(crate) const BLOCK: usize = 480;
const RATE: u32 = 48_000;

/// Whose sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// Everything but process `pid` and its children.
    AllBut(u32),
    /// Process `pid` and its children only.
    Only(u32),
}

/// Capturing per process works here (Windows 10 build 20348, Windows 11).
pub(crate) fn supported() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        super::device_profile::registry_string(
            windows::Win32::System::Registry::HKEY_LOCAL_MACHINE,
            r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
            "CurrentBuildNumber",
        )
        .ok()
        .and_then(|build| build.trim().parse::<u32>().ok())
        .is_some_and(|build| build >= 20348)
    })
}

/// A running capture; stops when dropped.
pub(crate) struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// The device center of this profile in this session (its process ID):
/// the process whose window carries the control center's mark.
pub(crate) fn control_center() -> Option<u32> {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetPropW, GetWindowThreadProcessId};
    use windows::core::BOOL;
    unsafe extern "system" fn each(hwnd: HWND, found: LPARAM) -> BOOL {
        // SAFETY: `found` is the slot passed below, alive for the call.
        let found = unsafe { &mut *(found.0 as *mut Option<u32>) };
        let mark: Vec<u16> = crate::product::instance_atom("ControlCenter.Window.v1")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: plain queries of a window's properties.
        unsafe {
            if !GetPropW(hwnd, windows::core::PCWSTR(mark.as_ptr())).is_invalid() {
                let mut pid = 0;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                if pid != 0 {
                    *found = Some(pid);
                    return BOOL(0);
                }
            }
        }
        BOOL(1)
    }
    let mut found = None;
    // SAFETY: the callback only reads window properties.
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM((&raw mut found) as isize));
    }
    found
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Capture {
    /// Still capturing (its thread has not stopped on a failure).
    pub(crate) fn alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Starts capturing `target` (where that is not supported, everything
    /// the default output plays); `samples` gets each 10 ms block,
    /// interleaved stereo.
    pub(crate) fn start(target: Target, samples: impl FnMut(&[f32; BLOCK * 2]) + Send + 'static) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready) = mpsc::channel();
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("share-sound".into())
            .spawn(move || {
                // SAFETY: COM for this thread only.
                unsafe {
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                let result = run(target, samples, &flag, &ready_tx);
                if let Err(e) = &result {
                    tracing::warn!(error = %format!("{e:#}"), ?target, "share sound capture stopped");
                    let _ = ready_tx.send(Err(format!("{e:#}")));
                }
                // SAFETY: as above.
                unsafe { CoUninitialize() };
            })?;
        match ready.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => bail!("{e}"),
            Err(_) => {
                stop.store(true, Ordering::Release);
                bail!("share sound capture did not start")
            }
        }
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(mpsc::Sender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _operation: windows::core::Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let _ = self.0.send(());
        Ok(())
    }
}

/// An audio client capturing process loopback for `target`.
fn process_client(target: Target) -> Result<IAudioClient> {
    let (pid, mode) = match target {
        Target::AllBut(pid) => (pid, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE),
        Target::Only(pid) => (pid, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE),
    };
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: mode,
            },
        },
    };
    let mut variant = PROPVARIANT::default();
    // SAFETY: a blob variant pointing at `params`, which outlives the call.
    unsafe {
        let inner = &mut *variant.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob.cbSize = size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32;
        inner.Anonymous.blob.pBlobData = (&raw const params).cast_mut().cast();
    }
    let (done_tx, done) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(done_tx).into();
    // SAFETY: the variant and handler live until completion is awaited.
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&variant),
            &handler,
        )?
    };
    done.recv_timeout(Duration::from_secs(5)).context("process loopback activation timed out")?;
    let mut result = windows::core::HRESULT(0);
    let mut unknown = None;
    // SAFETY: the operation completed.
    unsafe { operation.GetActivateResult(&mut result, &mut unknown)? };
    result.ok()?;
    let client: IAudioClient = unknown.context("no audio client")?.cast()?;
    // Keep the variant from being freed as a blob it does not own.
    std::mem::forget(variant);
    Ok(client)
}

/// The default output's loopback (everything it plays).
fn endpoint_client() -> Result<IAudioClient> {
    // SAFETY: plain COM calls.
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        Ok(device.Activate::<IAudioClient>(CLSCTX_ALL, None)?)
    }
}

fn run(
    target: Target,
    mut samples: impl FnMut(&[f32; BLOCK * 2]),
    stop: &AtomicBool,
    ready: &mpsc::Sender<std::result::Result<(), String>>,
) -> Result<()> {
    let client = if supported() { process_client(target)? } else { endpoint_client()? };
    let format = WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: 2,
        nSamplesPerSec: RATE,
        nAvgBytesPerSec: RATE * 8,
        nBlockAlign: 8,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    // SAFETY: COM calls on this thread; the event is closed below.
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            200_000,
            0,
            &format,
            None,
        )?;
        let event: HANDLE = CreateEventW(None, false, false, None)?;
        let result = (|| -> Result<()> {
            client.SetEventHandle(event)?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            let _ = ready.send(Ok(()));
            let mut pending: Vec<f32> = Vec::with_capacity(BLOCK * 4);
            while !stop.load(Ordering::Acquire) {
                // Process loopback signals only while something plays: the
                // wait's 100 ms are silence.
                if WaitForSingleObject(event, 100) != WAIT_OBJECT_0 {
                    pending.extend(std::iter::repeat_n(0.0, BLOCK * 2 * 10));
                } else {
                    loop {
                        let size = capture.GetNextPacketSize()?;
                        if size == 0 {
                            break;
                        }
                        let mut data = std::ptr::null_mut();
                        let (mut frames, mut flags) = (0u32, 0u32);
                        capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                        let count = frames as usize * 2;
                        if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                            pending.extend(std::iter::repeat_n(0.0, count));
                        } else {
                            pending.extend_from_slice(std::slice::from_raw_parts(data.cast::<f32>(), count));
                        }
                        capture.ReleaseBuffer(frames)?;
                    }
                }
                while pending.len() >= BLOCK * 2 {
                    let mut block = [0f32; BLOCK * 2];
                    block.copy_from_slice(&pending[..BLOCK * 2]);
                    pending.drain(..BLOCK * 2);
                    samples(&block);
                }
            }
            let _ = client.Stop();
            Ok(())
        })();
        let _ = CloseHandle(event);
        result
    }
}
