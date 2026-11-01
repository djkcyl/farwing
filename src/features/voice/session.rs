//! Being in a voice channel or a call (`docs/protocol/chat-design.md`
//! §7, §10): the connections, frame keys and who is there. A channel goes
//! through the forwarder (an uplink this device offers, a downlink the
//! forwarder offers); a call goes straight to the other device, each side
//! signing its description's fingerprints.

use crate::features::host::peer::chat_video::Shown;
use super::engine::Engine;
use super::keys::{DeviceKeys, FRAME_KEY};
use super::settings::VoiceSettings;
use crate::account::client::AuthenticatedClient;
use crate::media::frame_crypto::FrameKey;
use anyhow::{Context, Result, anyhow, bail};
use farwing_protocol::chat::{
    Call, CallSignal, Id, VoiceDescription, VoiceJoined, VoiceKeyPush, VoiceKeys, VoiceMode, VoiceOffer,
    VoiceParticipant, call_signal_message, sdp_fingerprints, track, types,
};
use farwing_protocol::realtime::{Envelope, IceServer};
use reqwest::Method;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MIME_TYPE_OPUS, MediaEngine};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

/// Candidates are gathered before a call's description goes out (relays
/// take a moment).
const GATHER: Duration = Duration::from_secs(3);
/// The forwarder needs none of ours: it answers our checks wherever they
/// come from (peer-reflexive); the host candidates come at once.
const GATHER_FORWARDER: Duration = Duration::from_millis(300);
/// A replaced key is used this long after the new one is handed out.
const KEY_SWITCH: Duration = Duration::from_secs(2);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The relays as webrtc takes them.
fn ice(servers: &[IceServer]) -> Vec<RTCIceServer> {
    servers
        .iter()
        .map(|s| RTCIceServer {
            urls: s.urls.clone(),
            username: s.username.clone(),
            credential: s.credential.clone(),
        })
        .collect()
}

async fn connection(ice: &[IceServer]) -> Result<Arc<RTCPeerConnection>> {
    let mut media = MediaEngine::default();
    farwing_rtc_wire::register_codecs(&mut media)?;
    farwing_rtc_wire::register_header_extensions(&mut media, &[])?;
    let registry = register_default_interceptors(Registry::new(), &mut media)?;
    let mut settings = SettingEngine::default();
    settings.set_receive_mtu(1500);
    let api = APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_setting_engine(settings)
        .build();
    let configuration = RTCConfiguration {
        ice_servers: ice
            .iter()
            .map(|s| RTCIceServer {
                urls: s.urls.clone(),
                username: s.username.clone(),
                credential: s.credential.clone(),
            })
            .collect(),
        ..Default::default()
    };
    Ok(Arc::new(api.new_peer_connection(configuration).await?))
}

/// The far end's receiver reports on the mic's sender steer the Opus rate.
fn follow_loss(sender: Arc<webrtc::rtp_transceiver::rtp_sender::RTCRtpSender>, engine: Engine) {
    tokio::spawn(async move {
        while let Ok((packets, _)) = sender.read_rtcp().await {
            for packet in packets {
                if let Some(rr) = packet
                    .as_any()
                    .downcast_ref::<webrtc::rtcp::receiver_report::ReceiverReport>()
                    && let Some(block) = rr.reports.first()
                {
                    engine.report_loss(block.fraction_lost);
                }
            }
        }
    });
}

fn mic_track() -> Arc<TrackLocalStaticRTP> {
    Arc::new(TrackLocalStaticRTP::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48_000,
            channels: 2,
            sdp_fmtp_line: farwing_rtc_wire::OPUS_FMTP.to_owned(),
            rtcp_feedback: Vec::new(),
        },
        "mic".to_owned(),
        "me".to_owned(),
    ))
}

/// The local description once candidates are in (or after `wait`).
async fn gathered(pc: &RTCPeerConnection, description: RTCSessionDescription, wait: Duration) -> Result<String> {
    let mut done = pc.gathering_complete_promise().await;
    pc.set_local_description(description).await?;
    let _ = tokio::time::timeout(wait, done.recv()).await;
    Ok(pc.local_description().await.context("no local description")?.sdp)
}

/// What a voice connection tells the chat service (and the page).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// The media connections are up (`true`) or lost.
    Connected(bool),
    Failed(String),
}

