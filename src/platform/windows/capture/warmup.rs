//! GPU driver loading while a session connects.
//!
//! The user-mode graphics driver unloads with the last D3D device of the
//! process, so each session would load it again (100-200 ms) when its
//! capture opens, and only then create the encoder device (and, between two
//! GPUs, the D3D12 runtime of the handoff), one after the other. A session
//! starts this as soon as its screen is known: the capture opens on a loaded
//! driver, the encoder takes the device made here, and the D3D12 device of a
//! handoff already exists. What is not taken is released after [`LIFETIME`].
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};
use windows::{
    Win32::Graphics::{
        Direct3D::D3D_FEATURE_LEVEL_11_0,
        Direct3D11::ID3D11Device,
        Direct3D12::{D3D12CreateDevice, ID3D12Device},
        Dxgi::IDXGIDevice,
    },
    core::Interface,
};

const LIFETIME: Duration = Duration::from_secs(10);
/// A taker waits this long at most for a device still being created.
const WAIT: Duration = Duration::from_secs(2);

struct Spare {
    adapter: u64,
    id: u64,
    /// The device is still being created.
    creating: bool,
    device: Option<ID3D11Device>,
    _d3d12: Option<ID3D12Device>,
}

static SPARES: Mutex<Vec<Spare>> = Mutex::new(Vec::new());
static READY: Condvar = Condvar::new();
static NEXT: AtomicU64 = AtomicU64::new(0);

fn update(id: u64, change: impl FnOnce(&mut Spare)) {
    if let Some(spare) = super::super::lock(&SPARES).iter_mut().find(|s| s.id == id) {
        change(spare);
    }
    READY.notify_all();
}

/// Load the driver of these adapters (the screen's and its render GPU's) and
/// keep an encoder device ready on each.
pub(crate) fn warm_up(adapters: &[u64]) {
    let mut started = Vec::new();
    for &adapter in adapters {
        if started.contains(&adapter) {
            continue;
        }
        started.push(adapter);
        let id = {
            let mut spares = super::super::lock(&SPARES);
            if spares
                .iter()
                .any(|s| s.adapter == adapter && (s.creating || s.device.is_some()))
            {
                continue;
            }
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            spares.push(Spare {
                adapter,
                id,
                creating: true,
                device: None,
                _d3d12: None,
            });
            id
        };
        // Two GPUs hand frames over between adapters through D3D12.
        let handoff = adapters.iter().any(|&other| other != adapter);
        let spawned = std::thread::Builder::new()
            .name("GPU warm-up".to_owned())
            .spawn(move || prepare(id, adapter, handoff));
        if let Err(error) = spawned {
            tracing::debug!(%error, "GPU warm-up thread unavailable");
            super::super::lock(&SPARES).retain(|s| s.id != id);
            READY.notify_all();
        }
    }
}

fn prepare(id: u64, adapter: u64, handoff: bool) {
    let begun = Instant::now();
    let device = match super::new_device(adapter) {
        Ok((device, _)) => device,
        Err(error) => {
            tracing::debug!(%error, adapter, "GPU warm-up failed");
            super::super::lock(&SPARES).retain(|s| s.id != id);
            READY.notify_all();
            return;
        }
    };
    let device_ms = begun.elapsed().as_millis() as u64;
    update(id, |spare| {
        spare.device = Some(device.clone());
        spare.creating = false;
    });
    // Only the adapter: the device's context belongs to its taker now.
    let d3d12 = handoff
        .then(|| d3d12(&device))
        .transpose()
        .inspect_err(|error| tracing::debug!(%error, "D3D12 warm-up unavailable"))
        .ok()
        .flatten();
    drop(device);
    tracing::debug!(
        adapter,
        device_ms,
        d3d12 = d3d12.is_some(),
        elapsed_ms = begun.elapsed().as_millis() as u64,
        "GPU driver warmed up for a connecting session"
    );
    update(id, |spare| spare._d3d12 = d3d12);
    std::thread::sleep(LIFETIME);
    let expired = {
        let mut spares = super::super::lock(&SPARES);
        spares
            .iter()
            .position(|s| s.id == id)
            .map(|index| spares.swap_remove(index))
    };
    drop(expired);
}

/// A prepared device of this adapter, created like [`super::create_device`].
/// One still being created is waited for: the driver loads for either.
pub(super) fn take(adapter: u64) -> Option<ID3D11Device> {
    let deadline = Instant::now() + WAIT;
    let mut spares = super::super::lock(&SPARES);
    loop {
        if let Some(spare) = spares
            .iter_mut()
            .find(|s| s.adapter == adapter && s.device.is_some())
        {
            let device = spare.device.take()?;
            drop(spares);
            unsafe { device.GetDeviceRemovedReason() }.ok()?;
            return Some(device);
        }
        let now = Instant::now();
        if now >= deadline || !spares.iter().any(|s| s.adapter == adapter && s.creating) {
            return None;
        }
        spares = READY
            .wait_timeout(spares, deadline - now)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
}

/// The adapter's D3D12 device: one per adapter and process.
fn d3d12(device: &ID3D11Device) -> Result<ID3D12Device> {
    unsafe {
        let adapter = device.cast::<IDXGIDevice>()?.GetAdapter()?;
        let mut d3d12 = None;
        D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut d3d12)?;
        d3d12.context("D3D12 device missing")
    }
}
