//! Voice channels on the control node (`docs/protocol/chat-design.md` §7):
//! who is in which channel, joining and leaving, the forwarder's offers to
//! each device and frame keys between devices. The media itself is the
//! forwarder's ([`crate::sfu::VoiceOp`]), here or on an edge node.

use crate::egress::Payer;
use crate::error::{ApiError, ApiResult};
use crate::http::App;
use crate::sfu::{SfuRequest, VoiceOp, VoiceResult};
use farwing_protocol::ErrorCode;
use farwing_protocol::chat::{
    ChannelKind, ControlAnswer, ControlAnswered, ControlEnded, ControlRequest, Id, VoiceChannelState, VoiceDemand,
    VoiceJoin, VoiceJoined, VoiceKeyPush, VoiceKeys, VoiceLeft, VoiceMode, VoiceModerate, VoiceMoved, VoiceOffer,
    VoiceParticipant, VoiceSelf, VoiceSubscribe, VoiceTrack, perm, track, types,
};
use farwing_protocol::error::reason;
use farwing_protocol::realtime::Envelope;
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A device whose chat connection is gone this long leaves (§7.1).
const CHAT_GRACE: Duration = Duration::from_secs(30);
/// Or whose media connection carried nothing this long.
const MEDIA_IDLE_MS: u64 = 20_000;
/// A channel whose node failed this many sweeps in a row moves (§7.2).
const NODE_FAILURES: u32 = 2;
/// Key boxes per request, and the size of each field.
const MAX_BOXES: usize = 128;
const MAX_FIELD: usize = 1024;

#[derive(Clone)]
struct Part {
    info: VoiceParticipant,
    /// Its egress (and relay credentials) are this session's.
    session: String,
    gone_since: Option<Instant>,
    /// The others' tracks it chose (besides every `mic`).
    subs: Vec<(u32, String)>,
    /// What it was last told the others watch of what it does not send.
    demand: Vec<String>,
}

/// Control of a screen share asked for, and given (§8.3).
#[derive(Clone)]
struct Control {
    channel: i64,
    requester_user: String,
    requester_device: String,
    sharer_device: String,
    /// Answered yes (with the clipboard or not).
    allowed: Option<bool>,
    clipboard: bool,
}

struct Chan {
    room: String,
    /// An edge node's forwarder; `None`: this server's.
    node: Option<(String, String)>,
    mode: VoiceMode,
    parts: BTreeMap<u32, Part>,
    /// Sweeps in a row the node did not answer.
    failures: u32,
}

#[derive(Default)]
struct State {
    chans: HashMap<i64, Chan>,
    by_device: HashMap<String, i64>,
    next: u32,
    controls: HashMap<String, Control>,
}

#[derive(Default)]
pub struct Voice {
    st: Mutex<State>,
    /// One operation at a time per channel.
    locks: Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
}

/// Nothing `info` says is forwarded.
fn silenced(info: &VoiceParticipant) -> bool {
    info.server_muted || info.server_deafened || info.cannot_speak
}

fn not_in() -> ApiError {
    ApiError::reason(ErrorCode::NotFound, reason::VOICE_NOT_IN, "not in this voice channel")
}

fn unavailable(detail: impl std::fmt::Display) -> ApiError {
    tracing::warn!(%detail, "voice forwarder failed");
    ApiError::reason(ErrorCode::Unavailable, reason::FORWARDING_UNAVAILABLE, "the forwarder is unavailable")
}

