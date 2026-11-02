//! The chat service's voice (`docs/protocol/chat-design.md` §7): joining
//! and leaving channels, calls, and the `voice.*` / `call.*` pushes for the
//! session this device is in.

use super::{ChatService, Inner, lock, update};
use crate::features::chat::state::MyVoice;
use crate::features::voice::keys::DeviceKeys;
use crate::features::voice::session::{CallVoice, ChannelVoice, Event};
use crate::features::voice::settings::{self, VoiceSettings};
use crate::features::voice::capture::Capture;
use crate::features::voice::engine::Engine;
use crate::features::voice::sounds::{self, Sound};
use farwing_protocol::chat::{
    Call, CallEnded, CallSignal, Id, VoiceKeyPush, VoiceLeft, VoiceMode, VoiceMoved, VoiceOffer, types,
};
use reqwest::Method;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// The microphone test records this long, then plays it back.
const TEST: Duration = Duration::from_secs(5);
/// The settings' meter keeps the microphone open this long after it was
/// last shown.
const PREVIEW_KEEP: Duration = Duration::from_millis(1500);

/// The microphone outside a voice session: the settings' meter and the
/// microphone test.
pub(super) struct Preview {
    capture: Capture,
    polled: Instant,
    /// Started at, and what it recorded.
    recording: Option<(Instant, Vec<f32>)>,
    /// Playing back until.
    playing: Option<Instant>,
}

/// Where the microphone test is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MicTest {
    Idle,
    /// Seconds left.
    Recording(u64),
    Playing,
}

/// What the voice sounds last saw.
#[derive(Default)]
pub(super) struct SoundState {
    connected: bool,
    channel: Option<Id>,
    people: Vec<u32>,
    /// Those in the channel sharing their screen.
    live: Vec<u32>,
    muted: bool,
    deafened: bool,
    rang: Option<std::time::Instant>,
    /// The channel where another device of this account was pointed out.
    hinted: Option<Id>,
}

#[derive(Clone)]
pub(super) enum Active {
    Channel(Arc<ChannelVoice>),
    Call(Arc<CallVoice>),
}

/// Pictures alike in a channel and a call.
impl Active {
    async fn set_camera(&self, spec: Option<crate::media::camera::Spec>) -> anyhow::Result<()> {
        match self {
            Self::Channel(v) => v.set_camera(spec).await,
            Self::Call(c) => c.set_camera(spec).await,
        }
    }

    fn camera_on(&self) -> bool {
        match self {
            Self::Channel(v) => v.camera_on(),
            Self::Call(c) => c.camera_on(),
        }
    }

    async fn set_screen(
        &self,
        share: Option<(crate::features::host::peer::chat_video::Shown, crate::features::host::peer::chat_video::Quality, bool)>,
    ) -> anyhow::Result<()> {
        match self {
            Self::Channel(v) => v.set_screen(share).await,
            Self::Call(c) => c.set_screen(share).await,
        }
    }

    fn screen(&self) -> Option<farwing_protocol::chat::VoiceScreen> {
        match self {
            Self::Channel(v) => v.screen(),
            Self::Call(c) => c.screen(),
        }
    }

    async fn follow_share(&self) -> bool {
        match self {
            Self::Channel(v) => v.follow_share().await,
            Self::Call(c) => c.follow_share().await,
        }
    }

    fn following(&self) -> &std::sync::atomic::AtomicBool {
        match self {
            Self::Channel(v) => &v.following,
            Self::Call(c) => &c.following,
        }
    }

    fn pictures(&self) -> &crate::features::voice::video::Pictures {
        match self {
            Self::Channel(v) => &v.pictures,
            Self::Call(c) => &c.pictures,
        }
    }

    fn own_picture(&self, kind: &str) -> Option<crate::features::voice::video::Picture> {
        match self {
            Self::Channel(v) => v.own_picture(kind),
            Self::Call(c) => c.own_picture(kind),
        }
    }
}

/// What to share in a voice channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShareTarget {
    /// A screen by ID (the primary if unset).
    Screen(Option<i32>),
    /// A window (its handle) and the program it belongs to.
    Window { hwnd: isize, pid: u32 },
}

impl Active {
    pub(super) async fn close(&self, tell: bool) {
        match self {
            Self::Channel(v) => v.close(tell).await,
            Self::Call(c) => {
                if tell {
                    c.hang_up().await;
                }
                c.close().await;
            }
        }
    }
}