// ================================================================= channel

pub(crate) struct ChannelVoice {
    pub channel: Id,
    pub participant: u32,
    pub mode: VoiceMode,
    pub engine: Engine,
    client: Arc<AuthenticatedClient>,
    keys: Arc<DeviceKeys>,
    outgoing: mpsc::UnboundedSender<Envelope>,
    ice: Vec<IceServer>,
    uplink: crate::features::host::peer::chat_video::ChatUplink,
    downlink: tokio::sync::Mutex<Option<Arc<RTCPeerConnection>>>,
    /// The share's description in the voice state.
    screen: Mutex<Option<farwing_protocol::chat::VoiceScreen>>,
    /// Self mute and deafen as last told, sent again with the share.
    me: Mutex<(bool, bool)>,
    /// Republishing one at a time.
    publishing: tokio::sync::Mutex<()>,
    /// A task follows the share (`follow_share`).
    pub(crate) following: std::sync::atomic::AtomicBool,
    participants: Mutex<HashMap<u32, VoiceParticipant>>,
    /// (ID, key) of this device's frame key.
    key: Mutex<(u8, [u8; FRAME_KEY])>,
    events: mpsc::UnboundedSender<Event>,
}

impl ChannelVoice {
    /// Joins `channel`; the media connects in the background.
    pub(crate) async fn join(
        client: Arc<AuthenticatedClient>,
        keys: Arc<DeviceKeys>,
        outgoing: mpsc::UnboundedSender<Envelope>,
        channel: Id,
        settings: VoiceSettings,
        muted: bool,
        deafened: bool,
        events: mpsc::UnboundedSender<Event>,
    ) -> Result<Arc<Self>> {
        let joined: VoiceJoined = serde_json::from_value(
            client
                .chat(
                    Method::POST,
                    &format!("/api/v2/chat/channels/{channel}/voice/join"),
                    Some(json!({"self_muted": muted, "self_deafened": deafened})),
                )
                .await?,
        )?;
        // Nothing leaves this device unencrypted when it accepts only
        // end-to-end encryption.
        if joined.mode == VoiceMode::Compatible && crate::transport::nat::settings::current().e2ee_only {
            let _ = client
                .chat(Method::POST, &format!("/api/v2/chat/channels/{channel}/voice/leave"), None)
                .await;
            anyhow::bail!("{}", crate::i18n::t().voice.e2ee_only);
        }
        let engine = Engine::start(settings, &tokio::runtime::Handle::current());
        engine.set_e2ee(joined.mode == VoiceMode::E2ee);
        engine.set_bitrate(joined.bitrate_kbps);
        engine.set_self(muted, deafened);
        let key = (0u8, FrameKey::generate());
        engine.set_key(FrameKey::new(key.0, &key.1)?);
        let cipher = (joined.mode == VoiceMode::E2ee).then(|| engine.cipher()).flatten();
        let uplink = crate::features::host::peer::chat_video::ChatUplink::new(ice(&joined.ice_servers), cipher).await?;
        let voice = Arc::new(Self {
            channel,
            participant: joined.participant,
            mode: joined.mode,
            engine,
            client,
            keys,
            outgoing,
            ice: joined.ice_servers.clone(),
            uplink,
            downlink: tokio::sync::Mutex::new(None),
            screen: Mutex::new(None),
            me: Mutex::new((muted, deafened)),
            publishing: tokio::sync::Mutex::new(()),
            following: std::sync::atomic::AtomicBool::new(false),
            participants: Mutex::new(joined.participants.iter().map(|p| (p.participant, p.clone())).collect()),
            key: Mutex::new(key),
            events,
        });
        let others: Vec<VoiceParticipant> = joined
            .participants
            .into_iter()
            .filter(|p| p.participant != voice.participant)
            .collect();
        tracing::info!(%channel, participant = voice.participant, mode = ?voice.mode, "voice channel joined");
        voice.hand_key(&others).await;
        let v = Arc::clone(&voice);
        tokio::spawn(async move {
            if let Err(e) = v.publish().await {
                tracing::warn!(error = %format!("{e:#}"), "voice uplink failed");
                let _ = v.events.send(Event::Failed(format!("{e:#}")));
            }
        });
        Ok(voice)
    }