impl Voice {
    fn lock(&self, channel: i64) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(self.locks.lock().entry(channel).or_default())
    }

    fn state(&self, channel: i64) -> Option<VoiceChannelState> {
        let st = self.st.lock();
        let chan = st.chans.get(&channel)?;
        Some(VoiceChannelState {
            channel_id: Id(channel),
            participants: chan.parts.values().map(|p| p.info.clone()).collect(),
        })
    }

    /// The channels among `visible` with someone in them.
    pub fn states(&self, visible: &[i64]) -> Vec<VoiceChannelState> {
        let st = self.st.lock();
        let mut out: Vec<VoiceChannelState> = st
            .chans
            .iter()
            .filter(|(id, c)| visible.contains(id) && !c.parts.is_empty())
            .map(|(id, c)| VoiceChannelState {
                channel_id: Id(*id),
                participants: c.parts.values().map(|p| p.info.clone()).collect(),
            })
            .collect();
        out.sort_by_key(|s| s.channel_id);
        out
    }

    /// The channel `device` is in.
    pub fn channel_of(&self, device: &str) -> Option<i64> {
        self.st.lock().by_device.get(device).copied()
    }

    fn part(&self, channel: i64, device: &str) -> Option<(Part, String, Option<(String, String)>)> {
        let st = self.st.lock();
        let chan = st.chans.get(&channel)?;
        let part = chan.parts.values().find(|p| p.info.device_id == device)?.clone();
        Some((part, chan.room.clone(), chan.node.clone()))
    }

    /// What a screen share's control gives `device` of account `user` on
    /// `host`: the capabilities, if its sharer said yes.
    pub fn share_grant(&self, user: &str, device: &str, host: &str) -> Option<Vec<String>> {
        let st = self.st.lock();
        st.controls
            .values()
            .find(|c| {
                c.allowed == Some(true) && c.requester_user == user && c.requester_device == device && c.sharer_device == host
            })
            .map(|c| {
                let mut caps = vec![
                    farwing_protocol::caps::VIEW.to_owned(),
                    farwing_protocol::caps::CONTROL.to_owned(),
                    farwing_protocol::caps::ANNOTATION.to_owned(),
                ];
                if c.clipboard {
                    caps.push(farwing_protocol::caps::CLIPBOARD.to_owned());
                }
                caps
            })
    }

    /// The device's chat connection ended (`false`) or came back.
    pub fn chat_connection(&self, device: &str, up: bool) {
        let mut st = self.st.lock();
        let Some(channel) = st.by_device.get(device).copied() else { return };
        if let Some(part) = st
            .chans
            .get_mut(&channel)
            .and_then(|c| c.parts.values_mut().find(|p| p.info.device_id == device))
        {
            part.gone_since = if up { None } else { Some(Instant::now()) };
        }
    }
}

async fn run(app: &App, node: Option<&(String, String)>, op: VoiceOp) -> ApiResult<VoiceResult> {
    let request = SfuRequest::Voice(op);
    let text = match node {
        Some((id, _)) => app.edges.sfu(id, request).await.map_err(unavailable)?,
        None => app.forwarding.local.handle(request).await.map_err(unavailable)?,
    };
    serde_json::from_str(&text).map_err(unavailable)
}

fn push_offer(app: &App, device: &str, channel: i64, offer: Option<(u64, String)>) {
    if let Some((generation, sdp)) = offer {
        app.chat.hub.send_device(
            device,
            &Envelope::new(
                types::VOICE_OFFER,
                VoiceOffer {
                    channel_id: Id(channel),
                    generation,
                    sdp,
                },
            ),
        );
    }
}

/// Everyone's downlink (but `except`'s) follows what is published now.
async fn sync_all(app: &App, channel: i64, except: Option<u32>) {
    let (room, node, parts) = {
        let st = app.chat.voice.st.lock();
        let Some(chan) = st.chans.get(&channel) else { return };
        let parts: Vec<Part> = chan
            .parts
            .values()
            .filter(|p| Some(p.info.participant) != except)
            .cloned()
            .collect();
        (chan.room.clone(), chan.node.clone(), parts)
    };
    for part in parts {
        let op = VoiceOp::Sync {
            room: room.clone(),
            participant: part.info.participant,
            session: part.session.clone(),
        };
        match run(app, node.as_ref(), op).await {
            Ok(result) => push_offer(app, &part.info.device_id, channel, result.offer),
            Err(_) => tracing::warn!(channel, participant = part.info.participant, "voice downlink not updated"),
        }
    }
}

async fn push_state(app: &App, channel: i64) {
    let state = app.chat.voice.state(channel).unwrap_or(VoiceChannelState {
        channel_id: Id(channel),
        participants: Vec::new(),
    });
    super::push::to_channel(app, channel, &Envelope::new(types::VOICE_STATE, state), None).await;
}

