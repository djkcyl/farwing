//! Multi-viewer forwarding on the host (`docs/protocol/deployment-design.md`
//! §5.4): when the server forwards and enough participants watch, the host
//! sends its picture once to the server's forwarder and the participants
//! receive it from there. Their own video tracks stop sending (their encoder
//! groups go on, for the uplink); audio, input and every other channel stay
//! on each participant's connection. A participant that cannot receive
//! through the forwarder gets its picture directly again.

use super::peer::uplink::{Cipher, Uplink};
use crate::media::frame_crypto::{Encryptor, FrameKey};
use crate::features::stream_control::wire::{ForwardStatus, ForwardedTrack, MediaForward};
use crate::session::host_client::HostClient;
use farwing_protocol::api::{ForwardMode, Forwarding};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};
use webrtc::ice_transport::ice_server::RTCIceServer;

/// How the host owner wants the picture to pass the forwarder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    /// Frames are encrypted for the participants; the server passes
    /// ciphertext.
    #[default]
    E2ee,
    /// The server can see the picture (browsers without encoded transforms).
    Compatible,
}

static MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The owner's choice, from the host settings.
pub(crate) fn set_mode(mode: Mode) {
    MODE.store(mode == Mode::Compatible, Ordering::Release);
}

fn preferred() -> ForwardMode {
    if MODE.load(Ordering::Acquire) {
        ForwardMode::Compatible
    } else {
        ForwardMode::E2ee
    }
}

/// How often participants and encoder groups are looked at.
const TICK: Duration = Duration::from_millis(250);
/// The server's forwarding settings are read again after this.
const FEATURES_TTL: Duration = Duration::from_secs(60);
/// After a room could not be opened, the next try waits this long.
const RETRY: Duration = Duration::from_secs(30);
/// A participant that could not receive through the forwarder gets its
/// picture directly for this long.
const EXCLUDED: Duration = Duration::from_secs(300);
/// A closed room still forwards this long, until the participants show
/// their own picture again.
const CLOSE_GRACE: Duration = Duration::from_secs(3);
/// A new frame key is used this long after the participants got it.
const KEY_SWITCH: Duration = Duration::from_secs(2);

/// A participant's part in forwarding, read by its capture workers and its
/// sound.
#[derive(Default)]
pub(crate) struct Flags {
    /// In a room: the uplink carries this participant's stream.
    pub(crate) in_room: AtomicBool,
    /// The participant shows the forwarder's picture: its own media stops.
    /// Until then it keeps it, so switching costs the picture nothing.
    pub(crate) receiving: AtomicBool,
    /// The forwarder gives the participant the low layer (§5.5).
    pub(crate) low: AtomicBool,
}

struct Entry {
    session_id: String,
    forwarded: Arc<Flags>,
    /// A native controller watching the desktop.
    eligible: bool,
    excluded_until: Option<Instant>,
    instruction: watch::Sender<Option<MediaForward>>,
    /// Whether the participant may hear the host.
    audio: Arc<dyn Fn() -> bool + Send + Sync>,
    /// The participant's video feedback: that of the forwarder's tracks
    /// reaches its own tracks' recovery.
    recovery: Arc<crate::features::host::recovery::Inbox>,
    /// The room, tracks, frame key and audio track last told.
    told: Option<(String, Vec<(u32, String)>, u8, String)>,
    /// The generation of the last instruction; answers to older ones are
    /// stale.
    generation: u32,
    /// The last generation the participant receives through the forwarder.
    confirmed: u32,
}

impl Entry {
    fn candidate(&self, now: Instant) -> bool {
        self.eligible && self.excluded_until.is_none_or(|until| now >= until)
    }
}

/// End-to-end mode: the frame key participants get, and the uplink's cipher.
struct Keys {
    id: u8,
    material: [u8; crate::media::frame_crypto::KEY_BYTES],
    cipher: Cipher,
    /// The uplink still encrypts with the previous key until then.
    switch_at: Option<Instant>,
}

impl Keys {
    fn new() -> anyhow::Result<Self> {
        let material = FrameKey::generate();
        Ok(Self {
            id: 1,
            material,
            cipher: Arc::new(Mutex::new(Encryptor::new(FrameKey::new(1, &material)?))),
            switch_at: None,
        })
    }

    /// A new key for the participants (one left); the uplink takes it once
    /// they have had time to receive it.
    fn rotate(&mut self) {
        self.id = self.id.wrapping_add(1).max(1);
        self.material = FrameKey::generate();
        self.switch_at = Some(Instant::now() + KEY_SWITCH);
    }

