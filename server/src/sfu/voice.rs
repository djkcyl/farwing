//! Voice channels on the forwarder (`docs/protocol/chat-design.md` §7.3,
//! §8).
//!
//! Every participant publishes on its own connection (its uplink: the
//! participant offers, naming each media section's kind) and receives what
//! it chose of everyone else's tracks on another (its downlink: the
//! forwarder offers, and offers again as tracks and choices change): every
//! `mic`, and the cameras and screens it subscribed to. Uplink packets are
//! brought to the forwarder's own tables (browsers number payload types and
//! header extensions their way); each downlink then only sets every
//! packet's MID to its track's on that connection. Tracks keep their SSRCs;
//! their stream is `p<participant>-<kind>`. Video is repaired here: lost
//! uplink packets are asked for again, a receiver's losses are resent from
//! a cache, keyframe requests reach the sender merged.
//!
//! The control node drives it with [`VoiceOp`]s, here or on an edge node:
//! a publication or a choice changes what someone should receive, and each
//! gets an offer once its previous one is answered.

use super::leg::{ForwardTrack, Leg, Rewrite, Rtcp, Tap, TapBuilder};
use super::room::{Cache, Missing};
use super::{SfuError, media_engine, negotiate, sdp, GATHER_TIMEOUT};
use farwing_protocol::chat::track;
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::configure_twcc_receiver_with_builder;
use webrtc::interceptor::registry::Registry;
use webrtc::interceptor::report::receiver::ReceiverReport;
use webrtc::interceptor::twcc::receiver::Receiver as TwccReceiver;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtcp::transport_feedbacks::transport_layer_nack::{TransportLayerNack, nack_pairs_from_sequence_numbers};
use webrtc::rtp::packet::Packet;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;

/// Keyframe requests for one track are merged within this.
const KEYFRAME_INTERVAL: Duration = Duration::from_millis(300);
/// Lost uplink video packets are looked for this often.
const NACK_TICK: Duration = Duration::from_millis(20);

/// A voice operation, as the control node sends it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "vop", rename_all = "snake_case")]
pub enum VoiceOp {
    /// Publishes, or renegotiates, `participant`'s uplink; the answer.
    /// `kinds`: (mid, kind) of its media sections (none: every audio
    /// section is a `mic`).
    Publish {
        room: String,
        participant: u32,
        sdp: String,
        #[serde(default)]
        kinds: Vec<(String, String)>,
    },
    /// `participant`'s downlink should carry what the others publish now:
    /// an offer, unless nothing changed or one is not answered yet.
    /// `session`: whose egress its packets are.
    Sync { room: String, participant: u32, session: String },
    /// `participant` chose these (participant, kind) besides every `mic`;
    /// an offer as for `Sync`.
    Subscribe {
        room: String,
        participant: u32,
        session: String,
        tracks: Vec<(u32, String)>,
    },
    /// The answer to offer `generation`; another offer if things changed
    /// meanwhile.
    Answer { room: String, participant: u32, generation: u64, sdp: String },
    /// A moderator's mute (or a lost permission to speak): its voice goes
    /// nowhere.
    Mute { room: String, participant: u32, muted: bool },
    /// A moderator's deafen: no sound reaches it.
    Deafen { room: String, participant: u32, deafened: bool },
    Leave { room: String, participant: u32 },
    Close { room: String },
    /// Participants whose connections failed, or who published and sent
    /// nothing for `idle_ms`.
    Idle { room: String, idle_ms: u64 },
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct VoiceResult {
    pub answer: String,
    /// (generation, offer)
    pub offer: Option<(u64, String)>,
    pub idle: Vec<u32>,
}

/// One track a participant sends.
struct UpTrack {
    /// As the sender announced it (its payload types).
    info: sdp::Track,
    kind: String,
    /// The forwarded codec: the forwarder's payload type and MIME.
    payload_type: u8,
    mime: String,
    cache: Mutex<Cache>,
    missing: Mutex<Missing>,
    keyframe_at: Mutex<Option<Instant>>,
}

impl UpTrack {
    fn video(&self) -> bool {
        self.info.kind == sdp::Kind::Video
    }
    /// Sound, which deafened members miss.
    fn sound(&self) -> bool {
        self.info.kind == sdp::Kind::Audio
    }
}

struct Uplink {
    pc: Arc<RTCPeerConnection>,
    tracks: RwLock<Vec<Arc<UpTrack>>>,
    /// The sender's tables, and how they become the forwarder's.
    wire: RwLock<sdp::Wire>,
    normalize: RwLock<Rewrite>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Uplink {
    fn track(&self, ssrc: u32) -> Option<Arc<UpTrack>> {
        self.tracks.read().iter().find(|t| t.info.ssrc == ssrc).cloned()
    }

    /// Asks the sender for a keyframe of `ssrc` (merged).
    async fn keyframe(&self, ssrc: u32) {
        let Some(t) = self.track(ssrc) else { return };
        {
            let mut at = t.keyframe_at.lock();
            if at.is_some_and(|at| at.elapsed() < KEYFRAME_INTERVAL) {
                return;
            }
            *at = Some(Instant::now());
        }
        let _ = self
            .pc
            .write_rtcp(&[Box::new(PictureLossIndication {
                sender_ssrc: 0,
                media_ssrc: ssrc,
            })])
            .await;
    }
}

#[derive(Default)]
struct Down {
    /// Forwarded tracks: their SSRC → (participant, sender).
    sent: HashMap<u32, (u32, Arc<RTCRtpSender>)>,
    /// Video tracks the offer out adds: a keyframe once it is answered.
    fresh: Vec<u32>,
    generation: u64,
    pending: Option<u64>,
    /// Changed while an offer was out.
    dirty: bool,
}

struct Member {
    participant: u32,
    muted: AtomicBool,
    deafened: AtomicBool,
    failed: AtomicBool,
    /// The last uplink packet.
    heard: Mutex<Option<Instant>>,
    uplink: RwLock<Option<Arc<Uplink>>>,
    leg: RwLock<Option<Arc<Leg>>>,
    /// (participant, kind) chosen besides every `mic`.
    subs: RwLock<HashSet<(u32, String)>>,
    down: tokio::sync::Mutex<Down>,
}

impl Member {
    fn new(participant: u32) -> Arc<Self> {
        Arc::new(Self {
            participant,
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            heard: Mutex::new(None),
            uplink: RwLock::new(None),
            leg: RwLock::new(None),
            subs: RwLock::default(),
            down: tokio::sync::Mutex::new(Down::default()),
        })
    }