/// `POST /chat/channels/{id}/voice/join`
pub async fn join(app: &App, user: &str, device: &crate::db::DeviceRow, channel: i64, req: VoiceJoin) -> ApiResult<VoiceJoined> {
    let cfg = app.cfg();
    if !cfg.sfu.enabled {
        return Err(ApiError::reason(ErrorCode::Forbidden, reason::FORWARDING_DISABLED, "forwarding is off"));
    }
    let (u, d) = (user.to_owned(), device.id.clone());
    let found = app
        .db
        .call(move |c| {
            let Some(a) = super::access(c, &u, channel)? else {
                return Ok(None);
            };
            let frozen = a.ctx.as_ref().is_some_and(|g| g.group.frozen_at.is_some());
            Ok(Some((a.channel, a.perms, frozen, crate::db::device_x25519(c, &d)?)))
        })
        .await?;
    let Some((ch, perms, frozen, x25519)) = found else {
        return Err(crate::http::chat::not_found());
    };
    if ch.kind != ChannelKind::Voice {
        return Err(ApiError::bad_request("not a voice channel"));
    }
    if frozen {
        return Err(crate::http::chat::frozen());
    }
    if perms & perm::CONNECT == 0 {
        return Err(crate::http::chat::forbidden());
    }
    let mode = match cfg.e2ee {
        farwing_protocol::api::E2ee::Required => VoiceMode::E2ee,
        farwing_protocol::api::E2ee::Off => VoiceMode::Compatible,
        farwing_protocol::api::E2ee::Allowed if ch.voice_compat => VoiceMode::Compatible,
        farwing_protocol::api::E2ee::Allowed => VoiceMode::E2ee,
    };
    if mode == VoiceMode::E2ee && x25519.is_empty() {
        return Err(ApiError::reason(ErrorCode::BadRequest, reason::VOICE_NO_KEY, "the device has no X25519 key"));
    }
    // Elsewhere first: a device is in one channel at a time.
    if let Some(other) = app.chat.voice.channel_of(&device.id) {
        if other == channel {
            leave(app, &device.id, channel, None).await;
        } else {
            leave(app, &device.id, other, Some("replaced")).await;
        }
    }
    let lock = app.chat.voice.lock(channel);
    let _held = lock.lock().await;
    let full = {
        let st = app.chat.voice.st.lock();
        let count = st.chans.get(&channel).map_or(0, |c| c.parts.len()) as u32;
        ch.voice_limit > 0 && count >= ch.voice_limit && perms & perm::PRIORITY == 0
    };
    if full {
        return Err(ApiError::reason(ErrorCode::Forbidden, reason::VOICE_FULL, "the voice channel is full"));
    }
    let participant = {
        let mut st = app.chat.voice.st.lock();
        st.next = st.next.wrapping_add(1).max(1);
        st.next
    };
    let session = format!("voice-{channel}-{participant}");
    app.egress.ledger.open(&session, Payer::User(user.to_owned()));
    let access = app.egress.access(&session, true);
    if !access.forward {
        app.egress.ledger.close(&session);
        return Err(ApiError::reason(
            ErrorCode::Forbidden,
            reason::VOICE_NO_FORWARDING,
            "the account may not use forwarding",
        ));
    }
    if access.used_up {
        app.egress.ledger.close(&session);
        return Err(ApiError::reason(ErrorCode::Forbidden, reason::VOICE_QUOTA, "the month's traffic is used up"));
    }
    app.egress.apply(&session, &access);
    let ice_servers = app.hub.relay_servers(&session, &device.id, &access);
    let info = VoiceParticipant {
        participant,
        user_id: user.to_owned(),
        device_id: device.id.clone(),
        ed25519: device.public_key.clone(),
        x25519,
        self_muted: req.self_muted,
        self_deafened: req.self_deafened,
        server_muted: false,
        server_deafened: false,
        cannot_speak: false,
        tracks: Vec::new(),
        screen: None,
        joined_at: crate::db::now_ms(),
    };
    let participants = {
        let mut st = app.chat.voice.st.lock();
        let chan = st.chans.entry(channel).or_insert_with(|| Chan {
            room: format!("voice-{channel}"),
            node: (app.egress.local_full())
                .then(|| app.edges.forwarder_for(&device.id))
                .flatten(),
            mode,
            parts: BTreeMap::new(),
            failures: 0,
        });
        chan.parts.insert(
            participant,
            Part {
                info,
                session: session.clone(),
                gone_since: None,
                subs: Vec::new(),
                demand: Vec::new(),
            },
        );
        let parts: Vec<VoiceParticipant> = chan.parts.values().map(|p| p.info.clone()).collect();
        let mode = chan.mode;
        st.by_device.insert(device.id.clone(), channel);
        (parts, mode)
    };
    let (participants, mode) = participants;
    drop(_held);
    push_state(app, channel).await;
    // What the others publish already.
    {
        let lock = app.chat.voice.lock(channel);
        let _held = lock.lock().await;
        if let Some((part, room, node)) = app.chat.voice.part(channel, &device.id) {
            let op = VoiceOp::Sync {
                room,
                participant,
                session: part.session.clone(),
            };
            if let Ok(result) = run(app, node.as_ref(), op).await {
                // After the answer to the join reaches the device.
                let (app, device, offer) = (app.clone(), device.id.clone(), result.offer);
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    push_offer(&app, &device, channel, offer);
                });
            }
        }
    }
    Ok(VoiceJoined {
        channel_id: Id(channel),
        participant,
        mode,
        bitrate_kbps: ch.voice_bitrate.min(cfg.chat.voice_max_kbps),
        ice_servers,
        participants,
    })
}

