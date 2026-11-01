//! The host's cameras for its participants (`docs/protocol/camera-design.md`
//! §4): the inventory, the display IDs cameras take in the display list, and
//! which parameters a running camera uses (the first viewer's).
use crate::media::camera::{Camera, Spec};
use std::collections::{HashMap, HashSet};
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

/// A running camera: the parameters everyone watching it uses, whose they
/// are, and who watches (participant key, name).
struct Running {
    spec: Spec,
    chosen_by: String,
    viewers: Vec<(u64, String)>,
}

static RUNNING: LazyLock<Mutex<HashMap<String, Running>>> = LazyLock::new(Mutex::default);

fn running() -> std::sync::MutexGuard<'static, HashMap<String, Running>> {
    RUNNING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Participants hearing the microphone (participant key, name).
static LISTENERS: Mutex<Vec<(u64, String)>> = Mutex::new(Vec::new());

fn listeners() -> std::sync::MutexGuard<'static, Vec<(u64, String)>> {
    LISTENERS.lock().unwrap_or_else(|e| e.into_inner())
}

/// A participant watching a camera (or hearing the microphone: `None`);
/// leaves on drop.
pub(crate) struct Use {
    camera: Option<String>,
    participant: u64,
}

impl Drop for Use {
    fn drop(&mut self) {
        match &self.camera {
            Some(camera) => {
                let mut running = running();
                if let Some(entry) = running.get_mut(camera) {
                    if let Some(at) = entry.viewers.iter().position(|(key, _)| *key == self.participant) {
                        entry.viewers.remove(at);
                    }
                    if entry.viewers.is_empty() {
                        running.remove(camera);
                    }
                }
            }
            None => {
                let mut listeners = listeners();
                if let Some(at) = listeners.iter().position(|(key, _)| *key == self.participant) {
                    listeners.remove(at);
                }
            }
        }
        notice();
    }
}

/// `participant` hears the microphone while the returned value lives.
pub(crate) fn listen(participant: u64, nickname: &str) -> Arc<Use> {
    listeners().push((participant, nickname.to_owned()));
    notice();
    Arc::new(Use {
        camera: None,
        participant,
    })
}

/// Participants the owner stopped: they may not open a camera again in
/// their session.
static STOPPED: LazyLock<Mutex<HashSet<u64>>> = LazyLock::new(Mutex::default);

/// Whether the owner stopped `participant`'s cameras.
pub(crate) fn stopped(participant: u64) -> bool {
    STOPPED.lock().unwrap_or_else(|e| e.into_inner()).contains(&participant)
}

/// The owner pressed "stop": everyone watching stops, for their session.
fn stop_all() {
    let mut watching: Vec<u64> = running()
        .values()
        .flat_map(|r| r.viewers.iter().map(|(key, _)| *key))
        .collect();
    watching.extend(listeners().iter().map(|(key, _)| *key));
    tracing::info!(participants = watching.len(), "owner stopped camera viewing");
    STOPPED.lock().unwrap_or_else(|e| e.into_inner()).extend(watching);
}

/// The corner notice on this computer: who watches a camera now.
fn notice() {
    if cfg!(test) || !crate::platform::windows::participant_overlay::capture_exclusion() {
        return;
    }
    let watching: Vec<String> = running()
        .values()
        .flat_map(|r| r.viewers.iter().map(|(_, name)| name.clone()))
        .collect();
    let listening: Vec<String> = listeners().iter().map(|(_, name)| name.clone()).collect();
    let text = &crate::i18n::t().camera;
    let mut names: Vec<String> = watching.iter().chain(&listening).cloned().collect();
    names.sort();
    names.dedup();
    let text = match (watching.is_empty(), listening.is_empty()) {
        (true, true) => None,
        (false, true) => Some((text.notice_watching)(&names)),
        (true, false) => Some((text.notice_listening)(&names)),
        (false, false) => Some((text.notice_both)(&names)),
    };
    crate::platform::windows::camera_notice::set(text, crate::i18n::t().camera.stop, stop_all);
}

/// `participant` watches `wanted.camera`: with `wanted` when nobody watches
/// it yet, else with the parameters of the one who started it. Returns them
/// and who chose them.
pub(crate) fn join(wanted: Spec, participant: u64, nickname: &str) -> (Spec, String, Arc<Use>) {
    let mut running = running();
    let camera = wanted.camera.clone();
    let entry = running.entry(camera.clone()).or_insert_with(|| Running {
        spec: wanted.clone(),
        chosen_by: nickname.to_owned(),
        viewers: Vec::new(),
    });
    entry.viewers.push((participant, nickname.to_owned()));
    let joined = (entry.spec.clone(), entry.chosen_by.clone());
    drop(running);
    notice();
    (
        joined.0,
        joined.1,
        Arc::new(Use {
            camera: Some(camera),
            participant,
        }),
    )
}

/// How many participants watch `camera`.
pub(crate) fn viewers(camera: &str) -> usize {
    running().get(camera).map_or(0, |r| r.viewers.len())
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