/// The text for a refused join or a lost channel.
fn reason_text(reason: &str) -> &'static str {
    let v = &crate::i18n::t().voice;
    match reason {
        "voice_full" => v.full,
        "voice_no_forwarding" => v.no_forwarding,
        "voice_quota" => v.quota,
        "disconnected" => v.left_disconnected,
        "replaced" => v.left_replaced,
        "forbidden" | "chat_forbidden" => v.left_forbidden,
        "idle" => v.left_idle,
        "ended" => v.left_ended,
        _ => v.failed,
    }
}

fn error_reason(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    for r in ["voice_full", "voice_no_forwarding", "voice_quota", "chat_forbidden"] {
        if text.contains(r) {
            return r.to_owned();
        }
    }
    text
}

/// The device's keys, registered with the server once.
async fn keys(inner: &Arc<Inner>) -> anyhow::Result<Arc<DeviceKeys>> {
    if let Some(k) = lock(&inner.voice_keys).clone() {
        return Ok(k);
    }
    let identity = inner.client.device_identity().ok_or_else(|| anyhow::anyhow!("no device identity"))?;
    let keys = Arc::new(DeviceKeys::new(identity));
    inner
        .client
        .chat(
            Method::PUT,
            "/api/v2/devices/self/x25519",
            Some(serde_json::to_value(keys.registration())?),
        )
        .await?;
    *lock(&inner.voice_keys) = Some(Arc::clone(&keys));
    Ok(keys)
}

/// The mute and deafen keys, while a session runs (also with the window
/// closed).
fn toggles(inner: &Arc<Inner>, engine: crate::features::voice::engine::Engine) {
    let inner = Arc::clone(inner);
    inner.runtime.clone().spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(50));
        let mut told = None;
        loop {
            tick.tick().await;
            if inner.stop.is_cancelled() || lock(&inner.voice).is_none() {
                return;
            }
            // The microphone failed (or went away): said once.
            let failed = engine.capture_error();
            if failed.is_some() && failed != told {
                let v = &crate::i18n::t().voice;
                let text = if failed.as_deref() == Some(v.no_microphone) { v.no_microphone } else { v.microphone_lost };
                update(&inner, |s| s.errors.push(text.to_owned()));
            }
            told = failed;
            let pressed = engine.take_toggles();
            if pressed == 0 {
                continue;
            }
            let (muted, deafened) = {
                let state = lock(&inner.state);
                state.my_voice.as_ref().map_or((false, false), |v| (v.muted, v.deafened))
            };
            let muted = if pressed & 1 != 0 { !muted } else { muted };
            let deafened = if pressed & 2 != 0 { !deafened } else { deafened };
            ChatService { inner: Arc::clone(&inner) }.set_voice_self(muted, deafened);
        }
    });
}

/// Follows a session's events into the state.
fn watch(inner: &Arc<Inner>, mut events: mpsc::UnboundedReceiver<Event>) {
    let inner = Arc::clone(inner);
    inner.runtime.clone().spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                Event::Connected(up) => update(&inner, |s| {
                    if let Some(v) = &mut s.my_voice {
                        v.connected = up;
                    }
                }),
                Event::Failed(e) => {
                    tracing::warn!(error = %e, "voice failed");
                    update(&inner, |s| s.errors.push(crate::i18n::t().voice.failed.to_owned()));
                }
            }
        }
    });
}

impl ChatService {
    pub(crate) fn voice_settings(&self) -> VoiceSettings {
        lock(&self.inner.voice_settings).clone()
    }

    pub(crate) fn set_voice_settings(&self, s: VoiceSettings) {
        if let Err(e) = settings::save(&s) {
            tracing::warn!(error = %format!("{e:#}"), "voice settings not saved");
        }
        *lock(&self.inner.voice_settings) = s.clone();
        // The settings' microphone opens again with the new devices.
        lock(&self.inner.preview).take();
        if let Some(active) = lock(&self.inner.voice).clone() {
            match active {
                Active::Channel(v) => v.engine.set_settings(&s),
                Active::Call(c) => c.engine.set_settings(&s),
            }
        }
    }

    /// Who speaks in this device's session, and whether it does; the
    /// microphone's level.
    pub(crate) fn voice_activity(&self) -> (Vec<u32>, bool, f32) {
        match lock(&self.inner.voice).clone() {
            Some(Active::Channel(v)) => {
                let (who, me) = v.engine.speaking();
                (who, me, v.engine.input_level())
            }
            Some(Active::Call(c)) => {
                let (who, me) = c.engine.speaking();
                (who, me, c.engine.input_level())
            }
            None => (Vec::new(), false, 0.0),
        }
    }

