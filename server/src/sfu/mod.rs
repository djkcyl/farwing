//! Multi-viewer media forwarding (SFU), `docs/protocol/deployment-design.md` §5.
//!
//! A host publishes each encoding once (the uplink); every viewer receives
//! the host's packets on its own connection (a leg). Packets keep their
//! SSRC, sequence number, payload type and header extensions — RS-FEC
//! covers whole packets — except the transport-wide sequence number, which
//! belongs to each connection. The forwarder answers viewers' retransmission
//! requests from its own copy, asks the host once for packets lost on the
//! uplink, passes the host's sender reports on and merges keyframe requests.
//!
//! Every forwarder connection shares one UDP port and offers host
//! candidates only (the public address); clients reach it directly or
//! through their relay, and the forwarder answers their checks from there
//! (peer-reflexive candidates, no trickling needed).

mod emulate;
mod estimate;
mod leg;
mod room;
pub mod sdp;

pub use room::{LegStats, RoomStats};

use farwing_protocol::api::ForwardLayer;
use leg::{ForwardTrack, Leg, Rewrite, Tap, TapBuilder};
use parking_lot::{Mutex, RwLock};
use room::{Room, Uplink};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::configure_twcc_receiver_with_builder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::ice::network_type::NetworkType;
use webrtc::ice::udp_network::{EphemeralUDP, UDPNetwork};
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
use webrtc::interceptor::registry::Registry;
use webrtc::interceptor::report::receiver::ReceiverReport;
use webrtc::interceptor::twcc::receiver::Receiver as TwccReceiver;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtcp::sender_report::SenderReport;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;

/// Gathering is local (host candidates on one port) and quick.
const GATHER_TIMEOUT: Duration = Duration::from_secs(5);
/// How often lost uplink packets are requested.
const TICK: Duration = Duration::from_millis(10);

/// The leg of a session's connection `connection`: a viewer may hold two
/// for a moment (the new one up before the old one is left).
pub fn leg_id(session: &str, connection: u32) -> String {
    format!("{session}#{connection}")
}

