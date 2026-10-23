//! Multi-viewer forwarding on the host (`docs/protocol/deployment-design.md`
//! §5.4): when the server forwards and enough participants watch, the host
//! sends its picture once to the server's forwarder and the participants
//! receive it from there. Their own video tracks stop sending (their encoder
//! groups go on, for the uplink); audio, input and every other channel stay
//! on each participant's connection. A participant that cannot receive
//! through the forwarder gets its picture directly again.

use super::peer::uplink::Uplink;
use crate::features::stream_control::wire::{ForwardStatus, ForwardedTrack, MediaForward};
use crate::session::host_client::HostClient;
use farwing_protocol::api::{ForwardMode, Forwarding};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};
use webrtc::ice_transport::ice_server::RTCIceServer;

/// How often participants and encoder groups are looked at.
const TICK: Duration = Duration::from_millis(250);
/// The server's forwarding settings are read again after this.
const FEATURES_TTL: Duration = Duration::from_secs(60);
/// After a room could not be opened, the next try waits this long.
const RETRY: Duration = Duration::from_secs(30);
/// A participant that could not receive through the forwarder gets its
/// picture directly for this long.
const EXCLUDED: Duration = Duration::from_secs(300);

struct Entry {
    session_id: String,
    /// Read by the participant's capture workers.
    forwarded: Arc<AtomicBool>,
    /// A native controller watching the desktop.
    eligible: bool,
    excluded_until: Option<Instant>,
    instruction: watch::Sender<Option<MediaForward>>,
    /// The room and tracks last told.
    told: Option<(String, Vec<(u32, String)>)>,
}

impl Entry {
    fn candidate(&self, now: Instant) -> bool {
        self.eligible && self.excluded_until.is_none_or(|until| now >= until)
    }
}

struct Room {
    id: String,
    mode: ForwardMode,
    uplink: Arc<Uplink>,
    sessions: BTreeSet<String>,
}

#[derive(Default)]
struct State {
    entries: BTreeMap<u64, Entry>,
    client: Option<HostClient>,
    ice: Vec<RTCIceServer>,
    features: Option<(Option<Forwarding>, Instant)>,
    room: Option<Room>,
    retry_at: Option<Instant>,
    generation: u32,
    running: bool,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);
static WAKE: LazyLock<Notify> = LazyLock::new(Notify::new);

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The server connection rooms are opened with.
pub(crate) fn set_client(client: HostClient) {
    state().client = Some(client);
}

/// A participant's place in forwarding; leaves on drop.
pub(crate) struct Registration {
    key: u64,
    instruction: watch::Receiver<Option<MediaForward>>,
}

/// Registers participant `key` of server session `session_id`; `forwarded`
/// is the flag its tracks read.
pub(crate) fn register(
    key: u64,
    session_id: String,
    forwarded: Arc<AtomicBool>,
    eligible: bool,
    ice: Vec<RTCIceServer>,
) -> Registration {
    let (instruction, receiver) = watch::channel(None);
    let mut state = state();
    if state.ice.is_empty() {
        state.ice = ice;
    }
    state.entries.insert(
        key,
        Entry {
            session_id,
            forwarded,
            eligible,
            excluded_until: None,
            instruction,
            told: None,
        },
    );
    if !state.running {
        state.running = true;
        tokio::spawn(run());
    }
    drop(state);
    WAKE.notify_one();
    Registration {
        key,
        instruction: receiver,
    }
}

impl Registration {
    /// What to tell the participant; `None` until there is something.
    pub(crate) fn instructions(&self) -> watch::Receiver<Option<MediaForward>> {
        self.instruction.clone()
    }

    /// The participant's answer to an instruction.
    pub(crate) fn status(&self, status: &ForwardStatus) {
        let mut state = state();
        let generation = state.generation;
        let Some(entry) = state.entries.get_mut(&self.key) else {
            return;
        };
        if status.receiving {
            tracing::info!(participant = self.key, generation = status.generation,
                "participant receives through the forwarder");
            return;
        }
        tracing::warn!(participant = self.key, error = %status.error,
            "participant cannot receive through the forwarder; sending directly");
        entry.excluded_until = Some(Instant::now() + EXCLUDED);
        direct(entry, generation.wrapping_add(1));
        state.generation = state.generation.wrapping_add(1);
        drop(state);
        WAKE.notify_one();
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        state().entries.remove(&self.key);
        WAKE.notify_one();
    }
}

/// Back to direct media for `entry`.
fn direct(entry: &mut Entry, generation: u32) {
    entry.forwarded.store(false, Ordering::Release);
    if entry.told.take().is_some() {
        entry.instruction.send_replace(Some(MediaForward {
            generation,
            session: entry.session_id.clone(),
            ..Default::default()
        }));
    }
}

async fn run() {
    loop {
        tokio::select! {
            _ = WAKE.notified() => {}
            _ = tokio::time::sleep(TICK) => {}
        }
        if !step().await {
            break;
        }
    }
}