/// `PUT …/voice/publish`
pub async fn publish(
    app: &App,
    user: &str,
    device: &str,
    channel: i64,
    sdp: String,
    tracks: Vec<VoiceTrack>,
) -> ApiResult<String> {
    if sdp.is_empty() || sdp.len() > 64 * 1024 || tracks.len() > track::ALL.len() {
        return Err(ApiError::bad_request("invalid description"));
    }
    if tracks.iter().any(|t| !track::ALL.contains(&t.kind.as_str()) || t.mid.is_empty() || t.mid.len() > 32) {
        return Err(ApiError::bad_request("invalid tracks"));
    }
    let video = tracks.iter().any(|t| t.kind != track::MIC);
    let u = user.to_owned();
    let allowed = app
        .db
        .call(move |c| {
            Ok(super::access(c, &u, channel)?.is_some_and(|a| a.can(perm::SPEAK) && (!video || a.can(perm::VIDEO))))
        })
        .await?;
    if !allowed {
        return Err(crate::http::chat::forbidden());
    }
    let lock = app.chat.voice.lock(channel);
    let held = lock.lock().await;
    let (part, room, node) = app.chat.voice.part(channel, device).ok_or_else(not_in)?;
    let participant = part.info.participant;
    let mut kinds: Vec<String> = if tracks.is_empty() {
        vec![track::MIC.to_owned()]
    } else {
        tracks.iter().map(|t| t.kind.clone()).collect()
    };
    kinds.sort();
    kinds.dedup();
    let result = run(
        app,
        node.as_ref(),
        VoiceOp::Publish {
            room,
            participant,
            sdp,
            kinds: tracks.into_iter().map(|t| (t.mid, t.kind)).collect(),
        },
    )
    .await?;
    {
        let mut st = app.chat.voice.st.lock();
        if let Some(p) = st.chans.get_mut(&channel).and_then(|c| c.parts.get_mut(&participant)) {
            p.info.tracks = kinds;
        }
    }
    let (now, room, node) = app.chat.voice.part(channel, device).ok_or_else(not_in)?;
    if silenced(&now.info) {
        run(app, node.as_ref(), VoiceOp::Mute { room: room.clone(), participant, muted: true }).await?;
    }
    if now.info.server_deafened {
        run(app, node.as_ref(), VoiceOp::Deafen { room, participant, deafened: true }).await?;
    }
    drop(held);
    end_lost_controls(app, channel).await;
    push_state(app, channel).await;
    let app2 = app.clone();
    tokio::spawn(async move {
        let lock = app2.chat.voice.lock(channel);
        let _held = lock.lock().await;
        sync_all(&app2, channel, Some(participant)).await;
    });
    Ok(result.answer)
}

/// `PUT …/voice/subscribe`: the others' tracks this device watches.
pub async fn subscribe(app: &App, device: &str, channel: i64, req: VoiceSubscribe) -> ApiResult<()> {
    if req.tracks.len() > 64 || req.tracks.iter().any(|t| !track::chosen(&t.kind) || !track::ALL.contains(&t.kind.as_str())) {
        return Err(ApiError::bad_request("invalid tracks"));
    }
    let (part, room, node) = app.chat.voice.part(channel, device).ok_or_else(not_in)?;
    // With this month's traffic used up, the sound goes on but no pictures
    // start (§11).
    if !req.tracks.is_empty() && app.egress.access(&part.session, false).used_up {
        return Err(ApiError::reason(ErrorCode::Forbidden, reason::VOICE_QUOTA, "the month's traffic is used up"));
    }
    let tracks: Vec<(u32, String)> = req.tracks.into_iter().map(|t| (t.participant, t.kind)).collect();
    let lock = app.chat.voice.lock(channel);
    let held = lock.lock().await;
    {
        let mut st = app.chat.voice.st.lock();
        if let Some(p) = st.chans.get_mut(&channel).and_then(|c| c.parts.get_mut(&part.info.participant)) {
            p.subs = tracks.clone();
        }
    }
    let op = VoiceOp::Subscribe {
        room,
        participant: part.info.participant,
        session: part.session.clone(),
        tracks,
    };
    let result = run(app, node.as_ref(), op).await?;
    push_offer(app, device, channel, result.offer);
    drop(held);
    push_demand(app, channel);
    Ok(())
}

/// Each sender learns what the others watch of what it does not send
/// (`screen_720`), when that changed.
fn push_demand(app: &App, channel: i64) {
    let mut out = Vec::new();
    {
        let mut st = app.chat.voice.st.lock();
        let Some(chan) = st.chans.get_mut(&channel) else { return };
        let wanted: Vec<(u32, String)> = chan.parts.values().flat_map(|p| p.subs.clone()).collect();
        for p in chan.parts.values_mut() {
            let mut kinds: Vec<String> = wanted
                .iter()
                .filter(|(who, kind)| *who == p.info.participant && kind == track::SCREEN_720)
                .map(|(_, k)| k.clone())
                .collect();
            kinds.dedup();
            if kinds != p.demand {
                p.demand = kinds.clone();
                out.push((p.info.device_id.clone(), kinds));
            }
        }
    }
    for (device, kinds) in out {
        app.chat.hub.send_device(
            &device,
            &Envelope::new(
                types::VOICE_DEMAND,
                VoiceDemand {
                    channel_id: Id(channel),
                    kinds,
                },
            ),
        );
    }
}

