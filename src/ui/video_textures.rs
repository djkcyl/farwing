//! Pictures decoded off the window's thread (`features::voice::video`),
//! shown by the window: a decoder draws into a shared texture on the
//! window's graphics adapter and publishes its handle; the window opens
//! each handle once on its own device and draws it as an egui texture.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D11::{ID3D11ShaderResourceView, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::core::Interface;

/// The window's adapter (LUID), 0 before the window draws.
static ADAPTER: AtomicU64 = AtomicU64::new(0);

/// A handle not drawn for this long is let go.
const UNUSED: Duration = Duration::from_secs(3);

struct Opened {
    // Kept alive while egui draws the view.
    _texture: Option<ID3D11Texture2D>,
    id: Option<egui::TextureId>,
    used: Instant,
    /// Opening it failed (the decoder let it go meanwhile).
    failed: bool,
}

thread_local! {
    static OPENED: RefCell<HashMap<isize, Opened>> = RefCell::new(HashMap::new());
}

/// The adapter decoders draw on (the window's), once it has drawn.
pub(crate) fn adapter() -> Option<u64> {
    Some(ADAPTER.load(Ordering::Relaxed)).filter(|a| *a != 0)
}

/// The egui texture of shared texture `handle`, if it is open (it opens
/// before the next frame otherwise).
pub(crate) fn texture(handle: isize) -> Option<egui::TextureId> {
    OPENED.with(|opened| {
        let mut opened = opened.borrow_mut();
        let entry = opened.entry(handle).or_insert(Opened {
            _texture: None,
            id: None,
            used: Instant::now(),
            failed: false,
        });
        entry.used = Instant::now();
        entry.id
    })
}

/// Before each frame: opens the handles asked for, lets go of the unused.
pub(crate) fn sync(presenter: &mut crate::platform::graphics::UiPresenter) {
    if ADAPTER.load(Ordering::Relaxed) == 0
        && let Some(luid) = luid(presenter.device())
    {
        ADAPTER.store(luid, Ordering::Relaxed);
    }
    OPENED.with(|opened| {
        let mut opened = opened.borrow_mut();
        opened.retain(|_, entry| {
            let keep = entry.used.elapsed() < UNUSED;
            if !keep && let Some(id) = entry.id {
                presenter.unregister_texture(id);
            }
            keep
        });
        for (handle, entry) in opened.iter_mut().filter(|(_, e)| e.id.is_none() && !e.failed) {
            match open(presenter, *handle) {
                Ok((texture, view)) => {
                    entry.id = Some(presenter.register_texture(view));
                    entry._texture = Some(texture);
                }
                Err(e) => {
                    tracing::debug!(error = %e, "voice picture not opened");
                    entry.failed = true;
                }
            }
        }
    });
}

fn open(
    presenter: &crate::platform::graphics::UiPresenter,
    handle: isize,
) -> windows::core::Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
    // SAFETY: a shared handle published by a decoder on this adapter; the
    // view is created on the device that opened it.
    unsafe {
        let device = presenter.device();
        let mut texture: Option<ID3D11Texture2D> = None;
        device.OpenSharedResource(HANDLE(handle as *mut _), &mut texture)?;
        let texture = texture.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
        let mut view = None;
        device.CreateShaderResourceView(&texture, None, Some(&mut view))?;
        Ok((texture, view.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?))
    }
}

fn luid(device: &windows::Win32::Graphics::Direct3D11::ID3D11Device) -> Option<u64> {
    // SAFETY: plain queries of the device's adapter.
    unsafe {
        let dxgi: IDXGIDevice = device.cast().ok()?;
        let id = dxgi.GetAdapter().ok()?.GetDesc().ok()?.AdapterLuid;
        Some((u64::from(id.HighPart as u32) << 32) | u64::from(id.LowPart))
    }
}
