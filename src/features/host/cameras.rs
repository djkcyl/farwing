//! The host's cameras for its participants (`docs/protocol/camera-design.md`
//! §4): the inventory, the display IDs cameras take in the display list, and
//! which parameters a running camera uses (the first viewer's).
use crate::media::camera::{Camera, Spec};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Display IDs of cameras: this bit set, the rest from the camera's ID.
pub(crate) const DISPLAY_ID_BIT: i32 = 0x4000_0000;

/// The display ID `camera` takes in the display list (stable across
/// sessions: from its device link).
pub(crate) fn display_id(camera: &str) -> i32 {
    let mut hash = 0x811c_9dc5u32;
    for byte in camera.bytes() {
        hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
    }
    DISPLAY_ID_BIT | (hash & 0x0fff_ffff) as i32
}

/// Whether display `id` is a camera.
pub(crate) fn is_camera(id: i32) -> bool {
    id > 0 && id & DISPLAY_ID_BIT != 0
}

/// A listing this recent is used as it is.
const FRESH: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Inventory {
    cameras: Vec<Camera>,
    read_at: Option<Instant>,
    reading: bool,
}

static INVENTORY: LazyLock<Mutex<Inventory>> = LazyLock::new(Mutex::default);

fn inventory() -> std::sync::MutexGuard<'static, Inventory> {
    INVENTORY.lock().unwrap_or_else(|e| e.into_inner())
}

/// Read the cameras now. A camera this process captures keeps the modes
/// read before (it cannot be opened a second time to read them).
pub(crate) fn read() -> Vec<Camera> {
    let read = crate::platform::camera::list().unwrap_or_else(|error| {
        tracing::info!(error = %format!("{error:#}"), "cameras not listed");
        Vec::new()
    });
    let mut state = inventory();
    let cameras = read
        .into_iter()
        .map(|camera| {
            let running = running().contains_key(&camera.id);
            match state.cameras.iter().find(|c| c.id == camera.id) {
                Some(known) if running && camera.modes.is_empty() => known.clone(),
                _ => camera,
            }
        })
        .collect::<Vec<_>>();
    state.cameras = cameras.clone();
    state.read_at = Some(Instant::now());
    cameras
}

/// The cameras as last read; a read older than a few seconds is renewed in
/// the background (cameras come and go).
pub(crate) fn cameras() -> Vec<Camera> {
    let mut state = inventory();
    let stale = state.read_at.is_none_or(|at| at.elapsed() >= FRESH);
    if stale && !state.reading {
        state.reading = true;
        let started = std::thread::Builder::new().name("camera-inventory".into()).spawn(|| {
            read();
            inventory().reading = false;
        });
        if started.is_err() {
            state.reading = false;
        }
    }
    state.cameras.clone()
}

/// `camera`'s adjustments changed (read them again from its source).
pub(crate) fn controls_changed(camera: &str) {
    let controls = crate::platform::camera::controls(camera).unwrap_or_default();
    if let Some(known) = inventory().cameras.iter_mut().find(|c| c.id == camera) {
        known.controls = controls;
    }
}

/// A running camera: the parameters everyone watching it uses, and whose
/// they are.
struct Running {
    spec: Spec,
    chosen_by: String,
    viewers: usize,
}

static RUNNING: LazyLock<Mutex<HashMap<String, Running>>> = LazyLock::new(Mutex::default);

fn running() -> std::sync::MutexGuard<'static, HashMap<String, Running>> {
    RUNNING.lock().unwrap_or_else(|e| e.into_inner())
}

/// A participant watching a camera; leaves on drop.
pub(crate) struct Use {
    camera: String,
}

impl Drop for Use {
    fn drop(&mut self) {
        let mut running = running();
        if let Some(entry) = running.get_mut(&self.camera) {
            entry.viewers = entry.viewers.saturating_sub(1);
            if entry.viewers == 0 {
                running.remove(&self.camera);
            }
        }
    }
}

/// Watch `wanted.camera`: with `wanted` when nobody watches it yet, else
/// with the parameters of the one who started it. Returns them and who
/// chose them.
pub(crate) fn join(wanted: Spec, nickname: &str) -> (Spec, String, Arc<Use>) {
    let mut running = running();
    let camera = wanted.camera.clone();
    let entry = running.entry(camera.clone()).or_insert_with(|| Running {
        spec: wanted.clone(),
        chosen_by: nickname.to_owned(),
        viewers: 0,
    });
    entry.viewers += 1;
    (entry.spec.clone(), entry.chosen_by.clone(), Arc::new(Use { camera }))
}

/// How many participants watch `camera`.
pub(crate) fn viewers(camera: &str) -> usize {
    running().get(camera).map_or(0, |r| r.viewers)
}

/// Who chose the parameters `camera` runs with, if it runs.
pub(crate) fn chosen_by(camera: &str) -> Option<String> {
    running().get(camera).map(|r| r.chosen_by.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_display_ids_are_stable_and_apart_from_screens() {
        let id = display_id(r"\\?\usb#vid_0bda&pid_58c2#camera");
        assert_eq!(id, display_id(r"\\?\usb#vid_0bda&pid_58c2#camera"));
        assert!(is_camera(id));
        assert!(!is_camera(0) && !is_camera(1) && !is_camera(-1));
        assert_ne!(id, display_id("synthetic"));
    }
}
