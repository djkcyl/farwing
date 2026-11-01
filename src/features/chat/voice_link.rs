//! The chat service's voice (`docs/protocol/chat-design.md` §7): joining
//! and leaving channels, calls, and the `voice.*` / `call.*` pushes for the
//! session this device is in.

use super::{ChatService, Inner, lock, update};
use crate::features::chat::state::MyVoice;
use crate::features::voice::keys::DeviceKeys;
use crate::features::voice::session::{CallVoice, ChannelVoice, Event};
use crate::features::voice::settings::{self, VoiceSettings};
use farwing_protocol::chat::{Call, CallEnded, CallSignal, Id, VoiceKeyPush, VoiceLeft, VoiceMode, VoiceOffer, types};
use reqwest::Method;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;

/// What the voice sounds last saw.
#[derive(Default)]
pub(super) struct SoundState {
    connected: bool,
    channel: Option<Id>,
    people: Vec<u32>,
    muted: bool,
    deafened: bool,
    rang: Option<std::time::Instant>,
}

#[derive(Clone)]
pub(super) enum Active {
    Channel(Arc<ChannelVoice>),
    Call(Arc<CallVoice>),
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
    for r in ["voice_full", "voice_no_forwarding", "chat_forbidden"] {
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
        loop {
            tick.tick().await;
            if inner.stop.is_cancelled() || lock(&inner.voice).is_none() {
                return;
            }
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

    /// Plays the sounds the state's changes call for (each frame).
    pub(crate) fn tick_sounds(&self, view: &crate::features::chat::state::ChatState) {
        use crate::features::voice::sounds::{Sound, play};
        let settings = lock(&self.inner.voice_settings).clone();
        let mut last = lock(&self.inner.sounds);
        let mut sounds = Vec::new();
        let me = view.me().map(|u| u.user_id.clone()).unwrap_or_default();
        let dnd = view.snap.as_ref().is_some_and(|s| s.status == farwing_protocol::chat::Status::Dnd);
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
        let people: Vec<u32> = channel
            .and_then(|c| view.snap.as_ref()?.voice.iter().find(|v| v.channel_id == c))
            .map(|v| v.participants.iter().map(|p| p.participant).collect())
            .unwrap_or_default();
        if connected && channel == last.channel {
            let mine = voice.map(|v| v.participant).unwrap_or(0);
            if people.iter().any(|p| *p != mine && !last.people.contains(p)) {
                sounds.push(Sound::Came);
            } else if last.people.iter().any(|p| *p != mine && !people.contains(p)) {
                sounds.push(Sound::Went);
            }
        }
        let (muted, deafened) = voice.map_or((last.muted, last.deafened), |v| (v.muted, v.deafened));
        if voice.is_some() && (muted != last.muted || deafened != last.deafened) {
            sounds.push(if muted || deafened { Sound::Muted } else { Sound::Unmuted });
        }
        *last = SoundState {
            connected,
            channel,
            people,
            muted,
            deafened,
            rang: last.rang,
        };
        drop(last);
        if settings.sounds {
            for s in sounds {
                play(s, settings.output.clone(), f32::from(settings.volume) / 100.0);
            }
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
        let (muted, deafened) = self
            .view()
            .my_voice
            .as_ref()
            .map_or((false, false), |v| (v.muted, v.deafened));
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
                        s.my_voice = None;
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
            update(&inner, |s| s.my_voice = None);
            if let Some(a) = active {
                a.close(true).await;
            }
        });
    }

    pub(crate) fn set_voice_self(&self, muted: bool, deafened: bool) {
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

    /// A moderator's mute of `participant` in `channel`.
    pub(crate) fn moderate_voice(&self, channel: Id, participant: u32, muted: bool) {
        self.fire(
            Method::PATCH,
            format!("/api/v2/chat/channels/{channel}/voice/participants/{participant}"),
            Some(serde_json::json!({"server_muted": muted})),
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

/// This device takes part in `call` (calling, or answered).
fn begin_call(inner: &Arc<Inner>, call: Call) {
    let settings = lock(&inner.voice_settings).clone();
    let (tx, rx) = mpsc::unbounded_channel();
    let channel = call.channel_id;
    let answered = call.state == "active";
    match CallVoice::new(Arc::clone(&inner.client), inner.outgoing.clone(), call.clone(), settings, tx) {
        Ok(voice) => {
            watch(inner, rx);
            update(inner, |s| {
                s.my_voice = Some(MyVoice {
                    channel,
                    call: Some(call.id.clone()),
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
        (types::VOICE_LEFT, Some(Active::Channel(v))) => {
            if let Ok(left) = serde_json::from_value::<VoiceLeft>(data.clone())
                && left.channel_id == v.channel
            {
                lock(&inner.voice).take();
                let why = reason_text(&left.reason);
                update(inner, |s| {
                    s.my_voice = None;
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
                update(inner, |s| s.my_voice = None);
                inner.runtime.spawn(async move { c.close().await });
            }
        }
        _ => {}
    }
}