    async fn publish(&self) -> Result<()> {
        follow_loss(Arc::clone(&self.uplink.mic_sender), self.engine.clone());
        let events = self.events.clone();
        self.uplink.on_state(move |state| match state {
            RTCPeerConnectionState::Connected => {
                let _ = events.send(Event::Connected(true));
            }
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Disconnected => {
                let _ = events.send(Event::Connected(false));
            }
            _ => {}
        });
        self.republish().await?;
        self.engine.set_track(Some(Arc::clone(&self.uplink.mic)));
        tracing::info!(channel = %self.channel, "voice uplink published");
        Ok(())
    }

    /// Offers the uplink as it is now: the mic, and the pictures and
    /// sound it sends.
    async fn republish(&self) -> Result<()> {
        let _one = self.publishing.lock().await;
        let offer = self.uplink.pc.create_offer(None).await?;
        let sdp = gathered(&self.uplink.pc, offer, GATHER_FORWARDER).await?;
        let camera = self.uplink.camera_on();
        let screen = lock(&self.screen).clone();
        let tracks: Vec<serde_json::Value> = self
            .uplink
            .mids()
            .await
            .into_iter()
            .filter(|(kind, _)| match kind.as_str() {
                track::MIC => true,
                track::CAM_HI | track::CAM_LO => camera,
                track::SCREEN | track::SCREEN_720 => screen.is_some(),
                track::SCREEN_AUDIO => screen.as_ref().is_some_and(|s| s.audio),
                _ => false,
            })
            .map(|(kind, mid)| json!({"mid": mid, "kind": kind}))
            .collect();
        let answer: VoiceDescription = serde_json::from_value(
            self.client
                .chat(
                    Method::PUT,
                    &format!("/api/v2/chat/channels/{}/voice/publish", self.channel),
                    Some(json!({"sdp": sdp, "tracks": tracks})),
                )
                .await?,
        )?;
        self.uplink
            .pc
            .set_remote_description(RTCSessionDescription::answer(answer.sdp)?)
            .await?;
        Ok(())
    }

    /// Turns the camera on (`spec`) or off.
    pub(crate) async fn set_camera(&self, spec: Option<crate::media::camera::Spec>) -> Result<()> {
        match spec {
            Some(spec) => self.uplink.start_camera(spec),
            None => self.uplink.stop_camera(),
        }
        self.republish().await
    }

    /// Shares `shown` at `quality` (with its sound or not), or stops.
    pub(crate) async fn set_screen(
        &self,
        share: Option<(Shown, crate::features::host::peer::chat_video::Quality, bool)>,
    ) -> Result<()> {
        let description = match share {
            Some((shown, quality, audio)) => {
                let audio = if self.uplink.shown().as_ref() == Some(&shown) {
                    self.uplink.set_quality(quality);
                    self.uplink.set_sound(audio)
                } else {
                    self.uplink.start_screen(shown.clone(), quality, audio)
                };
                Some(farwing_protocol::chat::VoiceScreen {
                    quality: quality.name().to_owned(),
                    audio,
                    whole: matches!(shown, Shown::Screen(_)),
                    minimized: false,
                })
            }
            None => {
                self.uplink.stop_screen();
                None
            }
        };
        *lock(&self.screen) = description;
        self.republish().await?;
        self.tell_state().await;
        Ok(())
    }

    /// Follows the share's capture: tells a minimized window, stops the
    /// share when its capture ended. False once nothing is shared.
    pub(crate) async fn follow_share(&self) -> bool {
        let Some((minimized, ended)) = self.uplink.share_state() else {
            return false;
        };
        if ended {
            let _ = self.set_screen(None).await;
            return false;
        }
        let changed = match lock(&self.screen).as_mut() {
            Some(screen) if screen.minimized != minimized => {
                screen.minimized = minimized;
                true
            }
            _ => false,
        };
        if changed {
            self.tell_state().await;
        }
        true
    }

    /// Someone watches the share's 720p30 copy (`voice.demand`).
    pub(crate) fn demand(&self, kinds: &[String]) {
        self.uplink.set_copy(kinds.iter().any(|k| k == track::SCREEN_720));
    }

    pub(crate) fn camera_on(&self) -> bool {
        self.uplink.camera_on()
    }

