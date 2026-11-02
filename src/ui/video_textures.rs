//! Pictures decoded off the window's thread (`features::voice::video`),
//! shown by the windows: a decoder draws into a shared texture on the main
//! window's graphics adapter and publishes its handle; each window opens a
//! handle once on its own device and draws it as an egui texture.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D11::{ID3D11ShaderResourceView, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::core::Interface;

/// The main window's adapter (LUID), 0 before a window draws.
static ADAPTER: AtomicU64 = AtomicU64::new(0);

/// A handle not drawn for this long is let go.
const UNUSED: Duration = Duration::from_secs(3);
/// A window not drawn for this long is gone: its textures go too.
const GONE: Duration = Duration::from_secs(10);

struct Opened {
    // Kept alive while egui draws the view.
    _texture: Option<ID3D11Texture2D>,
    id: Option<egui::TextureId>,
    used: Instant,
    /// Opening it failed (the decoder let it go meanwhile).
    failed: bool,
}

/// One window's textures.
struct Window {
    opened: HashMap<isize, Opened>,
    synced: Instant,
}

thread_local! {
    /// By window (its device).
    static WINDOWS: RefCell<HashMap<usize, Window>> = RefCell::new(HashMap::new());
    /// The window drawing now.
    static CURRENT: Cell<usize> = const { Cell::new(0) };
}

/// The adapter decoders draw on (the main window's), once one has drawn.
pub(crate) fn adapter() -> Option<u64> {
    Some(ADAPTER.load(Ordering::Relaxed)).filter(|a| *a != 0)
}

/// The egui texture of shared texture `handle` in the window drawing now,
/// if it is open (it opens before that window's next frame otherwise).
pub(crate) fn texture(handle: isize) -> Option<egui::TextureId> {
    let current = CURRENT.with(Cell::get);
    WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        let window = windows.get_mut(&current)?;
        let entry = window.opened.entry(handle).or_insert(Opened {
            _texture: None,
            id: None,
            used: Instant::now(),
            failed: false,
        });
        entry.used = Instant::now();
        entry.id
    })
}

/// Before each of a window's frames: opens the handles it asked for, lets
/// go of the unused, and marks it as the window drawing now.
pub(crate) fn sync(presenter: &mut crate::platform::graphics::UiPresenter) {
    if ADAPTER.load(Ordering::Relaxed) == 0
        && let Some(luid) = luid(presenter.device())
    {
        ADAPTER.store(luid, Ordering::Relaxed);
    }
    let key = presenter.device().as_raw() as usize;
    CURRENT.with(|c| c.set(key));
    WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        windows.retain(|k, w| *k == key || w.synced.elapsed() < GONE);
        let window = windows.entry(key).or_insert_with(|| Window {
            opened: HashMap::new(),
            synced: Instant::now(),
        });
        window.synced = Instant::now();
        window.opened.retain(|_, entry| {
            let keep = entry.used.elapsed() < UNUSED;
            if !keep && let Some(id) = entry.id {
                presenter.unregister_texture(id);
            }
            keep
        });
        for (handle, entry) in window.opened.iter_mut().filter(|(_, e)| e.id.is_none() && !e.failed) {
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