    fn wants(&self, from: u32, kind: &str) -> bool {
        !track::chosen(kind) || self.subs.read().contains(&(from, kind.to_owned()))
    }

    async fn close(&self) {
        let uplink = self.uplink.write().take();
        if let Some(up) = uplink {
            for task in up.tasks.lock().drain(..) {
                task.abort();
            }
            let _ = up.pc.close().await;
        }
        let leg = self.leg.write().take();
        if let Some(leg) = leg {
            leg.close().await;
        }
    }
}

struct Room {
    members: RwLock<HashMap<u32, Arc<Member>>>,
}

impl Room {
    fn member(&self, participant: u32) -> Arc<Member> {
        if let Some(m) = self.members.read().get(&participant) {
            return Arc::clone(m);
        }
        Arc::clone(
            self.members
                .write()
                .entry(participant)
                .or_insert_with(|| Member::new(participant)),
        )
    }

    /// The uplink holding SSRC `ssrc`.
    fn uplink_of(&self, ssrc: u32) -> Option<Arc<Uplink>> {
        self.members.read().values().find_map(|m| {
            let up = m.uplink.read().clone()?;
            up.track(ssrc).is_some().then_some(up)
        })
    }

    /// `from`'s packet (on the forwarder's tables) to everyone who receives
    /// that track.
    fn fan_out(&self, from: &Member, up: &UpTrack, packet: Packet) {
        if up.kind == track::MIC && from.muted.load(Ordering::Relaxed) {
            return;
        }
        if up.video() && !up.cache.lock().insert(packet.clone()) {
            return;
        }
        for m in self.members.read().values() {
            if m.participant == from.participant
                || (up.sound() && m.deafened.load(Ordering::Relaxed))
                || !m.wants(from.participant, &up.kind)
            {
                continue;
            }
            if let Some(leg) = m.leg.read().as_ref() {
                leg.push(packet.clone());
            }
        }
    }