    /// The microphone's level (0–1) for the settings: the session's, or
    /// the microphone opened for the settings meanwhile.
    pub(crate) fn input_meter(&self) -> f32 {
        let gain = f32::from(lock(&self.inner.voice_settings).input_volume) / 100.0;
        if let Some(active) = lock(&self.inner.voice).clone() {
            let level = match active {
                Active::Channel(v) => v.engine.input_level(),
                Active::Call(c) => c.engine.input_level(),
            };
            return (level * gain).min(1.0);
        }
        let mut preview = self.preview();
        let Some(p) = preview.as_mut() else { return 0.0 };
        p.polled = Instant::now();
        (p.capture.peak() * gain).min(1.0)
    }

    /// The microphone for the settings, opened when needed.
    fn preview(&self) -> std::sync::MutexGuard<'_, Option<Preview>> {
        let mut preview = lock(&self.inner.preview);
        if preview.is_none() {
            let settings = lock(&self.inner.voice_settings).clone();
            *preview = Some(Preview {
                capture: Capture::start(Engine::capture_config(&settings)),
                polled: Instant::now(),
                recording: None,
                playing: None,
            });
            let inner = Arc::clone(&self.inner);
            self.inner.runtime.spawn(preview_loop(inner));
        }
        preview
    }

    /// Records five seconds of the microphone and plays them back (not
    /// while in voice: the session has the microphone).
    pub(crate) fn mic_test(&self) {
        if lock(&self.inner.voice).is_some() {
            return;
        }
        if let Some(p) = self.preview().as_mut()
            && p.recording.is_none()
        {
            p.recording = Some((Instant::now(), Vec::new()));
            p.polled = Instant::now();
        }
    }

    pub(crate) fn mic_test_state(&self) -> MicTest {
        let preview = lock(&self.inner.preview);
        match preview.as_ref() {
            Some(Preview { recording: Some((at, _)), .. }) => {
                MicTest::Recording(TEST.saturating_sub(at.elapsed()).as_secs() + 1)
            }
            Some(Preview { playing: Some(until), .. }) if Instant::now() < *until => MicTest::Playing,
            _ => MicTest::Idle,
        }
    }

    /// Plays `sound` as the settings say (its kind on, its volume).
    pub(crate) fn play_sound(&self, sound: Sound) {
        play_sound(&self.inner, sound);
    }

    /// Plays the sounds the state's changes call for (each frame).
    pub(crate) fn tick_sounds(&self, view: &crate::features::chat::state::ChatState) {
        let mut last = lock(&self.inner.sounds);
        let mut sounds = Vec::new();
        let me = view.me().map(|u| u.user_id.clone()).unwrap_or_default();
        let dnd = view.do_not_disturb();
        let calls = view.snap.as_ref().map(|s| s.calls.as_slice()).unwrap_or(&[]);
        let ring = calls.iter().find(|c| c.state == "ringing").map(|c| c.caller.user_id != me);
        match ring {
            Some(incoming) if last.rang.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(3)) => {
                if !(incoming && dnd) {
                    sounds.push(if incoming { Sound::Ring } else { Sound::Ringback });
                }
                last.rang = Some(std::time::Instant::now());
            }
            None => last.rang = None,
            _ => {}
        }
        let voice = view.my_voice.as_ref();
        let connected = voice.is_some_and(|v| v.connected);
        if connected && !last.connected {
            sounds.push(Sound::Joined);
        } else if !connected && last.connected {
            sounds.push(Sound::Left);
        }
        let channel = voice.filter(|v| v.call.is_none()).map(|v| v.channel);
        let here = channel.and_then(|c| view.snap.as_ref()?.voice.iter().find(|v| v.channel_id == c));
        let people: Vec<u32> = here.map(|v| v.participants.iter().map(|p| p.participant).collect()).unwrap_or_default();
        let live: Vec<u32> = here
            .map(|v| {
                v.participants
                    .iter()
                    .filter(|p| p.tracks.iter().any(|k| k == farwing_protocol::chat::track::SCREEN))
                    .map(|p| p.participant)
                    .collect()
            })
            .unwrap_or_default();
        if connected && channel == last.channel {
            let mine = voice.map(|v| v.participant).unwrap_or(0);
            if people.iter().any(|p| *p != mine && !last.people.contains(p)) {
                sounds.push(Sound::Came);
            } else if last.people.iter().any(|p| *p != mine && !people.contains(p)) {
                sounds.push(Sound::Went);
            }
            if live.iter().any(|p| !last.live.contains(p)) {
                sounds.push(Sound::LiveStarted);
            } else if last.live.iter().any(|p| !live.contains(p) && people.contains(p)) {
                sounds.push(Sound::LiveEnded);
            }
        }
        let (muted, deafened) = voice.map_or((last.muted, last.deafened), |v| (v.muted, v.deafened));
        if voice.is_some() && (muted != last.muted || deafened != last.deafened) {
            sounds.push(if muted || deafened { Sound::Muted } else { Sound::Unmuted });
        }
        // Another device of this account in the same channel: said once.
        let mut hinted = last.hinted;
        if let (Some(c), Some(v)) = (channel, voice)
            && hinted != Some(c)
            && view
                .snap
                .as_ref()
                .and_then(|s| s.voice.iter().find(|x| x.channel_id == c))
                .is_some_and(|x| x.participants.iter().any(|p| p.user_id == me && p.participant != v.participant))
        {
            hinted = Some(c);
            update(&self.inner, |s| s.errors.push(crate::i18n::t().voice.other_device.to_owned()));
        }
        *last = SoundState {
            connected,
            channel,
            people,
            live,
            muted,
            deafened,
            rang: last.rang,
            hinted,
        };
        drop(last);
        for s in sounds {
            play_sound(&self.inner, s);
        }
    }

    /// What each sender's track brought (`dev voice`).
    pub(crate) fn voice_stats(&self) -> Vec<crate::features::voice::engine::ReceiverStats> {
        match lock(&self.inner.voice).clone() {
            Some(Active::Channel(v)) => v.engine.stats(),
            Some(Active::Call(c)) => c.engine.stats(),
            None => Vec::new(),
        }
    }

    /// Joins voice channel `channel` (leaving any other voice first).
    pub(crate) fn join_voice(&self, channel: Id) {
        let inner = Arc::clone(&self.inner);
        let (muted, deafened) = *lock(&self.inner.voice_self);
        self.inner.runtime.spawn(async move {
            let old = lock(&inner.voice).take();
            if let Some(old) = old {
                old.close(true).await;
            }
            update(&inner, |s| {
                s.my_voice = Some(MyVoice {
                    channel,
                    muted,
                    deafened,
                    ..Default::default()
                });
            });
            *lock(&inner.voice_joining) = Some((channel, Vec::new()));
            let result = async {
                let keys = keys(&inner).await?;
                let settings = lock(&inner.voice_settings).clone();
                let (tx, rx) = mpsc::unbounded_channel();
                let voice = ChannelVoice::join(
                    Arc::clone(&inner.client),
                    keys,
                    inner.outgoing.clone(),
                    channel,
                    settings,
                    muted,
                    deafened,
                    tx,
                )
                .await?;
                watch(&inner, rx);
                Ok::<_, anyhow::Error>(voice)
            }
            .await;
            match result {
                Ok(voice) => {
                    update(&inner, |s| {
                        if let Some(v) = &mut s.my_voice {
                            v.participant = voice.participant;
                            v.compat = voice.mode == VoiceMode::Compatible;
                        }
                    });
                    toggles(&inner, voice.engine.clone());
                    *lock(&inner.voice) = Some(Active::Channel(voice));
                    // What came while joining.
                    let waiting = lock(&inner.voice_joining).take().map(|(_, w)| w).unwrap_or_default();
                    for (kind, data) in waiting {
                        push(&inner, &kind, &data);
                    }
                }
                Err(e) => {
                    lock(&inner.voice_joining).take();
                    tracing::warn!(error = %format!("{e:#}"), "voice join failed");
                    let why = reason_text(&error_reason(&e));
                    update(&inner, |s| {
                        s.end_voice();
                        s.errors.push(why.to_owned());
                    });
                }
            }
        });
    }

    /// Leaves the voice channel or ends the call.
    pub(crate) fn leave_voice(&self) {
        let inner = Arc::clone(&self.inner);
        self.inner.runtime.spawn(async move {
            let active = lock(&inner.voice).take();
            update(&inner, |s| s.end_voice());
            if let Some(a) = active {
                a.close(true).await;
            }
        });
    }

    pub(crate) fn set_voice_self(&self, muted: bool, deafened: bool) {
        *lock(&self.inner.voice_self) = (muted, deafened);
        update(&self.inner, |s| {
            if let Some(v) = &mut s.my_voice {
                v.muted = muted;
                v.deafened = deafened;
            }
        });
        let active = lock(&self.inner.voice).clone();
        self.inner.runtime.spawn(async move {
            match active {
                Some(Active::Channel(v)) => v.set_self(muted, deafened).await,
                Some(Active::Call(c)) => c.engine.set_self(muted, deafened),
                None => {}
            }
        });
    }

    /// Turns the camera on or off in this device's voice channel.
    pub(crate) fn set_camera(&self, on: bool) {
        let Some(v) = lock(&self.inner.voice).clone() else { return };
        let inner = Arc::clone(&self.inner);
        let camera = lock(&self.inner.voice_settings).camera.clone();
        self.inner.runtime.spawn(async move {
            let spec = if on {
                match tokio::task::spawn_blocking(move || {
                    crate::features::host::peer::chat_video::camera_spec(camera.as_deref())
                })
                .await
                {
                    Ok(Ok(spec)) => Some(spec),
                    _ => {
                        update(&inner, |s| s.errors.push(crate::i18n::t().voice.no_camera.to_owned()));
                        return;
                    }
                }
            } else {
                None
            };
            let result = v.set_camera(spec).await;
            update(&inner, |s| {
                if let Some(my) = &mut s.my_voice {
                    my.camera = v.camera_on();
                }
                if result.is_err() {
                    s.errors.push(crate::i18n::t().voice.failed.to_owned());
                }
            });
        });
    }

    /// Shares `what` at `quality`, with its sound or not; `None`: stops
    /// sharing.
    pub(crate) fn share_screen(&self, share: Option<(ShareTarget, crate::features::host::peer::chat_video::Quality, bool)>) {
        use crate::features::host::peer::chat_video::Shown;
        let Some(v) = lock(&self.inner.voice).clone() else { return };
        let inner = Arc::clone(&self.inner);
        self.inner.runtime.spawn(async move {
            let share = match share {
                Some((ShareTarget::Screen(id), quality, audio)) => {
                    let screens = crate::platform::windows::capture::screens().unwrap_or_default();
                    let screen = screens
                        .iter()
                        .find(|s| Some(s.id) == id)
                        .or_else(|| screens.iter().find(|s| s.primary))
                        .or(screens.first())
                        .cloned();
                    let Some(screen) = screen else {
                        update(&inner, |s| s.errors.push(crate::i18n::t().voice.failed.to_owned()));
                        return;
                    };
                    Some((Shown::Screen(screen), quality, audio))
                }
                Some((ShareTarget::Window { hwnd, pid }, quality, audio)) => {
                    Some((Shown::Window { hwnd, pid }, quality, audio))
                }
                None => None,
            };
            let sharing = share.is_some();
            let result = v.set_screen(share).await;
            update(&inner, |s| {
                if let Some(my) = &mut s.my_voice {
                    my.screen = if result.is_ok() { v.screen() } else { None };
                }
                if result.is_err() {
                    s.errors.push(crate::i18n::t().voice.failed.to_owned());
                }
            });
            if result.is_err() || !sharing || v.following().swap(true, std::sync::atomic::Ordering::AcqRel) {
                return;
            }
            // The share's own changes: a minimized window, a closed one.
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let going = v.follow_share().await;
                update(&inner, |s| {
                    if let Some(my) = &mut s.my_voice {
                        my.screen = v.screen();
                    }
                });
                if !going {
                    break;
                }
            }
            v.following().store(false, std::sync::atomic::Ordering::Release);
        });
    }

    /// The cameras and screens to receive now: (participant, kind).
    pub(crate) fn watch(&self, tracks: Vec<(u32, String)>) {
        let Some(Active::Channel(v)) = lock(&self.inner.voice).clone() else { return };
        self.inner.runtime.spawn(async move { v.watch(tracks).await });
    }

    /// The newest picture of `participant`'s `kind`, if watched.
    pub(crate) fn picture(&self, participant: u32, kind: &str) -> Option<crate::features::voice::video::Picture> {
        lock(&self.inner.voice).as_ref()?.pictures().picture(participant, kind)
    }

    /// This device's own camera (`cam_hi`) or share (`screen`) as it
    /// sends it.
    pub(crate) fn my_picture(&self, kind: &str) -> Option<crate::features::voice::video::Picture> {
        lock(&self.inner.voice).clone()?.own_picture(kind)
    }

    /// What each watched picture brought (`dev voice`).
    pub(crate) fn picture_stats(&self) -> Vec<crate::features::voice::video::PictureStats> {
        lock(&self.inner.voice).as_ref().map(|v| v.pictures().stats()).unwrap_or_default()
    }

    /// Asks `sharer_device` (sharing its whole screen) for control.
    pub(crate) fn request_control(&self, sharer_device: &str) {
        let Some(Active::Channel(v)) = lock(&self.inner.voice).clone() else { return };
        let (inner, sharer) = (Arc::clone(&self.inner), sharer_device.to_owned());
        self.inner.runtime.spawn(async move {
            let asked = inner
                .client
                .chat(
                    Method::POST,
                    &format!("/api/v2/chat/channels/{}/voice/control-request", v.channel),
                    Some(serde_json::json!({"sharer_device": sharer})),
                )
                .await;
            update(&inner, |s| match asked {
                Ok(answer) => {
                    s.control = Some(crate::features::chat::state::ControlAsked {
                        id: answer["id"].as_str().unwrap_or_default().to_owned(),
                        sharer_device: sharer,
                        allowed: false,
                    });
                }
                Err(_) => s.errors.push(crate::i18n::t().voice.failed.to_owned()),
            });
        });
    }

    /// Gives back (or stops asking for) the control of a share.
    pub(crate) fn end_control(&self) {
        let Some(Active::Channel(v)) = lock(&self.inner.voice).clone() else { return };
        let mut id = None;
        update(&self.inner, |s| id = s.control.take().map(|c| c.id));
        let Some(id) = id else { return };
        self.close_control(v.channel, id);
    }

    /// Takes back the control this device gave.
    pub(crate) fn take_back_control(&self) {
        let Some(Active::Channel(v)) = lock(&self.inner.voice).clone() else { return };
        let mut id = None;
        update(&self.inner, |s| id = s.control_given.take().map(|c| c.id));
        let Some(id) = id else { return };
        self.close_control(v.channel, id);
    }

    fn close_control(&self, channel: Id, id: String) {
        let inner = Arc::clone(&self.inner);
        self.inner.runtime.spawn(async move {
            let _ = inner
                .client
                .chat(Method::DELETE, &format!("/api/v2/chat/channels/{channel}/voice/control-request/{id}"), None)
                .await;
            update(&inner, |_| {});
        });
    }

    /// Answers a request to control this device's share.
    pub(crate) fn answer_control(&self, id: &str, allowed: bool, clipboard: bool) {
        let Some(Active::Channel(v)) = lock(&self.inner.voice).clone() else { return };
        let inner = Arc::clone(&self.inner);
        let id = id.to_owned();
        self.inner.runtime.spawn(async move {
            let answered = inner
                .client
                .chat(
                    Method::POST,
                    &format!("/api/v2/chat/channels/{}/voice/control-request/{id}/answer", v.channel),
                    Some(serde_json::json!({"allowed": allowed, "clipboard": clipboard})),
                )
                .await;
            update(&inner, |s| {
                let request = s.control_asks.iter().position(|r| r.id == id).map(|i| s.control_asks.remove(i));
                match answered {
                    Ok(_) if allowed => s.control_given = request,
                    Ok(_) => {}
                    Err(_) => s.errors.push(crate::i18n::t().voice.failed.to_owned()),
                }
            });
        });
    }

    /// The device to open a session to (a share's control), once.
    pub(crate) fn take_open_device(&self) -> Option<String> {
        if lock(&self.inner.state).open_device.is_none() {
            return None;
        }
        let mut device = None;
        update(&self.inner, |s| device = s.open_device.take());
        device
    }

    /// A moderator's change to `participant` in `channel`: `server_muted`,
    /// `server_deafened` or `move_to`.
    pub(crate) fn moderate_voice(&self, channel: Id, participant: u32, change: Value) {
        self.fire(
            Method::PATCH,
            format!("/api/v2/chat/channels/{channel}/voice/participants/{participant}"),
            Some(change),
        );
    }

    pub(crate) fn disconnect_voice(&self, channel: Id, participant: u32) {
        self.fire(
            Method::DELETE,
            format!("/api/v2/chat/channels/{channel}/voice/participants/{participant}"),
            None,
        );
    }

    // ------------------------------------------------------------- calls

    /// Calls the other side of direct message `channel`.
    pub(crate) fn start_call(&self, channel: Id) {
        let inner = Arc::clone(&self.inner);
        self.inner.runtime.spawn(async move {
            let old = lock(&inner.voice).take();
            if let Some(old) = old {
                old.close(true).await;
            }
            let result = async {
                let value = inner
                    .client
                    .chat(Method::POST, &format!("/api/v2/chat/dm/{channel}/call"), Some(serde_json::json!({})))
                    .await?;
                let call: Call = serde_json::from_value(value)?;
                Ok::<_, anyhow::Error>(call)
            }
            .await;
            match result {
                Ok(call) => begin_call(&inner, call),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "call not started");
                    update(&inner, |s| s.errors.push(crate::i18n::t().voice.failed.to_owned()));
                }
            }
        });
    }

    /// Answers call `id` on this device (`take`: a call of this account
    /// on another device moves here).
    pub(crate) fn answer_call(&self, id: &str, take: bool) {
        let inner = Arc::clone(&self.inner);
        let id = id.to_owned();
        self.inner.runtime.spawn(async move {
            let old = lock(&inner.voice).take();
            if let Some(old) = old {
                old.close(true).await;
            }
            let action = if take { "move" } else { "answer" };
            match inner.client.chat(Method::POST, &format!("/api/v2/chat/calls/{id}/{action}"), None).await {
                Ok(value) => {
                    if let Ok(call) = serde_json::from_value::<Call>(value) {
                        begin_call(&inner, call);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "call not answered");
                    update(&inner, |s| s.errors.push(crate::i18n::t().voice.failed.to_owned()));
                }
            }
        });
    }

    pub(crate) fn decline_call(&self, id: &str) {
        self.fire(Method::POST, format!("/api/v2/chat/calls/{id}/decline"), None);
    }
}