    fn switch_due(&mut self) {
        if self.switch_at.is_some_and(|at| Instant::now() >= at) {
            self.switch_at = None;
            if let Ok(key) = FrameKey::new(self.id, &self.material) {
                self.cipher.lock().unwrap_or_else(|e| e.into_inner()).set_key(key);
                tracing::info!(key = self.id, "forwarded frames use the new key");
            }
        }
    }
}

struct Room {
    id: String,
    mode: ForwardMode,
    keys: Option<Keys>,
    uplink: Arc<Uplink>,
    sessions: BTreeSet<String>,
}

#[derive(Default)]
struct State {
    entries: BTreeMap<u64, Entry>,
    client: Option<HostClient>,
    ice: Vec<RTCIceServer>,
    /// The host's settings for the uplink's sound.
    host: Option<crate::features::host::access::Handle>,
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

/// Forwarding changed (an encoder group ended or formed, the forwarder's
/// low layer viewers changed): look now rather than on the next tick.
pub(crate) fn wake() {
    WAKE.notify_one();
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

/// Participant `key` watches the picture again (a candidate for the room),
/// or only listens (audio only: not in the room, nothing forwarded to it).
pub(crate) fn set_eligible(key: u64, eligible: bool) {
    let mut state = state();
    let Some(entry) = state.entries.get_mut(&key) else {
        return;
    };
    if entry.eligible == eligible {
        return;
    }
    entry.eligible = eligible;
    drop(state);
    WAKE.notify_one();
}

/// Registers participant `key` of server session `session_id`; `forwarded`
/// are the flags its tracks read.
pub(crate) fn register(
    key: u64,
    session_id: String,
    forwarded: Arc<Flags>,
    eligible: bool,
    ice: Vec<RTCIceServer>,
    host: crate::features::host::access::Handle,
    audio: Arc<dyn Fn() -> bool + Send + Sync>,
    recovery: Arc<crate::features::host::recovery::Inbox>,
) -> Registration {
    let (instruction, receiver) = watch::channel(None);
    let mut state = state();
    if state.ice.is_empty() {
        state.ice = ice;
    }
    state.host.get_or_insert(host);
    state.entries.insert(
        key,
        Entry {
            session_id,
            forwarded,
            eligible,
            excluded_until: None,
            instruction,
            audio,
            recovery,
            told: None,
            generation: 0,
            confirmed: 0,
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
        let mut generation = state.generation;
        let Some(entry) = state.entries.get_mut(&self.key) else {
            return;
        };
        if status.generation != entry.generation {
            tracing::debug!(participant = self.key, generation = status.generation,
                "stale forwarding status");
            return;
        }
        if status.receiving {
            tracing::info!(participant = self.key, generation = status.generation,
                "participant receives through the forwarder");
            if entry.told.as_ref().is_some_and(|(room, ..)| !room.is_empty()) {
                entry.forwarded.receiving.store(true, Ordering::Release);
            }
            entry.confirmed = status.generation;
            // Back on the main stream: the participant leaves the low layer.
            drop(state);
            WAKE.notify_one();
            return;
        }
        tracing::warn!(participant = self.key, error = %status.error,
            "participant cannot receive through the forwarder; sending directly");
        entry.excluded_until = Some(Instant::now() + EXCLUDED);
        direct(entry, &mut generation);
        state.generation = generation;
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

/// Back to direct media for `entry`; telling it takes the next generation.
fn direct(entry: &mut Entry, generation: &mut u32) {
    entry.forwarded.receiving.store(false, Ordering::Release);
    entry.forwarded.in_room.store(false, Ordering::Release);
    entry.forwarded.low.store(false, Ordering::Release);
    if entry.told.take().is_some() {
        entry.recovery.forward(&[]);
        *generation = generation.wrapping_add(1);
        let generation = *generation;
        entry.generation = generation;
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
    // The owner's mode when the server allows it; end-to-end otherwise.
    let mode = forwarding.as_ref().map(|f| {
        let preferred = preferred();
        if f.modes.contains(&preferred) {
            preferred
        } else {
            ForwardMode::E2ee
        }
    });
    let wanted = forwarding
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
                    // Someone left: the frames get a key they do not have.
                    if !room.sessions.is_subset(&sessions)
                        && let Some(keys) = room.keys.as_mut()
                    {
                        keys.rotate();
                    }
                    room.sessions = sessions.clone();
                }
            }
            Err(error) => tracing::warn!(%error, "forwarding room sessions not updated"),
        }
    }
    let mut state = state();
    let Some(room) = state.room.as_mut() else { return true };
    if let Some(keys) = room.keys.as_mut() {
        keys.switch_due();
    }
    let (room_id, mode, uplink) = (room.id.clone(), room.mode, room.uplink.clone());
    let frame_key = room.keys.as_ref().map(|k| (k.id, k.material.to_vec()));
    let room_sessions = room.sessions.clone();
    let mapping = uplink.reconcile();
    let mut generation = state.generation;
    for (key, entry) in state.entries.iter_mut() {
        if !entry.candidate(now) || !room_sessions.contains(&entry.session_id) {
            direct(entry, &mut generation);
            continue;
        }
        entry.forwarded.in_room.store(true, Ordering::Release);
        let mut tracks: Vec<(u32, String)> = mapping
            .iter()
            .filter(|((participant, _), _)| participant == key)
            .map(|((_, index), track)| (*index as u32, track.clone()))
            .collect();
        let mut low = uplink.low(&entry.session_id);
        // Back to the main stream: the participant is told the main
        // stream's tracks at once but stays in the low layer's group (which
        // ends with its last member) until its viewer receives them (先接后断).
        if !low && !tracks.is_empty() {
            let main: Option<Vec<(u32, String)>> = tracks
                .iter()
                .map(|(index, track)| uplink.main_track(track).map(|main| (*index, main)))
                .collect();
            if let Some(main) = main {
                low = entry.confirmed != entry.generation
                    || entry.told.as_ref().is_none_or(|(_, told, ..)| *told != main);
                tracks = main;
            }
        }
        entry.forwarded.low.store(low, Ordering::Release);
        if tracks.is_empty() {
            continue;
        }
        let key_id = frame_key.as_ref().map_or(0, |(id, _)| *id);
        let audio = if uplink.has_audio() && (entry.audio)() {
            super::peer::uplink::AUDIO_TRACK.to_owned()
        } else {
            String::new()
        };
        let told = Some((room_id.clone(), tracks.clone(), key_id, audio.clone()));
        if entry.told != told {
            let forwarded: Vec<(u32, usize)> = tracks
                .iter()
                .filter_map(|(index, track)| uplink.ssrc(track).map(|ssrc| (ssrc, *index as usize)))
                .collect();
            entry.recovery.forward(&forwarded);
            generation = generation.wrapping_add(1);
            entry.told = told;
            entry.generation = generation;
            entry.instruction.send_replace(Some(MediaForward {
                generation,
                room: room_id.clone(),
                mode: mode_name(mode).to_owned(),
                tracks: tracks
                    .into_iter()
                    .map(|(index, track)| ForwardedTrack { index, track })
                    .collect(),
                session: entry.session_id.clone(),
                key: frame_key.as_ref().map(|(_, k)| k.clone()).unwrap_or_default(),
                key_id: u32::from(key_id),
                audio,
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
    let result: anyhow::Result<(String, Arc<Uplink>, Option<Keys>)>;
    let (ice, host) = {
        let state = state();
        (state.ice.clone(), state.host.clone())
    };
    let sessions: Vec<String> = candidates.values().cloned().collect();
    result = async {
        let keys = (mode == ForwardMode::E2ee).then(Keys::new).transpose()?;
        let (uplink, offer) =
            Uplink::new(ice, keys.as_ref().map(|k| k.cipher.clone()), host).await?;
        let uplink = Arc::new(uplink);
        match client.sfu_create_room(sessions.clone(), mode, offer).await {
            Ok(created) => {
                uplink.answer(created.answer).await?;
                tracing::info!(room = %created.room, node = ?created.node, participants = sessions.len(),
                    mode = mode_name(mode), "forwarding room opened");
                Ok((created.room, uplink, keys))
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
        Ok((id, uplink, keys)) => {
            state.retry_at = None;
            state.room = Some(Room {
                id,
                mode,
                keys,
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
            direct(entry, &mut generation);
        }
        state.generation = generation;
        if failed {
            state.retry_at = Some(Instant::now() + RETRY);
        }
        room
    };
    let Some(room) = room else { return };
    tracing::info!(room = %room.id, failed, "forwarding room closed; sending directly");
    // The participants show the forwarder's picture until their own
    // arrives: the room forwards a little longer.
    let client = client.clone();
    let finish = async move {
        if !failed {
            tokio::time::sleep(CLOSE_GRACE).await;
        }
        room.uplink.close().await;
        if let Err(error) = client.sfu_close_room(room.id).await {
            tracing::debug!(%error, "forwarding room not closed on the server");
        }
    };
    tokio::spawn(finish);
}