/// Controls given over shares that ended (the sharer stopped sharing its
/// whole screen or left, or the asker left) are taken back.
async fn end_lost_controls(app: &App, channel: i64) {
    let lost: Vec<(String, Control)> = {
        let st = app.chat.voice.st.lock();
        let chan = st.chans.get(&channel);
        let sharing = |device: &str| {
            chan.and_then(|c| c.parts.values().find(|p| p.info.device_id == device)).is_some_and(|p| {
                p.info.tracks.iter().any(|k| k == track::SCREEN) && p.info.screen.as_ref().is_some_and(|s| s.whole)
            })
        };
        let present = |device: &str| chan.is_some_and(|c| c.parts.values().any(|p| p.info.device_id == device));
        st.controls
            .iter()
            .filter(|(_, c)| c.channel == channel && (!sharing(&c.sharer_device) || !present(&c.requester_device)))
            .map(|(id, c)| (id.clone(), c.clone()))
            .collect()
    };
    for (id, c) in lost {
        end_control(app, &id, &c);
    }
}

fn end_control(app: &App, id: &str, c: &Control) {
    app.chat.voice.st.lock().controls.remove(id);
    app.hub.end_shared(&c.sharer_device, &c.requester_device);
    let env = Envelope::new(
        types::CONTROL_ENDED,
        ControlEnded {
            id: id.to_owned(),
            channel_id: Id(c.channel),
        },
    );
    app.chat.hub.send_device(&c.sharer_device, &env);
    app.chat.hub.send_device(&c.requester_device, &env);
}

/// `POST …/voice/control-request {sharer_device}`: asks the device sharing
/// its whole screen to let this one take control.
pub async fn control_request(app: &App, user: &str, device: &str, channel: i64, sharer_device: &str) -> ApiResult<String> {
    let (me, sharer) = {
        let st = app.chat.voice.st.lock();
        let chan = st.chans.get(&channel).ok_or_else(not_in)?;
        let me = chan.parts.values().find(|p| p.info.device_id == device).cloned().ok_or_else(not_in)?;
        let sharer = chan
            .parts
            .values()
            .find(|p| p.info.device_id == sharer_device)
            .cloned()
            .ok_or_else(not_in)?;
        (me, sharer)
    };
    let whole = sharer.info.tracks.iter().any(|k| k == track::SCREEN) && sharer.info.screen.as_ref().is_some_and(|s| s.whole);
    if !whole || sharer.info.device_id == device {
        return Err(ApiError::bad_request("that device does not share its whole screen"));
    }
    let u = user.to_owned();
    let from = app.db.call(move |c| super::store::user(c, &u)).await?;
    let id = uuid::Uuid::new_v4().to_string();
    {
        let mut st = app.chat.voice.st.lock();
        // One request per pair at a time.
        st.controls
            .retain(|_, c| !(c.requester_device == device && c.sharer_device == sharer.info.device_id));
        st.controls.insert(
            id.clone(),
            Control {
                channel,
                requester_user: user.to_owned(),
                requester_device: me.info.device_id.clone(),
                sharer_device: sharer.info.device_id.clone(),
                allowed: None,
                clipboard: false,
            },
        );
    }
    app.chat.hub.send_device(
        &sharer.info.device_id,
        &Envelope::new(
            types::CONTROL_REQUEST,
            ControlRequest {
                id: id.clone(),
                channel_id: Id(channel),
                from,
                from_device: device.to_owned(),
                sharer_device: sharer.info.device_id.clone(),
            },
        ),
    );
    Ok(id)
}

/// `POST …/voice/control-request/{id}/answer` (the sharer).
pub async fn control_answer(app: &App, device: &str, id: &str, answer: ControlAnswer) -> ApiResult<()> {
    let control = {
        let mut st = app.chat.voice.st.lock();
        let c = st
            .controls
            .get_mut(id)
            .filter(|c| c.sharer_device == device && c.allowed.is_none())
            .ok_or_else(not_in)?;
        c.allowed = Some(answer.allowed);
        c.clipboard = answer.clipboard;
        c.clone()
    };
    if !answer.allowed {
        app.chat.voice.st.lock().controls.remove(id);
    }
    app.chat.hub.send_device(
        &control.requester_device,
        &Envelope::new(
            types::CONTROL_ANSWERED,
            ControlAnswered {
                id: id.to_owned(),
                channel_id: Id(control.channel),
                sharer_device: control.sharer_device.clone(),
                allowed: answer.allowed,
                clipboard: answer.clipboard,
            },
        ),
    );
    Ok(())
}