fn play_sound(inner: &Arc<Inner>, sound: Sound) {
    let settings = lock(&inner.voice_settings).clone();
    if let Some(level) = settings.sound_level(sound) {
        sounds::play(sound, settings.output.clone(), level);
    }
}

/// Keeps the settings' microphone: drained, recording the test, closed
/// when no longer shown.
async fn preview_loop(inner: Arc<Inner>) {
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    loop {
        tick.tick().await;
        let mut slot = lock(&inner.preview);
        let Some(p) = slot.as_mut() else { return };
        let gain = f32::from(lock(&inner.voice_settings).input_volume) / 100.0;
        let mut done = None;
        if let Some((at, buffer)) = p.recording.as_mut() {
            while let Some(block) = p.capture.blocks.pop() {
                buffer.extend(block.iter().map(|s| (s * gain).clamp(-1.0, 1.0)));
            }
            if at.elapsed() >= TEST {
                done = Some(std::mem::take(buffer));
            }
        } else {
            while p.capture.blocks.pop().is_some() {}
        }
        if let Some(samples) = done {
            p.recording = None;
            p.playing = Some(Instant::now() + Duration::from_millis(samples.len() as u64 / 48) + Duration::from_millis(200));
            p.polled = Instant::now();
            let settings = lock(&inner.voice_settings).clone();
            sounds::play_recording(samples, settings.output.clone(), f32::from(settings.volume) / 100.0);
        }
        let busy = p.recording.is_some() || p.playing.is_some_and(|t| Instant::now() < t);
        if !busy && p.polled.elapsed() > PREVIEW_KEEP || inner.stop.is_cancelled() {
            *slot = None;
            return;
        }
    }
}

