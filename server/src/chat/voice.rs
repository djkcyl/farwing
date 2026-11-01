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
    ChannelKind, Id, VoiceChannelState, VoiceJoin, VoiceJoined, VoiceKeyPush, VoiceKeys, VoiceLeft, VoiceMode,
    VoiceModerate, VoiceOffer, VoiceParticipant, VoiceSelf, perm, types,
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
/// Key boxes per request, and the size of each field.
const MAX_BOXES: usize = 128;
const MAX_FIELD: usize = 1024;

#[derive(Clone)]
struct Part {
    info: VoiceParticipant,
    /// Its egress (and relay credentials) are this session's.
    session: String,
    gone_since: Option<Instant>,
}

struct Chan {
    room: String,
    /// An edge node's forwarder; `None`: this server's.
    node: Option<(String, String)>,
    mode: VoiceMode,
    parts: BTreeMap<u32, Part>,
}

#[derive(Default)]
struct State {
    chans: HashMap<i64, Chan>,
    by_device: HashMap<String, i64>,
    next: u32,
}

#[derive(Default)]
pub struct Voice {
    st: Mutex<State>,
    /// One operation at a time per channel.
    locks: Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
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
        });
        chan.parts.insert(
            participant,
            Part {
                info,
                session: session.clone(),
                gone_since: None,
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
pub async fn publish(app: &App, user: &str, device: &str, channel: i64, sdp: String) -> ApiResult<String> {
    if sdp.is_empty() || sdp.len() > 64 * 1024 {
        return Err(ApiError::bad_request("invalid description"));
    }
    let u = user.to_owned();
    let speak = app
        .db
        .call(move |c| Ok(super::access(c, &u, channel)?.is_some_and(|a| a.can(perm::SPEAK))))
        .await?;
    if !speak {
        return Err(crate::http::chat::forbidden());
    }
    let lock = app.chat.voice.lock(channel);
    let held = lock.lock().await;
    let (part, room, node) = app.chat.voice.part(channel, device).ok_or_else(not_in)?;
    let participant = part.info.participant;
    let result = run(
        app,
        node.as_ref(),
        VoiceOp::Publish {
            room,
            participant,
            sdp,
        },
    )
    .await?;
    if part.info.server_muted {
        let (_, room, node) = app.chat.voice.part(channel, device).ok_or_else(not_in)?;
        run(app, node.as_ref(), VoiceOp::Mute { room, participant, muted: true }).await?;
    }
    drop(held);
    let app2 = app.clone();
    tokio::spawn(async move {
        let lock = app2.chat.voice.lock(channel);
        let _held = lock.lock().await;
        sync_all(&app2, channel, Some(participant)).await;
    });
    Ok(result.answer)
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
    }
    push_state(app, channel).await;
    Ok(())
}

/// `PATCH …/voice/participants/{participant}` (`voice_moderate`).
pub async fn moderate(app: &App, user: &str, channel: i64, participant: u32, req: VoiceModerate) -> ApiResult<()> {
    let u = user.to_owned();
    let allowed = app
        .db
        .call(move |c| Ok(super::access(c, &u, channel)?.is_some_and(|a| a.can(perm::VOICE_MODERATE))))
        .await?;
    if !allowed {
        return Err(crate::http::chat::forbidden());
    }
    let lock = app.chat.voice.lock(channel);
    let _held = lock.lock().await;
    let (room, node, muted) = {
        let mut st = app.chat.voice.st.lock();
        let chan = st.chans.get_mut(&channel).ok_or_else(not_in)?;
        let (room, node) = (chan.room.clone(), chan.node.clone());
        let part = chan.parts.get_mut(&participant).ok_or_else(not_in)?;
        if let Some(m) = req.server_muted {
            part.info.server_muted = m;
        }
        if let Some(d) = req.server_deafened {
            part.info.server_deafened = d;
        }
        (room, node, req.server_muted)
    };
    if let Some(muted) = muted {
        run(app, node.as_ref(), VoiceOp::Mute { room, participant, muted }).await?;
    }
    drop(_held);
    push_state(app, channel).await;
    Ok(())
}

/// `DELETE …/voice/participants/{participant}` (`voice_moderate`).
pub async fn disconnect(app: &App, user: &str, channel: i64, participant: u32) -> ApiResult<()> {
    let u = user.to_owned();
    let allowed = app
        .db
        .call(move |c| Ok(super::access(c, &u, channel)?.is_some_and(|a| a.can(perm::VOICE_MODERATE))))
        .await?;
    if !allowed {
        return Err(crate::http::chat::forbidden());
    }
    let device = {
        let st = app.chat.voice.st.lock();
        st.chans
            .get(&channel)
            .and_then(|c| c.parts.get(&participant))
            .map(|p| p.info.device_id.clone())
            .ok_or_else(not_in)?
    };
    leave(app, &device, channel, Some("disconnected")).await;
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
    push_state(app, channel).await;
}

/// Every few seconds: devices without a chat connection, silent or failed
/// media connections, and people who lost the permission leave.
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
        if let Ok(result) = run(app, node.as_ref(), VoiceOp::Idle { room, idle_ms: MEDIA_IDLE_MS }).await {
            for p in parts.iter().filter(|p| result.idle.contains(&p.info.participant)) {
                out.push((p.info.device_id.clone(), "idle"));
            }
        }
        let users: Vec<String> = parts.iter().map(|p| p.info.user_id.clone()).collect();
        let allowed = app
            .db
            .call(move |c| {
                users
                    .iter()
                    .map(|u| Ok(super::access(c, u, channel)?.is_some_and(|a| a.can(perm::CONNECT))))
                    .collect::<Result<Vec<bool>, _>>()
            })
            .await
            .unwrap_or_default();
        for (p, ok) in parts.iter().zip(allowed) {
            if !ok {
                out.push((p.info.device_id.clone(), "forbidden"));
            }
        }
        out.sort();
        out.dedup_by(|a, b| a.0 == b.0);
        for (device, why) in out {
            leave(app, &device, channel, Some(why)).await;
        }
    }
}