/// `DELETE …/voice/control-request/{id}`: the sharer takes control back,
/// or the asker gives it up.
pub async fn control_end(app: &App, device: &str, id: &str) -> ApiResult<()> {
    let control = app
        .chat
        .voice
        .st
        .lock()
        .controls
        .get(id)
        .filter(|c| c.sharer_device == device || c.requester_device == device)
        .cloned()
        .ok_or_else(not_in)?;
    end_control(app, id, &control);
    Ok(())
}

/// `voice.answer` on the chat connection.
pub async fn answer(app: &App, device: &str, offer: VoiceOffer) {
    let channel = offer.channel_id.0;
    let lock = app.chat.voice.lock(channel);
    let _held = lock.lock().await;
    let Some((part, room, node)) = app.chat.voice.part(channel, device) else { return };
    let op = VoiceOp::Answer {
        room,
        participant: part.info.participant,
        generation: offer.generation,
        sdp: offer.sdp,
    };
    match run(app, node.as_ref(), op).await {
        Ok(result) => push_offer(app, device, channel, result.offer),
        Err(_) => tracing::warn!(channel, device, "voice answer not applied"),
    }
}

/// `PUT …/voice/state`
pub async fn set_self(app: &App, device: &str, channel: i64, req: VoiceSelf) -> ApiResult<()> {
    {
        let mut st = app.chat.voice.st.lock();
        let part = st
            .chans
            .get_mut(&channel)
            .and_then(|c| c.parts.values_mut().find(|p| p.info.device_id == device))
            .ok_or_else(not_in)?;
        part.info.self_muted = req.self_muted;
        part.info.self_deafened = req.self_deafened;
        part.info.screen = req.screen;
    }
    end_lost_controls(app, channel).await;
    push_state(app, channel).await;
    Ok(())
}

/// The group of `channel` when `user` may moderate voice there.
async fn moderator(app: &App, user: &str, channel: i64) -> ApiResult<i64> {
    let u = user.to_owned();
    let group = app
        .db
        .call(move |c| {
            Ok(super::access(c, &u, channel)?
                .filter(|a| a.can(perm::VOICE_MODERATE))
                .and_then(|a| a.group_id()))
        })
        .await?;
    group.ok_or_else(crate::http::chat::forbidden)
}

async fn audit(app: &App, group: i64, actor: &str, action: &'static str, target: String, channel: i64) {
    let actor = actor.to_owned();
    let details = serde_json::json!({ "channel_id": Id(channel) });
    let _ = app
        .db
        .call(move |c| super::store::audit(c, group, &actor, action, &target, &details))
        .await;
}

/// `PATCH …/voice/participants/{participant}` (`voice_moderate`): server
/// mute and deafen, or a move to another voice channel of the group.
pub async fn moderate(app: &App, user: &str, channel: i64, participant: u32, req: VoiceModerate) -> ApiResult<()> {
    let group = moderator(app, user, channel).await?;
    if let Some(to) = req.move_to {
        return move_to(app, user, group, channel, participant, to.0).await;
    }
    let lock = app.chat.voice.lock(channel);
    let held = lock.lock().await;
    let (room, node, info, before) = {
        let mut st = app.chat.voice.st.lock();
        let chan = st.chans.get_mut(&channel).ok_or_else(not_in)?;
        let (room, node) = (chan.room.clone(), chan.node.clone());
        let part = chan.parts.get_mut(&participant).ok_or_else(not_in)?;
        let before = part.info.clone();
        if let Some(m) = req.server_muted {
            part.info.server_muted = m;
        }
        if let Some(d) = req.server_deafened {
            part.info.server_deafened = d;
        }
        (room, node, part.info.clone(), before)
    };
    run(
        app,
        node.as_ref(),
        VoiceOp::Mute {
            room: room.clone(),
            participant,
            muted: silenced(&info),
        },
    )
    .await?;
    run(
        app,
        node.as_ref(),
        VoiceOp::Deafen {
            room,
            participant,
            deafened: info.server_deafened,
        },
    )
    .await?;
    drop(held);
    if before.server_muted != info.server_muted {
        let action = if info.server_muted { "voice_mute" } else { "voice_unmute" };
        audit(app, group, user, action, info.user_id.clone(), channel).await;
    }
    if before.server_deafened != info.server_deafened {
        let action = if info.server_deafened { "voice_deafen" } else { "voice_undeafen" };
        audit(app, group, user, action, info.user_id.clone(), channel).await;
    }
    push_state(app, channel).await;
    Ok(())
}