/// This device takes part in `call` (calling, or answered).
fn begin_call(inner: &Arc<Inner>, call: Call) {
    let settings = lock(&inner.voice_settings).clone();
    let (tx, rx) = mpsc::unbounded_channel();
    let channel = call.channel_id;
    let answered = call.state == "active";
    match CallVoice::new(Arc::clone(&inner.client), inner.outgoing.clone(), call.clone(), settings, tx) {
        Ok(voice) => {
            watch(inner, rx);
            let (muted, deafened) = *lock(&inner.voice_self);
            voice.engine.set_self(muted, deafened);
            update(inner, |s| {
                s.my_voice = Some(MyVoice {
                    channel,
                    call: Some(call.id.clone()),
                    muted,
                    deafened,
                    ..Default::default()
                });
                // As the server answered this device (the others learn by push).
                if let Some(snap) = &mut s.snap {
                    snap.calls.retain(|c| c.id != call.id);
                    snap.calls.push(call.clone());
                }
            });
            *lock(&inner.voice) = Some(Active::Call(Arc::clone(&voice)));
            toggles(inner, voice.engine.clone());
            if answered {
                // The callee's side waits for the caller's offer.
                let v = Arc::clone(&voice);
                inner.runtime.spawn(async move { v.answered(call).await });
            }
        }
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "call media not started"),
    }
}

