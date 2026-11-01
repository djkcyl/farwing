//! Voice channels on the forwarder (`docs/protocol/chat-design.md` §7.3).
//!
//! Every participant publishes on its own connection (its uplink: the
//! participant offers) and receives everyone else's tracks on another (its
//! downlink: the forwarder offers, and offers again as tracks come and go).
//! Uplink packets are brought to the forwarder's own tables (browsers number
//! payload types and header extensions their way); each downlink then only
//! sets every packet's MID to its track's on that connection. Tracks keep
//! their SSRCs; their stream is `p<participant>`.
//!
//! The control node drives it with [`VoiceOp`]s, here or on an edge node:
//! a publication changes what the others should receive, and each of them
//! gets an offer once its previous one is answered.

use super::leg::{ForwardTrack, Leg, Rewrite, Tap, TapBuilder};
use super::{SfuError, media_engine, negotiate, sdp, GATHER_TIMEOUT};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
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
use webrtc::rtp::packet::Packet;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;

/// A voice operation, as the control node sends it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "vop", rename_all = "snake_case")]
pub enum VoiceOp {
    /// Publishes, or renegotiates, `participant`'s uplink; the answer.
    Publish { room: String, participant: u32, sdp: String },
    /// `participant`'s downlink should carry what the others publish now:
    /// an offer, unless nothing changed or one is not answered yet.
    /// `session`: whose egress its packets are.
    Sync { room: String, participant: u32, session: String },
    /// The answer to offer `generation`; another offer if things changed
    /// meanwhile.
    Answer { room: String, participant: u32, generation: u64, sdp: String },
    /// A moderator's mute: its packets go nowhere.
    Mute { room: String, participant: u32, muted: bool },
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

struct Uplink {
    pc: Arc<RTCPeerConnection>,
    tracks: RwLock<Vec<sdp::Track>>,
    /// The uplink's tables to the forwarder's.
    normalize: RwLock<Rewrite>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

#[derive(Default)]
struct Down {
    /// Forwarded tracks: their SSRC → (participant, sender).
    sent: HashMap<u32, (u32, Arc<RTCRtpSender>)>,
    generation: u64,
    pending: Option<u64>,
    /// Changed while an offer was out.
    dirty: bool,
}

struct Member {
    participant: u32,
    muted: AtomicBool,
    failed: AtomicBool,
    /// The last uplink packet.
    heard: Mutex<Option<Instant>>,
    uplink: RwLock<Option<Arc<Uplink>>>,
    leg: RwLock<Option<Arc<Leg>>>,
    down: tokio::sync::Mutex<Down>,
}

impl Member {
    fn new(participant: u32) -> Arc<Self> {
        Arc::new(Self {
            participant,
            muted: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            heard: Mutex::new(None),
            uplink: RwLock::new(None),
            leg: RwLock::new(None),
            down: tokio::sync::Mutex::new(Down::default()),
        })
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

    /// `from`'s packet to everyone else's downlink.
    fn fan_out(&self, from: &Member, packet: &Packet) {
        if from.muted.load(Ordering::Relaxed) {
            return;
        }
        for m in self.members.read().values() {
            if m.participant == from.participant {
                continue;
            }
            if let Some(leg) = m.leg.read().as_ref() {
                leg.push(packet.clone());
            }
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
            VoiceOp::Publish { room, participant, sdp } => Ok(VoiceResult {
                answer: self.publish(sfu, &room, participant, &sdp).await?,
                ..Default::default()
            }),
            VoiceOp::Sync { room, participant, session } => {
                let room = self.room(&room);
                let member = room.member(participant);
                let mut down = member.down.lock().await;
                if down.pending.is_some() {
                    down.dirty = true;
                    return Ok(VoiceResult::default());
                }
                let offer = offer(sfu, &room, &member, &mut down, &session).await?;
                Ok(VoiceResult {
                    offer,
                    ..Default::default()
                })
            }
            VoiceOp::Answer {
                room,
                participant,
                generation,
                sdp,
            } => {
                let Some(room) = self.existing(&room) else {
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
                        for ssrc in [forward.track.ssrc, forward.track.rtx, forward.track.fec] {
                            if ssrc != 0 {
                                mids.insert(ssrc, bytes::Bytes::copy_from_slice(mid.as_bytes()));
                            }
                        }
                    }
                }
                leg.rewrite.write().mids = mids;
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

    async fn publish(&self, sfu: &super::Sfu, room_id: &str, participant: u32, offer: &str) -> Result<String, SfuError> {
        let tracks = sdp::tracks(offer);
        let room = self.room(room_id);
        // Tracks keep their SSRCs on every downlink: two participants'
        // must differ (they are random; the client offers anew).
        {
            let members = room.members.read();
            for m in members.values().filter(|m| m.participant != participant) {
                if let Some(up) = m.uplink.read().as_ref()
                    && up.tracks.read().iter().any(|t| tracks.iter().any(|n| n.ssrc == t.ssrc))
                {
                    return Err(SfuError::Description("SSRC in use".into()));
                }
            }
        }
        let member = room.member(participant);
        let existing = member.uplink.read().clone();
        if let Some(up) = existing {
            let answer = negotiate(&up.pc, offer).await?;
            *up.tracks.write() = tracks;
            *up.normalize.write() = Rewrite::new(&sdp::wire(&answer), &own_wire());
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
        let uplink = Arc::new(Uplink {
            pc: Arc::clone(&pc),
            tracks: RwLock::new(tracks),
            normalize: RwLock::new(Rewrite::new(&sdp::wire(&answer), &own_wire())),
            tasks: Mutex::default(),
        });
        let (weak_room, weak_member, weak_up) = (Arc::downgrade(&room), Arc::downgrade(&member), Arc::downgrade(&uplink));
        pc.on_track(Box::new(move |track, receiver, _| {
            let (Some(room), Some(member), Some(up)) = (weak_room.upgrade(), weak_member.upgrade(), weak_up.upgrade())
            else {
                return Box::pin(async {});
            };
            if !up.tracks.read().iter().any(|t| t.ssrc == track.ssrc()) {
                tracing::debug!(ssrc = track.ssrc(), "unannounced voice track");
            }
            let reader = {
                let (room, member, up) = (Arc::clone(&room), Arc::clone(&member), Arc::clone(&up));
                tokio::spawn(async move {
                    while let Ok((mut packet, _)) = track.read_rtp().await {
                        *member.heard.lock() = Some(Instant::now());
                        if !up.normalize.read().apply(&mut packet) {
                            continue;
                        }
                        room.fan_out(&member, &packet);
                    }
                })
            };
            // Receiver reports and transport-wide feedback flow while the
            // receiver's RTCP is read; the sender's reports also show it is
            // there while it says nothing.
            let rtcp = {
                let member = Arc::clone(&member);
                tokio::spawn(async move {
                    while receiver.read_rtcp().await.is_ok() {
                        *member.heard.lock() = Some(Instant::now());
                    }
                })
            };
            up.tasks.lock().extend([reader, rtcp]);
            Box::pin(async {})
        }));
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
            let _ = old.pc.close().await;
        }
        Ok(answer)
    }
}

/// Brings `member`'s downlink to the others' tracks; the offer when it
/// changed (`None`: nothing to send, or already so).
async fn offer(
    sfu: &super::Sfu,
    room: &Room,
    member: &Arc<Member>,
    down: &mut Down,
    session: &str,
) -> Result<Option<(u64, String)>, SfuError> {
    let wanted: Vec<(u32, sdp::Track)> = {
        let members = room.members.read();
        let mut out = Vec::new();
        for m in members.values().filter(|m| m.participant != member.participant) {
            if let Some(up) = m.uplink.read().as_ref() {
                for t in up.tracks.read().iter().filter(|t| t.kind == sdp::Kind::Audio) {
                    out.push((m.participant, t.clone()));
                }
            }
        }
        out.sort_by_key(|(p, t)| (*p, t.ssrc));
        out
    };
    let existing = member.leg.read().clone();
    let leg = match existing {
        Some(leg) => leg,
        None if wanted.is_empty() => return Ok(None),
        None => new_leg(sfu, member, session).await?,
    };
    let mut changed = false;
    let gone: Vec<u32> = down
        .sent
        .keys()
        .filter(|ssrc| !wanted.iter().any(|(_, t)| t.ssrc == **ssrc))
        .copied()
        .collect();
    for ssrc in gone {
        if let Some((_, sender)) = down.sent.remove(&ssrc) {
            leg.pc.remove_track(&sender).await?;
            changed = true;
        }
    }
    for (participant, track) in wanted {
        if down.sent.contains_key(&track.ssrc) {
            continue;
        }
        let forward = ForwardTrack {
            track: sdp::Track {
                stream_id: format!("p{participant}"),
                ..track.clone()
            },
            mime: "audio/opus".into(),
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
        down.sent.insert(track.ssrc, (participant, sender));
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

async fn new_leg(sfu: &super::Sfu, member: &Arc<Member>, session: &str) -> Result<Arc<Leg>, SfuError> {
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
        // Receiver reports and feedback: nothing to act on for voice.
        tasks.push(tokio::spawn(async move { while rtcp_rx.recv().await.is_some() {} }));
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