/// The session of leg `id`.
pub(crate) fn session_of(id: &str) -> &str {
    id.split_once('#').map_or(id, |(session, _)| session)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SfuConfig {
    /// The UDP port every forwarder connection shares.
    pub port: u16,
    /// Local address; unspecified = every interface.
    pub bind: IpAddr,
    /// Address announced to clients (1:1 NAT); else the interface addresses.
    pub public_ip: Option<IpAddr>,
    /// Loopback addresses as candidates (one-machine tests).
    pub loopback: bool,
}

impl SfuConfig {
    /// The forwarder of a server whose relay binds `bind` and is reached at
    /// `public_ip`; a loopback `bind` is a test on one machine.
    pub fn new(port: u16, bind: IpAddr, public_ip: Option<IpAddr>) -> Self {
        Self {
            port,
            bind,
            public_ip,
            loopback: bind.is_loopback(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SfuError {
    #[error("no such room")]
    NoRoom,
    #[error("nothing is published in the room")]
    NotPublished,
    #[error("invalid description: {0}")]
    Description(String),
    #[error("forwarder settings: {0}")]
    Settings(String),
    #[error(transparent)]
    WebRtc(#[from] webrtc::Error),
    #[error("candidate gathering timed out")]
    Timeout,
}

/// A forwarder operation, as the control node sends it to an edge node.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SfuRequest {
    Publish {
        room: String,
        sdp: String,
    },
    Subscribe {
        room: String,
        leg: String,
        sdp: String,
        #[serde(default)]
        tracks: Option<Vec<String>>,
        #[serde(default)]
        layer: ForwardLayer,
    },
    /// The stream viewer `session` chose.
    Layer {
        room: String,
        session: String,
        layer: ForwardLayer,
    },
    /// `leg`: one connection's leg, or a session (all its legs).
    Unsubscribe {
        room: String,
        leg: String,
    },
    Close {
        room: String,
    },
}

pub struct Sfu {
    config: RwLock<SfuConfig>,
    rooms: Mutex<HashMap<String, Arc<Room>>>,
    /// What the viewers' connections send, per session.
    meters: Arc<crate::egress::Meters>,
}

fn media_engine() -> Result<MediaEngine, SfuError> {
    let mut media = MediaEngine::default();
    farwing_rtc_wire::register_codecs(&mut media)?;
    farwing_rtc_wire::register_header_extensions(
        &mut media,
        &[farwing_rtc_wire::DEPENDENCY_DESCRIPTOR_URI],
    )?;
    Ok(media)
}

impl Sfu {
    pub fn new(config: SfuConfig, meters: Arc<crate::egress::Meters>) -> Arc<Self> {
        Arc::new(Self {
            config: RwLock::new(config),
            rooms: Mutex::default(),
            meters,
        })
    }

    pub fn config(&self) -> SfuConfig {
        self.config.read().clone()
    }

    /// New connections use `config`; existing ones keep their port.
    pub fn set_config(&self, config: SfuConfig) {
        *self.config.write() = config;
    }

    fn settings(&self) -> Result<SettingEngine, SfuError> {
        let config = self.config();
        let mut settings = SettingEngine::default();
        settings.set_udp_network(UDPNetwork::Ephemeral(
            EphemeralUDP::shared(config.port).map_err(|e| SfuError::Settings(e.to_string()))?,
        ));
        let v4 = config.public_ip.map_or(
            config.bind.is_ipv4() || config.bind.is_unspecified(),
            |ip| ip.is_ipv4(),
        );
        settings.set_network_types(vec![if v4 {
            NetworkType::Udp4
        } else {
            NetworkType::Udp6
        }]);
        if let Some(ip) = config.public_ip {
            settings.set_nat_1to1_ips(vec![ip.to_string()], RTCIceCandidateType::Host);
        }
        if !config.bind.is_unspecified() {
            let bind = config.bind;
            settings.set_ip_filter(Box::new(move |ip| ip == bind));
        }
        settings.set_include_loopback_candidate(config.loopback);
        // Video packets fill a 1500-byte MTU.
        settings.set_receive_mtu(2048);
        settings.set_srtp_replay_protection_window(1024);
        settings.set_srtcp_replay_protection_window(128);
        settings.enable_sender_rtx(true);
        settings.enable_sender_rsfec(true);
        Ok(settings)
    }

    fn room(&self, id: &str) -> Option<Arc<Room>> {
        self.rooms.lock().get(id).cloned()
    }

    /// The server's egress budget per viewer of room `id` (none: no limit).
    pub fn set_budget(&self, id: &str, bps: Option<u64>) {
        if let Some(room) = self.room(id) {
            room.budget
                .store(bps.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Publishes (or republishes) the host's tracks in `room`; returns the
    /// answer. Republishing replaces the uplink and ends every leg: viewers
    /// subscribe again to the new tracks.
    pub async fn publish(&self, room: &str, offer: &str) -> Result<String, SfuError> {
        let tracks = sdp::tracks(offer);
        if tracks.is_empty() {
            return Err(SfuError::Description("no tracks".into()));
        }
        let mut media = media_engine()?;
        let mut registry = Registry::new();
        registry.add(Box::new(
            ReceiverReport::builder().with_interval(Duration::from_secs(1)),
        ));
        let registry = configure_twcc_receiver_with_builder(
            registry,
            &mut media,
            TwccReceiver::builder().with_interval(Duration::from_millis(50)),
        )?;
        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(self.settings()?)
            .build();
        let pc = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await?);
        let answer = match negotiate(&pc, offer).await {
            Ok(answer) => answer,
            Err(error) => {
                let _ = pc.close().await;
                return Err(error);
            }
        };
        let uplink = Arc::new(Uplink::new(Arc::clone(&pc), tracks, sdp::wire(&answer)));

        let room = {
            let mut rooms = self.rooms.lock();
            Arc::clone(
                rooms
                    .entry(room.to_owned())
                    .or_insert_with(|| Room::new(room.to_owned())),
            )
        };
        // Viewers follow the new tracks with a new subscription.
        let legs: Vec<Arc<Leg>> = room.legs.write().drain().map(|(_, l)| l).collect();
        for leg in legs {
            leg.close().await;
        }
        let weak_room = Arc::downgrade(&room);
        let weak_uplink = Arc::downgrade(&uplink);
        pc.on_track(Box::new(move |track, receiver, _| {
            let (Some(room), Some(uplink)) = (weak_room.upgrade(), weak_uplink.upgrade()) else {
                return Box::pin(async {});
            };
            let Some(up) = uplink
                .tracks
                .iter()
                .find(|t| t.info.ssrc == track.ssrc())
                .cloned()
            else {
                tracing::debug!(ssrc = track.ssrc(), "unannounced uplink track");
                return Box::pin(async {});
            };
            let mut tasks = Vec::new();
            {
                let (room, up, track) = (Arc::clone(&room), Arc::clone(&up), Arc::clone(&track));
                tasks.push(tokio::spawn(async move {
                    while let Ok((packet, _)) = track.read_rtp().await {
                        room.media(&up, packet);
                    }
                }));
            }
            if track.kind() == RTPCodecType::Video {
                {
                    let (room, up, track, link) = (
                        Arc::clone(&room),
                        Arc::clone(&up),
                        Arc::clone(&track),
                        Arc::clone(&uplink),
                    );
                    tasks.push(tokio::spawn(async move {
                        while let Ok((packet, _)) = track.read_rtx().await {
                            room.rtx(&link, &up, packet);
                        }
                    }));
                }
                let (room, track) = (Arc::clone(&room), Arc::clone(&track));
                tasks.push(tokio::spawn(async move {
                    while let Ok((packet, _)) = track.read_fec().await {
                        room.fec(packet);
                    }
                }));
            }
            {
                let room = Arc::clone(&room);
                tasks.push(tokio::spawn(async move {
                    while let Ok((packets, _)) = receiver.read_rtcp().await {
                        for packet in packets {
                            if let Some(report) = packet.as_any().downcast_ref::<SenderReport>() {
                                room.sender_report(report).await;
                            }
                        }
                    }
                }));
            }
            uplink.tasks.lock().extend(tasks);
            Box::pin(async {})
        }));
        room.replace_uplink(Some(uplink)).await;
        let mut ticker = room.ticker.lock();
        if ticker.is_none() {
            let weak = Arc::downgrade(&room);
            *ticker = Some(tokio::spawn(async move {
                let mut interval = tokio::time::interval(TICK);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    let Some(room) = weak.upgrade() else { break };
                    room.tick().await;
                }
            }));
        }
        Ok(answer)
    }

    /// Adds (or replaces) viewer `leg` of `room`; `tracks` picks the
    /// published tracks by ID, in the order of the offer's media sections of
    /// each kind (`None`: all, in publishing order). Returns the answer.
    pub async fn subscribe(
        &self,
        room_id: &str,
        leg_id: &str,
        offer: &str,
        tracks: Option<&[String]>,
    ) -> Result<String, SfuError> {
        let room = self.room(room_id).ok_or(SfuError::NoRoom)?;
        let uplink = room.current().ok_or(SfuError::NotPublished)?;
        let sections = sdp::media_sections(offer);
        let wanted: Vec<&sdp::Track> = match tracks {
            Some(ids) => ids
                .iter()
                .filter_map(|id| uplink.tracks.iter().find(|t| &t.info.id == id))
                .map(|t| &t.info)
                .collect(),
            None => uplink.tracks.iter().map(|t| &t.info).collect(),
        };
        let mut chosen = Vec::new();
        for kind in [sdp::Kind::Audio, sdp::Kind::Video] {
            let slots = sections.iter().filter(|k| **k == kind).count();
            chosen.extend(wanted.iter().filter(|t| t.kind == kind).take(slots).copied());
        }

        let (rtcp_tx, mut rtcp_rx) = mpsc::unbounded_channel();
        let tap = Tap::new(rtcp_tx);
        let mut registry = Registry::new();
        registry.add(Box::new(TapBuilder(Arc::clone(&tap))));
        let api = APIBuilder::new()
            .with_media_engine(media_engine()?)
            .with_interceptor_registry(registry)
            .with_setting_engine(self.settings()?)
            .build();
        let pc = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await?);
        let result = async {
            for track in &chosen {
                let mime = uplink
                    .wire
                    .codecs
                    .get(&track.payload_type)
                    .map(|c| c.mime.clone())
                    .unwrap_or_default();
                pc.add_transceiver_from_track(
                    Arc::new(ForwardTrack {
                        track: (*track).clone(),
                        mime,
                    }),
                    Some(RTCRtpTransceiverInit {
                        direction: RTCRtpTransceiverDirection::Sendonly,
                        send_encodings: Vec::new(),
                    }),
                )
                .await?;
            }
            negotiate(&pc, offer).await
        }
        .await;
        let answer = match result {
            Ok(answer) => answer,
            Err(error) => {
                let _ = pc.close().await;
                return Err(error);
            }
        };
        let leg_wire = sdp::wire(&answer);
        let tcc = leg_wire.extension_id(farwing_rtc_wire::TRANSPORT_CC_URI);
        let mut rewrite = Rewrite::new(&uplink.wire, &leg_wire);
        if !rewrite.identity {
            for transceiver in pc.get_transceivers().await {
                let (Some(mid), Some(track)) =
                    (transceiver.mid(), transceiver.sender().await.track().await)
                else {
                    continue;
                };
                let Some(info) = chosen.iter().find(|t| t.id == track.id()) else {
                    continue;
                };
                for ssrc in [info.ssrc, info.rtx, info.fec].into_iter().filter(|s| *s != 0) {
                    rewrite
                        .mids
                        .insert(ssrc, bytes::Bytes::copy_from_slice(mid.as_bytes()));
                }
            }
        }
        // Development emulation follows the viewer (its session), counted
        // from 1 in the order viewers first subscribe.
        let emulator = {
            let mut emulators = room.emulators.lock();
            let count = emulators.len();
            emulators
                .entry(session_of(leg_id).to_owned())
                .or_insert_with(|| {
                    emulate::Emulator::for_viewer(count + 1, leg_id)
                        .map(|e| Arc::new(parking_lot::Mutex::new(e)))
                })
                .clone()
        };
        let video: Arc<[u32]> = chosen
            .iter()
            .filter(|t| t.kind == sdp::Kind::Video)
            .map(|t| t.ssrc)
            .collect();
        let (leg, packets) = Leg::new(
            leg_id.to_owned(),
            Arc::clone(&pc),
            tap,
            rewrite,
            tcc,
            emulator,
            video.to_vec(),
            self.meters.session(session_of(leg_id)),
        );
        let senders = pc.get_senders().await;
        {
            let mut tasks = leg.tasks.lock();
            tasks.push(tokio::spawn(Arc::clone(&leg).run(packets)));
            let (weak_room, weak_leg) = (Arc::downgrade(&room), Arc::downgrade(&leg));
            tasks.push(tokio::spawn(async move {
                while let Some(packets) = rtcp_rx.recv().await {
                    let (Some(room), Some(leg)) = (weak_room.upgrade(), weak_leg.upgrade()) else {
                        break;
                    };
                    room.leg_rtcp(&leg, packets).await;
                }
            }));
            // RTCP reaches the tap only while each media sender is read.
            for sender in senders {
                tasks.push(tokio::spawn(async move {
                    while sender.read_rtcp().await.is_ok() {}
                }));
            }
        }
        let weak_room = Arc::downgrade(&room);
        let id = leg_id.to_owned();
        let weak_pc = Arc::downgrade(&pc);
        pc.on_peer_connection_state_change(Box::new(move |state| {
            let (weak_room, id, weak_pc, video) =
                (weak_room.clone(), id.clone(), weak_pc.clone(), video.clone());
            Box::pin(async move {
                let Some(room) = weak_room.upgrade() else { return };
                // A new viewer starts with a keyframe once packets reach it:
                // one asked for at the subscription went out before.
                if state == RTCPeerConnectionState::Connected {
                    room.first_keyframe(&video).await;
                    return;
                }
                if !matches!(
                    state,
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
                ) {
                    return;
                }
                // Only this connection's leg (a replacement keeps its ID).
                let current = room.legs.read().get(&id).cloned();
                if let Some(leg) = current
                    && weak_pc.upgrade().is_some_and(|pc| Arc::ptr_eq(&pc, &leg.pc))
                {
                    room.remove_leg(&id).await;
                }
            })
        }));
        // The viewer's downlink is what its other connection learnt (a new
        // layer's connection would otherwise start unlimited).
        if let Some(limit) = room.known_capacity(session_of(leg_id)) {
            leg.downlink.lock().inherit(limit);
        }
        let old = room.legs.write().insert(leg_id.to_owned(), leg);
        if let Some(old) = old {
            old.close().await;
        }
        Ok(answer)
    }

    /// Removes leg `leg`, or every leg of session `leg`.
    pub async fn unsubscribe(&self, room: &str, leg: &str) {
        let Some(room) = self.room(room) else { return };
        let ids: Vec<String> = room
            .legs
            .read()
            .keys()
            .filter(|id| *id == leg || session_of(id) == leg)
            .cloned()
            .collect();
        for id in ids {
            room.remove_leg(&id).await;
        }
    }

    pub async fn close_room(&self, room: &str) {
        let room = self.rooms.lock().remove(room);
        if let Some(room) = room {
            room.close().await;
        }
    }

    pub fn rooms(&self) -> Vec<RoomStats> {
        let rooms: Vec<Arc<Room>> = self.rooms.lock().values().cloned().collect();
        let mut stats: Vec<RoomStats> = rooms.iter().map(|r| r.stats()).collect();
        stats.sort_by(|a, b| a.id.cmp(&b.id));
        stats
    }

    /// Runs `request`; the answer for a publication or subscription.
    pub async fn handle(&self, request: SfuRequest) -> Result<String, SfuError> {
        match request {
            SfuRequest::Publish { room, sdp } => self.publish(&room, &sdp).await,
            SfuRequest::Subscribe {
                room,
                leg,
                sdp,
                tracks,
                layer,
            } => {
                if let Some(room) = self.room(&room) {
                    room.choose_layer(session_of(&leg), layer);
                }
                self.subscribe(&room, &leg, &sdp, tracks.as_deref()).await
            }
            SfuRequest::Layer {
                room,
                session,
                layer,
            } => {
                let room = self.room(&room).ok_or(SfuError::NoRoom)?;
                room.choose_layer(&session, layer);
                Ok(String::new())
            }
            SfuRequest::Unsubscribe { room, leg } => {
                self.unsubscribe(&room, &leg).await;
                Ok(String::new())
            }
            SfuRequest::Close { room } => {
                self.close_room(&room).await;
                Ok(String::new())
            }
        }
    }

    pub async fn close(&self) {
        let rooms: Vec<Arc<Room>> = self.rooms.lock().drain().map(|(_, r)| r).collect();
        for room in rooms {
            room.close().await;
        }
    }
}

/// Applies `offer`, answers, and waits for the (local) candidates.
async fn negotiate(pc: &RTCPeerConnection, offer: &str) -> Result<String, SfuError> {
    let offer = RTCSessionDescription::offer(offer.to_owned())
        .map_err(|e| SfuError::Description(e.to_string()))?;
    pc.set_remote_description(offer).await?;
    let answer = pc.create_answer(None).await?;
    let mut gathered = pc.gathering_complete_promise().await;
    pc.set_local_description(answer).await?;
    tokio::time::timeout(GATHER_TIMEOUT, gathered.recv())
        .await
        .map_err(|_| SfuError::Timeout)?;
    Ok(pc
        .local_description()
        .await
        .ok_or(SfuError::Timeout)?
        .sdp)
}