    /// A receiver's feedback on its downlink: losses resent from the cache,
    /// keyframe requests passed to the sender.
    async fn feedback(&self, leg: &Leg, packets: Rtcp) {
        for packet in packets {
            let any = packet.as_any();
            if let Some(nack) = any.downcast_ref::<TransportLayerNack>() {
                self.repair(leg, nack);
            } else if let Some(pli) = any.downcast_ref::<PictureLossIndication>() {
                if let Some(up) = self.uplink_of(pli.media_ssrc) {
                    up.keyframe(pli.media_ssrc).await;
                }
            } else if let Some(fir) = any.downcast_ref::<FullIntraRequest>() {
                for entry in &fir.fir {
                    if let Some(up) = self.uplink_of(entry.ssrc) {
                        up.keyframe(entry.ssrc).await;
                    }
                }
            }
        }
    }

    fn repair(&self, leg: &Leg, nack: &TransportLayerNack) {
        let Some(up) = self.uplink_of(nack.media_ssrc) else { return };
        let Some(t) = up.track(nack.media_ssrc) else { return };
        if t.info.rtx == 0 {
            return;
        }
        let Some(rtx_pt) = farwing_rtc_wire::rtx_payload_type(t.payload_type) else { return };
        let repairs: Vec<Packet> = {
            let cache = t.cache.lock();
            nack.nacks
                .iter()
                .flat_map(|pair| pair.packet_list())
                .filter_map(|seq| leg.repair(cache.get(seq)?, t.info.rtx, rtx_pt))
                .collect()
        };
        for packet in repairs {
            leg.push(packet);
        }
    }
}

/// The forwarder's tables: what its own offers use and uplinks are brought
/// to.
fn own_wire() -> sdp::Wire {
    let mut wire = sdp::Wire::default();
    wire.codecs.insert(
        farwing_rtc_wire::OPUS_PAYLOAD_TYPE,
        sdp::Codec {
            mime: "audio/opus".into(),
            clock_rate: 48_000,
            fmtp: farwing_rtc_wire::OPUS_FMTP.into(),
        },
    );
    for c in farwing_rtc_wire::VIDEO_CODECS {
        wire.codecs.insert(
            c.payload_type,
            sdp::Codec {
                mime: c.mime_type.to_ascii_lowercase(),
                clock_rate: 90_000,
                fmtp: c.fmtp.into(),
            },
        );
    }
    for (id, uri) in farwing_rtc_wire::header_extension_ids(&[farwing_rtc_wire::DEPENDENCY_DESCRIPTOR_URI]) {
        wire.extensions.insert(id, uri.to_owned());
    }
    wire
}

/// The tracks an offer announces, with their kinds and forwarded codecs
/// (each section's codec is the first of the answer's).
fn up_tracks(offer: &str, answer: &str, kinds: &[(String, String)], normalize: &Rewrite) -> Vec<Arc<UpTrack>> {
    let own = own_wire();
    let chosen = sdp::first_payload_types(answer);
    sdp::tracks(offer)
        .into_iter()
        .filter_map(|mut t| {
            if let Some(pt) = chosen.get(&t.mid) {
                t.payload_type = *pt;
            }
            let kind = match kinds.iter().find(|(mid, _)| *mid == t.mid) {
                Some((_, k)) if track::ALL.contains(&k.as_str()) => k.clone(),
                Some(_) => return None,
                None if kinds.is_empty() && t.kind == sdp::Kind::Audio => track::MIC.to_owned(),
                None => return None,
            };
            let payload_type = if normalize.identity {
                t.payload_type
            } else {
                *normalize.payload_types.get(&t.payload_type)?
            };
            let mime = own.codecs.get(&payload_type)?.mime.clone();
            Some(Arc::new(UpTrack {
                info: t,
                kind,
                payload_type,
                mime,
                cache: Mutex::new(Cache::new()),
                missing: Mutex::default(),
                keyframe_at: Mutex::default(),
            }))
        })
        .collect()
}

#[derive(Default)]
pub struct VoiceRooms {
    rooms: Mutex<HashMap<String, Arc<Room>>>,
}

impl VoiceRooms {
    fn room(&self, id: &str) -> Arc<Room> {
        Arc::clone(self.rooms.lock().entry(id.to_owned()).or_insert_with(|| {
            Arc::new(Room {
                members: RwLock::default(),
            })
        }))
    }

    fn existing(&self, id: &str) -> Option<Arc<Room>> {
        self.rooms.lock().get(id).cloned()
    }