/// The device of `participant` goes to voice channel `to` of the same group
/// (it joins there itself, `voice.moved`).
async fn move_to(app: &App, user: &str, group: i64, channel: i64, participant: u32, to: i64) -> ApiResult<()> {
    let (device, target_user) = {
        let st = app.chat.voice.st.lock();
        st.chans
            .get(&channel)
            .and_then(|c| c.parts.get(&participant))
            .map(|p| (p.info.device_id.clone(), p.info.user_id.clone()))
            .ok_or_else(not_in)?
    };
    let (u, t) = (user.to_owned(), target_user.clone());
    let fits = app
        .db
        .call(move |c| {
            let mover = super::access(c, &u, to)?;
            let moved = super::access(c, &t, to)?;
            Ok(mover.is_some_and(|a| a.group_id() == Some(group) && a.channel.kind == ChannelKind::Voice)
                && moved.is_some_and(|a| a.can(perm::CONNECT)))
        })
        .await?;
    if !fits || to == channel {
        return Err(ApiError::bad_request("not a voice channel the device can join"));
    }
    app.chat.hub.send_device(
        &device,
        &Envelope::new(
            types::VOICE_MOVED,
            VoiceMoved {
                channel_id: Id(channel),
                to: Some(Id(to)),
            },
        ),
    );
    leave(app, &device, channel, None).await;
    audit(app, group, user, "voice_move", target_user, channel).await;
    Ok(())
}

/// `DELETE …/voice/participants/{participant}` (`voice_moderate`).
pub async fn disconnect(app: &App, user: &str, channel: i64, participant: u32) -> ApiResult<()> {
    let group = moderator(app, user, channel).await?;
    let (device, target) = {
        let st = app.chat.voice.st.lock();
        st.chans
            .get(&channel)
            .and_then(|c| c.parts.get(&participant))
            .map(|p| (p.info.device_id.clone(), p.info.user_id.clone()))
            .ok_or_else(not_in)?
    };
    leave(app, &device, channel, Some("disconnected")).await;
    audit(app, group, user, "voice_disconnect", target, channel).await;
    Ok(())
}

/// `POST …/voice/keys`: the device's frame key for the others in the channel.
pub async fn keys(app: &App, device: &str, channel: i64, req: VoiceKeys) -> ApiResult<()> {
    if req.boxes.len() > MAX_BOXES
        || req.boxes.iter().any(|b| {
            [&b.device_id, &b.ephemeral, &b.data, &b.signature]
                .iter()
                .any(|f| f.is_empty() || f.len() > MAX_FIELD)
        })
    {
        return Err(ApiError::bad_request("invalid key boxes"));
    }
    let (sender, devices) = {
        let st = app.chat.voice.st.lock();
        let chan = st.chans.get(&channel).ok_or_else(not_in)?;
        let sender = chan
            .parts
            .values()
            .find(|p| p.info.device_id == device)
            .map(|p| p.info.participant)
            .ok_or_else(not_in)?;
        let devices: Vec<String> = chan.parts.values().map(|p| p.info.device_id.clone()).collect();
        (sender, devices)
    };
    for b in req.boxes {
        if b.device_id == device || !devices.contains(&b.device_id) {
            continue;
        }
        app.chat.hub.send_device(
            &b.device_id,
            &Envelope::new(
                types::VOICE_KEYS,
                VoiceKeyPush {
                    channel_id: Id(channel),
                    participant: sender,
                    device_id: device.to_owned(),
                    key_id: req.key_id,
                    ephemeral: b.ephemeral,
                    data: b.data,
                    signature: b.signature,
                },
            ),
        );
    }
    Ok(())
}

/// `device` leaves `channel`; with `reason`, it is told (`voice.left`).
pub async fn leave(app: &App, device: &str, channel: i64, reason: Option<&str>) {
    let lock = app.chat.voice.lock(channel);
    let held = lock.lock().await;
    let removed = {
        let mut st = app.chat.voice.st.lock();
        let Some(chan) = st.chans.get_mut(&channel) else { return };
        let Some(participant) = chan.parts.values().find(|p| p.info.device_id == device).map(|p| p.info.participant)
        else {
            return;
        };
        let part = chan.parts.remove(&participant);
        let (room, node, empty) = (chan.room.clone(), chan.node.clone(), chan.parts.is_empty());
        if empty {
            st.chans.remove(&channel);
        }
        if st.by_device.get(device) == Some(&channel) {
            st.by_device.remove(device);
        }
        part.map(|p| (p, room, node, empty))
    };
    let Some((part, room, node, empty)) = removed else { return };
    let op = if empty {
        VoiceOp::Close { room }
    } else {
        VoiceOp::Leave {
            room,
            participant: part.info.participant,
        }
    };
    let _ = run(app, node.as_ref(), op).await;
    app.egress.ledger.close(&part.session);
    app.egress.ended(&part.session);
    if let Some(why) = reason {
        app.chat.hub.send_device(
            device,
            &Envelope::new(
                types::VOICE_LEFT,
                VoiceLeft {
                    channel_id: Id(channel),
                    reason: why.to_owned(),
                },
            ),
        );
    }
    if !empty {
        sync_all(app, channel, None).await;
    }
    drop(held);
    end_lost_controls(app, channel).await;
    push_demand(app, channel);
    push_state(app, channel).await;
}