/// One look at the participants; `false` once there is nothing left to do.
async fn step() -> bool {
    let now = Instant::now();
    let (client, features_due) = {
        let mut state = state();
        if state.entries.is_empty() && state.room.is_none() {
            state.running = false;
            return false;
        }
        let due = state
            .features
            .as_ref()
            .is_none_or(|(_, at)| at.elapsed() >= FEATURES_TTL);
        (state.client.clone(), due)
    };
    let Some(client) = client else { return true };
    if features_due {
        let forwarding = client.forwarding().await.ok().flatten();
        state().features = Some((forwarding, Instant::now()));
    }

    let (forwarding, candidates, room) = {
        let state = state();
        let forwarding = state.features.as_ref().and_then(|(f, _)| f.clone());
        let candidates: BTreeMap<u64, String> = state
            .entries
            .iter()
            .filter(|(_, e)| e.candidate(now))
            .map(|(key, e)| (*key, e.session_id.clone()))
            .collect();
        (forwarding, candidates, state.room.as_ref().map(|r| r.id.clone()))
    };
    // Frames are encrypted for the participants only with end-to-end
    // encryption, which comes with the mode setting; until then forwarding
    // needs compatible mode.
    let mode = forwarding
        .as_ref()
        .filter(|f| f.modes.contains(&ForwardMode::Compatible))
        .map(|_| ForwardMode::Compatible);
    let wanted = mode.is_some()
        && forwarding
            .as_ref()
            .is_some_and(|f| candidates.len() >= f.threshold as usize);

    let failed = state().room.as_ref().is_some_and(|r| r.uplink.failed());
    if room.is_some() && (!wanted || failed) {
        close(&client, failed).await;
        return true;
    }
    if room.is_none() {
        if wanted && state().retry_at.is_none_or(|at| now >= at) {
            open(&client, mode.unwrap_or_default(), &candidates).await;
        }
        return true;
    }

    // The room's sessions follow the candidates.
    let sessions: BTreeSet<String> = candidates.values().cloned().collect();
    let (room_id, changed) = {
        let state = state();
        let room = state.room.as_ref().expect("room");
        (room.id.clone(), room.sessions != sessions)
    };
    if changed {
        match client
            .sfu_update_room(room_id.clone(), sessions.iter().cloned().collect())
            .await
        {
            Ok(()) => {
                if let Some(room) = state().room.as_mut() {
                    room.sessions = sessions.clone();
                }
            }
            Err(error) => tracing::warn!(%error, "forwarding room sessions not updated"),
        }
    }
    let mut state = state();
    let Some(room) = state.room.as_ref() else { return true };
    let (room_id, mode, uplink) = (room.id.clone(), room.mode, room.uplink.clone());
    let room_sessions = room.sessions.clone();
    let mapping = uplink.reconcile();
    let mut generation = state.generation;
    for (key, entry) in state.entries.iter_mut() {
        if !entry.candidate(now) || !room_sessions.contains(&entry.session_id) {
            generation = generation.wrapping_add(1);
            direct(entry, generation);
            continue;
        }
        entry.forwarded.store(true, Ordering::Release);
        let tracks: Vec<(u32, String)> = mapping
            .iter()
            .filter(|((participant, _), _)| participant == key)
            .map(|((_, index), track)| (*index as u32, track.clone()))
            .collect();
        if tracks.is_empty() {
            continue;
        }
        let told = Some((room_id.clone(), tracks.clone()));
        if entry.told != told {
            generation = generation.wrapping_add(1);
            entry.told = told;
            entry.instruction.send_replace(Some(MediaForward {
                generation,
                room: room_id.clone(),
                mode: mode_name(mode).to_owned(),
                tracks: tracks
                    .into_iter()
                    .map(|(index, track)| ForwardedTrack { index, track })
                    .collect(),
                session: entry.session_id.clone(),
            }));
        }
    }
    state.generation = generation;
    true
}

fn mode_name(mode: ForwardMode) -> &'static str {
    match mode {
        ForwardMode::E2ee => "e2ee",
        ForwardMode::Compatible => "compatible",
    }
}

async fn open(client: &HostClient, mode: ForwardMode, candidates: &BTreeMap<u64, String>) {
    let ice = state().ice.clone();
    let sessions: Vec<String> = candidates.values().cloned().collect();
    let result = async {
        let (uplink, offer) = Uplink::new(ice).await?;
        let uplink = Arc::new(uplink);
        match client.sfu_create_room(sessions.clone(), mode, offer).await {
            Ok(created) => {
                uplink.answer(created.answer).await?;
                tracing::info!(room = %created.room, node = ?created.node, participants = sessions.len(),
                    "forwarding room opened");
                Ok((created.room, uplink))
            }
            Err(error) => {
                uplink.close().await;
                Err(error)
            }
        }
    }
    .await;
    let mut state = state();
    match result {
        Ok((id, uplink)) => {
            state.retry_at = None;
            state.room = Some(Room {
                id,
                mode,
                uplink,
                sessions: sessions.into_iter().collect(),
            });
        }
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "forwarding room not opened; sending directly");
            state.retry_at = Some(Instant::now() + RETRY);
        }
    }
    drop(state);
    WAKE.notify_one();
}

async fn close(client: &HostClient, failed: bool) {
    let room = {
        let mut state = state();
        let room = state.room.take();
        let mut generation = state.generation;
        for entry in state.entries.values_mut() {
            generation = generation.wrapping_add(1);
            direct(entry, generation);
        }
        state.generation = generation;
        if failed {
            state.retry_at = Some(Instant::now() + RETRY);
        }
        room
    };
    let Some(room) = room else { return };
    tracing::info!(room = %room.id, failed, "forwarding room closed; sending directly");
    room.uplink.close().await;
    if let Err(error) = client.sfu_close_room(room.id).await {
        tracing::debug!(%error, "forwarding room not closed on the server");
    }
}