/// A `voice.*` or `call.*` push for the session this device is in.
pub(super) fn push(inner: &Arc<Inner>, kind: &str, data: &Value) {
    let active = lock(&inner.voice).clone();
    if active.is_none() && kind.starts_with("voice.") {
        let mut joining = lock(&inner.voice_joining);
        if let Some((channel, waiting)) = joining.as_mut()
            && data["channel_id"].as_str() == Some(channel.to_string().as_str())
        {
            waiting.push((kind.to_owned(), data.clone()));
        }
        return;
    }
    match (kind, active) {
        (types::VOICE_OFFER, Some(Active::Channel(v))) => {
            if let Ok(offer) = serde_json::from_value::<VoiceOffer>(data.clone())
                && offer.channel_id == v.channel
            {
                inner.runtime.spawn(async move { v.offer(offer).await });
            }
        }
        (types::CONTROL_REQUEST, Some(Active::Channel(v))) => {
            if let Ok(request) = serde_json::from_value::<farwing_protocol::chat::ControlRequest>(data.clone())
                && request.channel_id == v.channel
            {
                update(inner, |s| {
                    s.control_asks.retain(|r| r.id != request.id);
                    s.control_asks.push(request);
                });
            }
        }
        (types::CONTROL_ANSWERED, Some(Active::Channel(_))) => {
            if let Ok(answer) = serde_json::from_value::<farwing_protocol::chat::ControlAnswered>(data.clone()) {
                update(inner, |s| {
                    if s.control.as_ref().is_none_or(|c| c.id != answer.id) {
                        return;
                    }
                    if answer.allowed {
                        if let Some(c) = &mut s.control {
                            c.allowed = true;
                        }
                        // A remote desktop session to the sharer, under the share's grant.
                        s.open_device = Some(answer.sharer_device.clone());
                    } else {
                        s.control = None;
                        s.errors.push(crate::i18n::t().voice.control_declined.to_owned());
                    }
                });
            }
        }
        (types::CONTROL_ENDED, Some(Active::Channel(_))) => {
            if let Ok(ended) = serde_json::from_value::<farwing_protocol::chat::ControlEnded>(data.clone()) {
                update(inner, |s| {
                    s.control_asks.retain(|r| r.id != ended.id);
                    if s.control_given.as_ref().is_some_and(|r| r.id == ended.id) {
                        s.control_given = None;
                    }
                    if s.control.as_ref().is_some_and(|c| c.id == ended.id) {
                        s.control = None;
                        s.errors.push(crate::i18n::t().voice.control_ended.to_owned());
                    }
                });
            }
        }
        (types::VOICE_DEMAND, Some(Active::Channel(v))) => {
            if let Ok(demand) = serde_json::from_value::<farwing_protocol::chat::VoiceDemand>(data.clone())
                && demand.channel_id == v.channel
            {
                v.demand(&demand.kinds);
            }
        }
        (types::VOICE_KEYS, Some(Active::Channel(v))) => {
            if let Ok(push) = serde_json::from_value::<VoiceKeyPush>(data.clone())
                && push.channel_id == v.channel
            {
                v.key(&push);
            }
        }
        (types::VOICE_STATE, Some(Active::Channel(v))) => {
            if let Ok(state) = serde_json::from_value::<farwing_protocol::chat::VoiceChannelState>(data.clone())
                && state.channel_id == v.channel
            {
                inner.runtime.spawn(async move { v.state(state.participants).await });
            }
        }
        (types::VOICE_MOVED, Some(Active::Channel(v))) => {
            if let Ok(moved) = serde_json::from_value::<VoiceMoved>(data.clone())
                && moved.channel_id == v.channel
            {
                lock(&inner.voice).take();
                if moved.to.is_some() {
                    play_sound(inner, Sound::Removed);
                }
                let target = moved.to.unwrap_or(moved.channel_id);
                let inner = Arc::clone(inner);
                let runtime = inner.runtime.clone();
                runtime.spawn(async move {
                    v.close(false).await;
                    ChatService { inner }.join_voice(target);
                });
            }
        }
        (types::VOICE_LEFT, Some(Active::Channel(v))) => {
            if let Ok(left) = serde_json::from_value::<VoiceLeft>(data.clone())
                && left.channel_id == v.channel
            {
                lock(&inner.voice).take();
                if left.reason == "disconnected" {
                    play_sound(inner, Sound::Removed);
                }
                let why = reason_text(&left.reason);
                update(inner, |s| {
                    s.end_voice();
                    s.errors.push(why.to_owned());
                });
                inner.runtime.spawn(async move { v.close(false).await });
            }
        }
        (types::CALL_ANSWERED, Some(Active::Call(c))) => {
            if let Ok(call) = serde_json::from_value::<Call>(data.clone())
                && call.id == c.id()
            {
                inner.runtime.spawn(async move { c.answered(call).await });
            }
        }
        (types::CALL_SIGNAL, Some(Active::Call(c))) => {
            if let Ok(signal) = serde_json::from_value::<CallSignal>(data.clone())
                && signal.call_id == c.id()
            {
                inner.runtime.spawn(async move { c.signal(signal).await });
            }
        }
        (types::CALL_ENDED, Some(Active::Call(c))) => {
            if let Ok(ended) = serde_json::from_value::<CallEnded>(data.clone())
                && ended.call_id == c.id()
            {
                lock(&inner.voice).take();
                update(inner, |s| s.end_voice());
                inner.runtime.spawn(async move { c.close().await });
            }
        }
        _ => {}
    }
}