    pub async fn handle(&self, sfu: &super::Sfu, op: VoiceOp) -> Result<VoiceResult, SfuError> {
        match op {
            VoiceOp::Publish {
                room,
                participant,
                sdp,
                kinds,
            } => Ok(VoiceResult {
                answer: self.publish(sfu, &room, participant, &sdp, &kinds).await?,
                ..Default::default()
            }),
            VoiceOp::Sync { room, participant, session } => {
                let room = self.room(&room);
                let member = room.member(participant);
                Ok(VoiceResult {
                    offer: sync(sfu, &room, &member, &session).await?,
                    ..Default::default()
                })
            }
            VoiceOp::Subscribe {
                room,
                participant,
                session,
                tracks,
            } => {
                let room = self.room(&room);
                let member = room.member(participant);
                *member.subs.write() = tracks.into_iter().filter(|(_, k)| track::chosen(k)).collect();
                Ok(VoiceResult {
                    offer: sync(sfu, &room, &member, &session).await?,
                    ..Default::default()
                })
            }
            VoiceOp::Answer {
                room: room_id,
                participant,
                generation,
                sdp,
            } => {
                let Some(room) = self.existing(&room_id) else {
                    return Err(SfuError::NoRoom);
                };
                let Some(member) = room.members.read().get(&participant).cloned() else {
                    return Err(SfuError::NoRoom);
                };
                let mut down = member.down.lock().await;
                if down.pending != Some(generation) {
                    return Ok(VoiceResult::default());
                }
                let Some(leg) = member.leg.read().clone() else {
                    return Err(SfuError::NotPublished);
                };
                let answer = RTCSessionDescription::answer(sdp).map_err(|e| SfuError::Description(e.to_string()))?;
                leg.pc.set_remote_description(answer).await?;
                down.pending = None;
                // Each forwarded SSRC's MID on this connection.
                let mut mids = HashMap::new();
                for transceiver in leg.pc.get_transceivers().await {
                    let Some(mid) = transceiver.mid() else { continue };
                    let Some(track) = transceiver.sender().await.track().await else {
                        continue;
                    };
                    if let Some(forward) = track.as_any().downcast_ref::<ForwardTrack>() {
                        for ssrc in [forward.track.ssrc, forward.track.rtx] {
                            if ssrc != 0 {
                                mids.insert(ssrc, bytes::Bytes::copy_from_slice(mid.as_bytes()));
                            }
                        }
                    }
                }
                leg.rewrite.write().mids = mids;
                // New pictures start with a keyframe.
                for ssrc in std::mem::take(&mut down.fresh) {
                    if let Some(up) = room.uplink_of(ssrc) {
                        up.keyframe(ssrc).await;
                    }
                }
                let offer = if down.dirty {
                    down.dirty = false;
                    let session = leg.id.clone();
                    offer(sfu, &room, &member, &mut down, &session).await?
                } else {
                    None
                };
                Ok(VoiceResult {
                    offer,
                    ..Default::default()
                })
            }
            VoiceOp::Mute { room, participant, muted } => {
                if let Some(room) = self.existing(&room)
                    && let Some(m) = room.members.read().get(&participant)
                {
                    m.muted.store(muted, Ordering::Relaxed);
                }
                Ok(VoiceResult::default())
            }
            VoiceOp::Deafen {
                room,
                participant,
                deafened,
            } => {
                if let Some(room) = self.existing(&room)
                    && let Some(m) = room.members.read().get(&participant)
                {
                    m.deafened.store(deafened, Ordering::Relaxed);
                }
                Ok(VoiceResult::default())
            }
            VoiceOp::Leave { room, participant } => {
                if let Some(r) = self.existing(&room) {
                    let member = r.members.write().remove(&participant);
                    if let Some(m) = member {
                        m.close().await;
                    }
                    if r.members.read().is_empty() {
                        self.rooms.lock().remove(&room);
                    }
                }
                Ok(VoiceResult::default())
            }
            VoiceOp::Close { room } => {
                let r = self.rooms.lock().remove(&room);
                if let Some(r) = r {
                    let members: Vec<Arc<Member>> = r.members.write().drain().map(|(_, m)| m).collect();
                    for m in members {
                        m.close().await;
                    }
                }
                Ok(VoiceResult::default())
            }
            VoiceOp::Idle { room, idle_ms } => {
                let idle = Duration::from_millis(idle_ms);
                let mut out = Vec::new();
                if let Some(r) = self.existing(&room) {
                    for m in r.members.read().values() {
                        let silent = m.uplink.read().is_some()
                            && m.heard.lock().is_none_or(|at| at.elapsed() > idle);
                        if m.failed.load(Ordering::Relaxed) || silent {
                            out.push(m.participant);
                        }
                    }
                }
                out.sort_unstable();
                Ok(VoiceResult {
                    idle: out,
                    ..Default::default()
                })
            }
        }
    }