    pub(crate) fn screen(&self) -> Option<farwing_protocol::chat::VoiceScreen> {
        lock(&self.screen).clone()
    }

    /// The voice state this device describes: mute, deafen, the share.
    async fn tell_state(&self) {
        let (muted, deafened) = *lock(&self.me);
        let screen = lock(&self.screen).clone();
        let _ = self
            .client
            .chat(
                Method::PUT,
                &format!("/api/v2/chat/channels/{}/voice/state", self.channel),
                Some(json!({"self_muted": muted, "self_deafened": deafened, "screen": screen})),
            )
            .await;
    }

    /// This device's key, sealed to each of `to`.
    async fn hand_key(&self, to: &[VoiceParticipant]) {
        if self.mode != VoiceMode::E2ee || to.is_empty() {
            return;
        }
        let (id, key) = *lock(&self.key);
        let scope = self.channel.to_string();
        let boxes: Vec<_> = to
            .iter()
            .filter(|p| p.device_id != self.keys.device_id())
            .filter_map(|p| self.keys.seal(&scope, self.participant, &p.device_id, &p.x25519, id, &key).ok())
            .collect();
        if boxes.is_empty() {
            return;
        }
        let body = VoiceKeys { key_id: id, boxes };
        if let Err(e) = self
            .client
            .chat(
                Method::POST,
                &format!("/api/v2/chat/channels/{}/voice/keys", self.channel),
                Some(serde_json::to_value(&body).unwrap_or_default()),
            )
            .await
        {
            tracing::warn!(error = %format!("{e:#}"), "voice keys not handed out");
        }
    }

    /// The forwarder's next downlink description.
    pub(crate) async fn offer(self: &Arc<Self>, offer: VoiceOffer) {
        if let Err(e) = self.answer(offer).await {
            tracing::warn!(error = %format!("{e:#}"), "voice downlink offer not answered");
        }
    }

    async fn answer(self: &Arc<Self>, offer: VoiceOffer) -> Result<()> {
        let mut downlink = self.downlink.lock().await;
        if downlink.is_none() {
            let pc = connection(&self.ice).await?;
            let weak = Arc::downgrade(self);
            pc.on_track(Box::new(move |track, _, _| {
                if let Some(v) = weak.upgrade() {
                    let stream = track.stream_id();
                    let participant = farwing_protocol::chat::track::parse(&stream)
                        .filter(|(_, kind)| *kind == farwing_protocol::chat::track::MIC)
                        .map(|(p, _)| p);
                    if let Some(p) = participant {
                        let user = lock(&v.participants).get(&p).map(|x| x.user_id.clone()).unwrap_or_default();
                        tracing::info!(participant = p, "voice track");
                        v.engine.receive(p, &user, track);
                    }
                }
                Box::pin(async {})
            }));
            *downlink = Some(pc);
        }
        let pc = downlink.as_ref().expect("set above");
        pc.set_remote_description(RTCSessionDescription::offer(offer.sdp)?).await?;
        let answer = pc.create_answer(None).await?;
        let sdp = gathered(pc, answer, GATHER_FORWARDER).await?;
        let reply = VoiceOffer {
            channel_id: self.channel,
            generation: offer.generation,
            sdp,
        };
        self.outgoing
            .send(Envelope::new(types::VOICE_ANSWER, reply))
            .map_err(|_| anyhow!("chat connection closed"))?;
        tracing::info!(generation = offer.generation, "voice downlink answered");
        Ok(())
    }