/// Every few seconds: devices without a chat connection, silent or failed
/// media connections, and people who lost the permission leave; those who
/// lost `speak` are not forwarded; a channel whose node is gone moves.
pub async fn sweep(app: &App) {
    let chans: Vec<(i64, String, Option<(String, String)>, Vec<Part>)> = {
        let st = app.chat.voice.st.lock();
        st.chans
            .iter()
            .map(|(id, c)| (*id, c.room.clone(), c.node.clone(), c.parts.values().cloned().collect()))
            .collect()
    };
    for (channel, room, node, parts) in chans {
        let mut out: Vec<(String, &'static str)> = parts
            .iter()
            .filter(|p| p.gone_since.is_some_and(|t| t.elapsed() > CHAT_GRACE))
            .map(|p| (p.info.device_id.clone(), "idle"))
            .collect();
        match run(app, node.as_ref(), VoiceOp::Idle { room: room.clone(), idle_ms: MEDIA_IDLE_MS }).await {
            Ok(result) => {
                if let Some(c) = app.chat.voice.st.lock().chans.get_mut(&channel) {
                    c.failures = 0;
                }
                for p in parts.iter().filter(|p| result.idle.contains(&p.info.participant)) {
                    out.push((p.info.device_id.clone(), "idle"));
                }
            }
            Err(_) => {
                let failed = app
                    .chat
                    .voice
                    .st
                    .lock()
                    .chans
                    .get_mut(&channel)
                    .map(|c| {
                        c.failures += 1;
                        c.failures >= NODE_FAILURES
                    })
                    .unwrap_or(false);
                if failed {
                    relocate(app, channel).await;
                    continue;
                }
            }
        }
        let users: Vec<String> = parts.iter().map(|p| p.info.user_id.clone()).collect();
        let allowed = app
            .db
            .call(move |c| {
                users
                    .iter()
                    .map(|u| {
                        let a = super::access(c, u, channel)?;
                        Ok((
                            a.as_ref().is_some_and(|a| a.can(perm::CONNECT)),
                            a.as_ref().is_some_and(|a| a.can(perm::SPEAK)),
                        ))
                    })
                    .collect::<Result<Vec<(bool, bool)>, _>>()
            })
            .await
            .unwrap_or_default();
        let mut changed = Vec::new();
        for (p, (connect, speak)) in parts.iter().zip(allowed) {
            if !connect {
                out.push((p.info.device_id.clone(), "forbidden"));
            } else if p.info.cannot_speak == speak {
                changed.push((p.info.participant, !speak));
            }
        }
        if !changed.is_empty() {
            for (participant, cannot) in changed {
                let info = {
                    let mut st = app.chat.voice.st.lock();
                    st.chans.get_mut(&channel).and_then(|c| c.parts.get_mut(&participant)).map(|p| {
                        p.info.cannot_speak = cannot;
                        p.info.clone()
                    })
                };
                if let Some(info) = info {
                    let op = VoiceOp::Mute {
                        room: room.clone(),
                        participant,
                        muted: silenced(&info),
                    };
                    let _ = run(app, node.as_ref(), op).await;
                }
            }
            push_state(app, channel).await;
        }
        out.sort();
        out.dedup_by(|a, b| a.0 == b.0);
        for (device, why) in out {
            leave(app, &device, channel, Some(why)).await;
        }
    }
}

/// The channel's node stopped answering: everyone in it is told to join
/// again, which picks a node anew.
async fn relocate(app: &App, channel: i64) {
    let lock = app.chat.voice.lock(channel);
    let _held = lock.lock().await;
    let removed = {
        let mut st = app.chat.voice.st.lock();
        let chan = st.chans.remove(&channel);
        if let Some(c) = &chan {
            for p in c.parts.values() {
                if st.by_device.get(&p.info.device_id) == Some(&channel) {
                    st.by_device.remove(&p.info.device_id);
                }
            }
        }
        chan
    };
    let Some(chan) = removed else { return };
    tracing::warn!(channel, node = ?chan.node.as_ref().map(|n| &n.0), "voice node lost; the channel moves");
    for p in chan.parts.values() {
        app.egress.ledger.close(&p.session);
        app.egress.ended(&p.session);
        app.chat.hub.send_device(
            &p.info.device_id,
            &Envelope::new(
                types::VOICE_MOVED,
                VoiceMoved {
                    channel_id: Id(channel),
                    to: None,
                },
            ),
        );
    }
    drop(_held);
    push_state(app, channel).await;
}