    async fn publish(
        &self,
        sfu: &super::Sfu,
        room_id: &str,
        participant: u32,
        offer: &str,
        kinds: &[(String, String)],
    ) -> Result<String, SfuError> {
        let announced = sdp::tracks(offer);
        let room = self.room(room_id);
        // Tracks keep their SSRCs on every downlink: two participants'
        // must differ (they are random; the client offers anew).
        {
            let members = room.members.read();
            for m in members.values().filter(|m| m.participant != participant) {
                if let Some(up) = m.uplink.read().as_ref()
                    && up.tracks.read().iter().any(|t| announced.iter().any(|n| n.ssrc == t.info.ssrc))
                {
                    return Err(SfuError::Description("SSRC in use".into()));
                }
            }
        }
        let member = room.member(participant);
        let existing = member.uplink.read().clone();
        if let Some(up) = existing {
            let answer = negotiate(&up.pc, offer).await?;
            let wire = sdp::wire(&answer);
            let normalize = Rewrite::new(&wire, &own_wire());
            // Tracks already sent keep their caches.
            let old = up.tracks.read().clone();
            let tracks: Vec<Arc<UpTrack>> = up_tracks(offer, &answer, kinds, &normalize)
                .into_iter()
                .map(|t| {
                    old.iter()
                        .find(|o| o.info.ssrc == t.info.ssrc && o.kind == t.kind)
                        .cloned()
                        .unwrap_or(t)
                })
                .collect();
            *up.tracks.write() = tracks;
            *up.wire.write() = wire;
            *up.normalize.write() = normalize;
            return Ok(answer);
        }
        let mut media = media_engine()?;
        let mut registry = Registry::new();
        registry.add(Box::new(ReceiverReport::builder().with_interval(Duration::from_secs(1))));
        let registry = configure_twcc_receiver_with_builder(
            registry,
            &mut media,
            TwccReceiver::builder().with_interval(Duration::from_millis(100)),
        )?;
        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(sfu.settings()?)
            .build();
        let pc = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await?);
        let answer = match negotiate(&pc, offer).await {
            Ok(a) => a,
            Err(e) => {
                let _ = pc.close().await;
                return Err(e);
            }
        };
        let wire = sdp::wire(&answer);
        let normalize = Rewrite::new(&wire, &own_wire());
        let uplink = Arc::new(Uplink {
            pc: Arc::clone(&pc),
            tracks: RwLock::new(up_tracks(offer, &answer, kinds, &normalize)),
            wire: RwLock::new(wire),
            normalize: RwLock::new(normalize),
            tasks: Mutex::default(),
        });
        let (weak_room, weak_member, weak_up) = (Arc::downgrade(&room), Arc::downgrade(&member), Arc::downgrade(&uplink));
        pc.on_track(Box::new(move |remote, receiver, _| {
            let (Some(room), Some(member), Some(up)) = (weak_room.upgrade(), weak_member.upgrade(), weak_up.upgrade())
            else {
                return Box::pin(async {});
            };
            let mut tasks = Vec::new();
            {
                let (room, member, up, remote) =
                    (Arc::clone(&room), Arc::clone(&member), Arc::clone(&up), Arc::clone(&remote));
                tasks.push(tokio::spawn(async move {
                    while let Ok((mut packet, _)) = remote.read_rtp().await {
                        *member.heard.lock() = Some(Instant::now());
                        let Some(t) = up.track(packet.header.ssrc) else { continue };
                        if t.video() {
                            t.missing.lock().received(packet.header.sequence_number);
                        }
                        if !up.normalize.read().apply(&mut packet) {
                            continue;
                        }
                        room.fan_out(&member, &t, packet);
                    }
                }));
            }
            if remote.kind() == RTPCodecType::Video {
                // Retransmissions: the original packet, forwarded as media.
                let (room, member, up, remote) =
                    (Arc::clone(&room), Arc::clone(&member), Arc::clone(&up), Arc::clone(&remote));
                tasks.push(tokio::spawn(async move {
                    while let Ok((rtx, _)) = remote.read_rtx().await {
                        if rtx.payload.len() <= 2 {
                            continue;
                        }
                        let Some(t) = up.tracks.read().iter().find(|t| t.info.rtx == rtx.header.ssrc).cloned() else {
                            continue;
                        };
                        let Some(apt) = up.wire.read().codecs.get(&rtx.header.payload_type).and_then(sdp::Codec::apt)
                        else {
                            continue;
                        };
                        let mut original = rtx;
                        let seq = u16::from_be_bytes([original.payload[0], original.payload[1]]);
                        original.payload = original.payload.slice(2..);
                        original.header.ssrc = t.info.ssrc;
                        original.header.sequence_number = seq;
                        original.header.payload_type = apt;
                        original.header.padding = false;
                        t.missing.lock().received(seq);
                        if !up.normalize.read().apply(&mut original) {
                            continue;
                        }
                        room.fan_out(&member, &t, original);
                    }
                }));
            }
            // Receiver reports and transport-wide feedback flow while the
            // receiver's RTCP is read; the sender's reports also show it is
            // there while it says nothing.
            {
                let member = Arc::clone(&member);
                tasks.push(tokio::spawn(async move {
                    while receiver.read_rtcp().await.is_ok() {
                        *member.heard.lock() = Some(Instant::now());
                    }
                }));
            }
            up.tasks.lock().extend(tasks);
            Box::pin(async {})
        }));
        // Lost video packets asked for again.
        {
            let weak_up = Arc::downgrade(&uplink);
            uplink.tasks.lock().push(tokio::spawn(async move {
                let mut tick = tokio::time::interval(NACK_TICK);
                loop {
                    tick.tick().await;
                    let Some(up) = weak_up.upgrade() else { return };
                    let now = Instant::now();
                    let tracks = up.tracks.read().clone();
                    for t in tracks.iter().filter(|t| t.video() && t.info.rtx != 0) {
                        let due = t.missing.lock().due(now);
                        if due.is_empty() {
                            continue;
                        }
                        let _ = up
                            .pc
                            .write_rtcp(&[Box::new(TransportLayerNack {
                                sender_ssrc: 0,
                                media_ssrc: t.info.ssrc,
                                nacks: nack_pairs_from_sequence_numbers(&due),
                            })])
                            .await;
                    }
                }
            }));
        }
        let weak_member = Arc::downgrade(&member);
        pc.on_peer_connection_state_change(Box::new(move |state| {
            if matches!(state, RTCPeerConnectionState::Failed) && let Some(m) = weak_member.upgrade() {
                m.failed.store(true, Ordering::Relaxed);
            }
            Box::pin(async {})
        }));
        *member.heard.lock() = Some(Instant::now());
        let old = member.uplink.write().replace(uplink);
        if let Some(old) = old {
            for task in old.tasks.lock().drain(..) {
                task.abort();
            }
            let _ = old.pc.close().await;
        }
        Ok(answer)
    }
}

/// An offer for `member`'s downlink now, or later if one is out.
async fn sync(
    sfu: &super::Sfu,
    room: &Arc<Room>,
    member: &Arc<Member>,
    session: &str,
) -> Result<Option<(u64, String)>, SfuError> {
    let mut down = member.down.lock().await;
    if down.pending.is_some() {
        down.dirty = true;
        return Ok(None);
    }
    offer(sfu, room, member, &mut down, session).await
}

/// Brings `member`'s downlink to what it should receive; the offer when it
/// changed (`None`: nothing to send, or already so).
async fn offer(
    sfu: &super::Sfu,
    room: &Arc<Room>,
    member: &Arc<Member>,
    down: &mut Down,
    session: &str,
) -> Result<Option<(u64, String)>, SfuError> {
    let wanted: Vec<(u32, Arc<UpTrack>)> = {
        let members = room.members.read();
        let mut out = Vec::new();
        for m in members.values().filter(|m| m.participant != member.participant) {
            if let Some(up) = m.uplink.read().as_ref() {
                for t in up.tracks.read().iter().filter(|t| member.wants(m.participant, &t.kind)) {
                    out.push((m.participant, Arc::clone(t)));
                }
            }
        }
        out.sort_by_key(|(p, t)| (*p, t.info.ssrc));
        out
    };
    let existing = member.leg.read().clone();
    let leg = match existing {
        Some(leg) => leg,
        None if wanted.is_empty() => return Ok(None),
        None => new_leg(sfu, room, member, session).await?,
    };
    let mut changed = false;
    let gone: Vec<u32> = down
        .sent
        .keys()
        .filter(|ssrc| !wanted.iter().any(|(_, t)| t.info.ssrc == **ssrc))
        .copied()
        .collect();
    for ssrc in gone {
        if let Some((_, sender)) = down.sent.remove(&ssrc) {
            leg.pc.remove_track(&sender).await?;
            changed = true;
        }
    }
    for (participant, t) in wanted {
        if down.sent.contains_key(&t.info.ssrc) {
            continue;
        }
        let forward = ForwardTrack {
            track: sdp::Track {
                stream_id: track::stream(participant, &t.kind),
                id: t.kind.clone(),
                payload_type: t.payload_type,
                fec: 0,
                ..t.info.clone()
            },
            mime: t.mime.clone(),
        };
        let transceiver = leg
            .pc
            .add_transceiver_from_track(
                Arc::new(forward),
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Sendonly,
                    send_encodings: Vec::new(),
                }),
            )
            .await?;
        let sender = transceiver.sender().await;
        // RTCP reaches the tap only while each sender is read.
        let reading = Arc::clone(&sender);
        leg.tasks.lock().push(tokio::spawn(async move {
            while reading.read_rtcp().await.is_ok() {}
        }));
        if t.video() {
            down.fresh.push(t.info.ssrc);
        }
        down.sent.insert(t.info.ssrc, (participant, sender));
        changed = true;
    }
    if !changed && down.generation > 0 {
        return Ok(None);
    }
    let offer = leg.pc.create_offer(None).await?;
    let mut gathered = leg.pc.gathering_complete_promise().await;
    leg.pc.set_local_description(offer).await?;
    let _ = tokio::time::timeout(GATHER_TIMEOUT, gathered.recv()).await;
    let sdp = leg.pc.local_description().await.ok_or(SfuError::Timeout)?.sdp;
    down.generation += 1;
    down.pending = Some(down.generation);
    down.dirty = false;
    Ok(Some((down.generation, sdp)))
}