    /// Who is in the channel now: new people get this device's key; those
    /// who left take their sounds with them and the key changes.
    pub(crate) async fn state(&self, participants: Vec<VoiceParticipant>) {
        let (joined, left) = {
            let mut current = lock(&self.participants);
            let next: HashMap<u32, VoiceParticipant> = participants.into_iter().map(|p| (p.participant, p)).collect();
            let joined: Vec<VoiceParticipant> =
                next.values().filter(|p| !current.contains_key(&p.participant)).cloned().collect();
            let left: Vec<u32> = current.keys().filter(|p| !next.contains_key(p)).copied().collect();
            if let Some(me) = next.get(&self.participant) {
                self.engine.set_server(me.server_muted, me.server_deafened);
            }
            *current = next;
            (joined, left)
        };
        for p in &left {
            self.engine.forget(*p);
        }
        if !left.is_empty() && self.mode == VoiceMode::E2ee {
            // Those who left must not hear what comes next.
            let next = {
                let mut key = lock(&self.key);
                *key = (key.0.wrapping_add(1), FrameKey::generate());
                *key
            };
            let everyone: Vec<VoiceParticipant> = lock(&self.participants)
                .values()
                .filter(|p| p.participant != self.participant)
                .cloned()
                .collect();
            self.hand_key(&everyone).await;
            let engine = self.engine.clone();
            tokio::spawn(async move {
                tokio::time::sleep(KEY_SWITCH).await;
                if let Ok(k) = FrameKey::new(next.0, &next.1) {
                    engine.set_key(k);
                }
            });
        } else if !joined.is_empty() {
            self.hand_key(&joined).await;
        }
    }

    /// A participant's frame key for this device.
    pub(crate) fn key(&self, push: &VoiceKeyPush) {
        let sender = lock(&self.participants).get(&push.participant).map(|p| p.ed25519.clone());
        let Some(sender) = sender else { return };
        match self.keys.open(&self.channel.to_string(), push, &sender) {
            Ok((key, id)) => {
                if let Ok(k) = FrameKey::new(id, &key) {
                    self.engine.add_key(push.participant, k);
                }
            }
            Err(e) => tracing::warn!(error = %format!("{e:#}"), participant = push.participant, "voice key refused"),
        }
    }

    pub(crate) async fn set_self(&self, muted: bool, deafened: bool) {
        self.engine.set_self(muted, deafened);
        *lock(&self.me) = (muted, deafened);
        self.tell_state().await;
    }

    /// Leaves (`tell`: the server does not know yet).
    pub(crate) async fn close(&self, tell: bool) {
        if tell {
            let _ = self
                .client
                .chat(Method::POST, &format!("/api/v2/chat/channels/{}/voice/leave", self.channel), None)
                .await;
        }
        self.engine.set_track(None);
        self.uplink.close().await;
        if let Some(pc) = self.downlink.lock().await.take() {
            let _ = pc.close().await;
        }
        self.engine.close().await;
    }
}

// ==================================================================== call

pub(crate) struct CallVoice {
    pub call: Mutex<Call>,
    pub engine: Engine,
    client: Arc<AuthenticatedClient>,
    identity: crate::account::api::ClientIdentity,
    outgoing: mpsc::UnboundedSender<Envelope>,
    pc: tokio::sync::Mutex<Option<Arc<RTCPeerConnection>>>,
    events: mpsc::UnboundedSender<Event>,
}

impl CallVoice {
    /// A call this device makes (`call` as the server answered) or
    /// answers.
    pub(crate) fn new(
        client: Arc<AuthenticatedClient>,
        outgoing: mpsc::UnboundedSender<Envelope>,
        call: Call,
        settings: VoiceSettings,
        events: mpsc::UnboundedSender<Event>,
    ) -> Result<Arc<Self>> {
        let identity = client.device_identity().context("no device identity")?;
        let engine = Engine::start(settings, &tokio::runtime::Handle::current());
        // The media goes between the two devices: its own encryption is
        // end to end (the fingerprints are signed).
        engine.set_e2ee(false);
        engine.set_bitrate(64);
        Ok(Arc::new(Self {
            call: Mutex::new(call),
            engine,
            client,
            identity,
            outgoing,
            pc: tokio::sync::Mutex::new(None),
            events,
        }))
    }

    pub(crate) fn id(&self) -> String {
        lock(&self.call).id.clone()
    }

    fn i_called(&self) -> bool {
        lock(&self.call).caller_device == self.identity.device_id
    }

    /// The other device's key.
    fn peer_key(&self) -> String {
        let call = lock(&self.call);
        if call.caller_device == self.identity.device_id {
            call.callee_key.clone()
        } else {
            call.caller_key.clone()
        }
    }

    fn peer_user(&self) -> String {
        let call = lock(&self.call);
        if call.caller_device == self.identity.device_id {
            call.callee.user_id.clone()
        } else {
            call.caller.user_id.clone()
        }
    }