async fn new_leg(sfu: &super::Sfu, room: &Arc<Room>, member: &Arc<Member>, session: &str) -> Result<Arc<Leg>, SfuError> {
    let (rtcp_tx, mut rtcp_rx) = mpsc::unbounded_channel();
    let tap = Tap::new(rtcp_tx);
    let mut registry = Registry::new();
    registry.add(Box::new(TapBuilder(Arc::clone(&tap))));
    let api = APIBuilder::new()
        .with_media_engine(media_engine()?)
        .with_interceptor_registry(registry)
        .with_setting_engine(sfu.settings()?)
        .build();
    let pc = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await?);
    let wire = own_wire();
    let (leg, packets) = Leg::new(
        session.to_owned(),
        Arc::clone(&pc),
        tap,
        Rewrite::mids_only(wire.extension_id(farwing_rtc_wire::MID_URI)),
        wire.extension_id(farwing_rtc_wire::TRANSPORT_CC_URI),
        None,
        Vec::new(),
        sfu.meters.session(session),
    );
    {
        let mut tasks = leg.tasks.lock();
        tasks.push(tokio::spawn(Arc::clone(&leg).run(packets)));
        // The receiver's losses and keyframe requests.
        let (weak_room, weak_leg) = (Arc::downgrade(room), Arc::downgrade(&leg));
        tasks.push(tokio::spawn(async move {
            while let Some(packets) = rtcp_rx.recv().await {
                let (Some(room), Some(leg)) = (weak_room.upgrade(), weak_leg.upgrade()) else { return };
                room.feedback(&leg, packets).await;
            }
        }));
    }
    let weak_member = Arc::downgrade(member);
    pc.on_peer_connection_state_change(Box::new(move |state| {
        if matches!(state, RTCPeerConnectionState::Failed) && let Some(m) = weak_member.upgrade() {
            m.failed.store(true, Ordering::Relaxed);
        }
        Box::pin(async {})
    }));
    *member.leg.write() = Some(Arc::clone(&leg));
    Ok(leg)
}