    async fn peer(self: &Arc<Self>) -> Result<Arc<RTCPeerConnection>> {
        let mut slot = self.pc.lock().await;
        if let Some(pc) = slot.as_ref() {
            return Ok(Arc::clone(pc));
        }
        let ice = lock(&self.call).ice_servers.clone();
        let pc = connection(&ice).await?;
        let track = mic_track();
        let transceiver = pc
            .add_transceiver_from_track(
                Arc::clone(&track) as _,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Sendrecv,
                    send_encodings: Vec::new(),
                }),
            )
            .await?;
        follow_loss(transceiver.sender().await, self.engine.clone());
        self.engine.set_track(Some(track));
        let weak = Arc::downgrade(self);
        pc.on_track(Box::new(move |track, _, _| {
            if let Some(v) = weak.upgrade() {
                let user = v.peer_user();
                v.engine.receive(1, &user, track);
            }
            Box::pin(async {})
        }));
        let events = self.events.clone();
        pc.on_peer_connection_state_change(Box::new(move |state| {
            match state {
                RTCPeerConnectionState::Connected => {
                    let _ = events.send(Event::Connected(true));
                }
                RTCPeerConnectionState::Failed => {
                    let _ = events.send(Event::Connected(false));
                }
                _ => {}
            }
            Box::pin(async {})
        }));
        *slot = Some(Arc::clone(&pc));
        Ok(pc)
    }

    fn send_signal(&self, kind: &str, sdp: String) -> Result<()> {
        let id = self.id();
        let signature = self
            .identity
            .sign(call_signal_message(&id, &sdp_fingerprints(&sdp)).as_bytes());
        let body = CallSignal {
            call_id: id,
            kind: kind.to_owned(),
            data: sdp,
            signature,
            from_device: String::new(),
        };
        self.outgoing
            .send(Envelope::new(types::CALL_SIGNAL, body))
            .map_err(|_| anyhow!("chat connection closed"))
    }

    /// The call is answered (or moved): the caller offers.
    pub(crate) async fn answered(self: &Arc<Self>, call: Call) {
        let fresh = lock(&self.call).callee_device != call.callee_device || lock(&self.call).caller_device != call.caller_device;
        *lock(&self.call) = call;
        if fresh && let Some(old) = self.pc.lock().await.take() {
            let _ = old.close().await;
        }
        if !self.i_called() {
            return;
        }
        let result = async {
            let pc = self.peer().await?;
            let offer = pc.create_offer(None).await?;
            let sdp = gathered(&pc, offer, GATHER).await?;
            self.send_signal("offer", sdp)
        }
        .await;
        if let Err(e) = result {
            let _ = self.events.send(Event::Failed(format!("{e:#}")));
        }
    }

    /// The other device's description: checked against its key first.
    pub(crate) async fn signal(self: &Arc<Self>, signal: CallSignal) {
        let result = async {
            if signal.kind == "candidate" {
                return Ok(());
            }
            let peer = self.peer_key();
            farwing_protocol::crypto::verify(
                &peer,
                call_signal_message(&signal.call_id, &sdp_fingerprints(&signal.data)).as_bytes(),
                &signal.signature,
            )
            .map_err(|_| anyhow!("{}", crate::i18n::t().voice.call_unverified))?;
            let pc = self.peer().await?;
            match signal.kind.as_str() {
                "offer" => {
                    pc.set_remote_description(RTCSessionDescription::offer(signal.data)?).await?;
                    let answer = pc.create_answer(None).await?;
                    let sdp = gathered(&pc, answer, GATHER).await?;
                    self.send_signal("answer", sdp)?;
                }
                "answer" => {
                    pc.set_remote_description(RTCSessionDescription::answer(signal.data)?).await?;
                }
                other => bail!("unknown call signal {other}"),
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(e) = result {
            tracing::warn!(error = %format!("{e:#}"), "call signal refused");
            let _ = self.events.send(Event::Failed(format!("{e:#}")));
        }
    }

    pub(crate) async fn hang_up(&self) {
        let id = self.id();
        let _ = self.client.chat(Method::POST, &format!("/api/v2/chat/calls/{id}/hangup"), None).await;
    }

    pub(crate) async fn close(&self) {
        self.engine.set_track(None);
        if let Some(pc) = self.pc.lock().await.take() {
            let _ = pc.close().await;
        }
        self.engine.close().await;
    }
}
