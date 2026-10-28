//! Receiving the host's picture through the server's forwarder
//! (`docs/protocol/deployment-design.md` §5.4). When the host says so, a
//! second connection — to the forwarder, receive-only — carries the video
//! tracks; their frames go into the same receivers and decoders as the
//! host's own tracks, which send nothing meanwhile. Audio, input and every
//! other channel stay on the host connection.

use super::clock::{RemoteNtpEstimator, observe_remote_ntp};
use super::core::ConnectionCore;
use super::feedback::send_picture_loss_indication;
use super::peer::{IceServer, ReceiveParameters};
use super::receive::{TrackForwardContext, forward_remote_track};
use super::tracks::{MediaKind, VideoTrackRegistry};
use super::workers::{SessionWorkers, std_mutex_lock};
use crate::account::api::ServerApi;
use crate::features::stream_control::StreamControlHandle;
use crate::features::stream_control::wire::{ForwardStatus, MediaForward};
use crate::media::frame_crypto::{Decryptor, FrameKey};
use crate::transport::rtcp_timing::RtcpTiming;
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::configure_twcc_receiver_with_builder;
use webrtc::api::media_engine::MediaEngine;
use webrtc::interceptor::registry::Registry;
use webrtc::interceptor::report::receiver::ReceiverReport;
use webrtc::interceptor::twcc::receiver::Receiver as TransportFeedbackReceiver;
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;

/// The forwarder connection must carry media within this.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The viewer's choice of stream is checked this often.
const CHOICE_CHECK: Duration = Duration::from_millis(500);
/// The longest the offer waits for further candidates.
const GATHER_WAIT: Duration = Duration::from_millis(300);
/// After the first host candidate, the others (more interfaces) come within
/// this.
const HOST_SIBLINGS: Duration = Duration::from_millis(20);
/// After the first track's picture came through the forwarder, the others
/// may take this long before joining counts as done.
const OTHER_TRACKS_WAIT: Duration = Duration::from_secs(1);
/// Without a keyframe through the forwarder this long, the joining viewer
/// asks again.
const KEYFRAME_AGAIN: Duration = Duration::from_millis(600);
/// A room left plays on at most this long until the host's picture is back.
const LEAVE_WAIT: Duration = Duration::from_secs(3);

/// The pictures of a forwarded room's tracks (`(index, track)`) this viewer
/// has.
fn pictures(context: &HostSide, tracks: &[(u32, String)]) -> Vec<Arc<super::tracks::PicturePath>> {
    tracks
        .iter()
        .filter_map(|(index, _)| context.tracks.get(*index as i32))
        .map(|source| source.picture.clone())
        .collect()
}

/// The forwarder's stream this viewer chose (`deployment-design.md` §5.4):
/// 流畅 is its low layer, another manual tier its main one.
fn chosen_layer(control: &StreamControlHandle) -> farwing_protocol::api::ForwardLayer {
    use crate::features::stream_control::StreamQuality;
    use farwing_protocol::api::ForwardLayer;
    match control.snapshot().settings.quality {
        StreamQuality::Auto => ForwardLayer::Auto,
        StreamQuality::Fast => ForwardLayer::Low,
        _ => ForwardLayer::Main,
    }
}

/// The pictures of `tracks` once the host connection has them all: an
/// instruction may come just before the host's track does.
async fn arrived_pictures(
    context: &HostSide,
    tracks: &[(u32, String)],
) -> Result<Vec<Arc<super::tracks::PicturePath>>> {
    tokio::time::timeout(CONNECT_TIMEOUT, async {
        loop {
            let changed = context.tracks.changed.notified();
            let found = pictures(context, tracks);
            if found.len() == tracks.len() {
                return found;
            }
            changed.await;
        }
    })
    .await
    .context("the host's video tracks did not arrive")
}

/// Until source `source`'s frames are shown (0: the host's).
async fn shown(picture: &super::tracks::PicturePath, source: u64) {
    let mut changes = picture.watch();
    while picture.shown() != source {
        if changes.changed().await.is_err() {
            return;
        }
    }
}

/// What a forwarded connection needs from the host connection.
#[derive(Clone)]
pub(super) struct HostSide {
    pub workers: Weak<SessionWorkers>,
    pub stop: CancellationToken,
    pub ice_servers: Vec<IceServer>,
    pub tracks: VideoTrackRegistry,
    pub stream_control: StreamControlHandle,
    /// Round trip to the host measured on the host connection.
    pub rtt_micros: Arc<AtomicU64>,
}

struct Active {
    /// This viewer's connection number (from 1): its leg on the forwarder
    /// and the picture's source.
    number: u32,
    room: String,
    tracks: Vec<(u32, String)>,
    /// The forwarder's sound track; empty when the sound stays direct.
    audio: String,
    /// End-to-end mode: the frame keys.
    decrypt: Option<Arc<StdMutex<Decryptor>>>,
    connection: Arc<RTCPeerConnection>,
    stop: CancellationToken,
    /// Forwarder SSRC of each forwarded track, by this viewer's track index.
    ssrcs: Arc<StdMutex<BTreeMap<i32, u32>>>,
}

/// `connection` reaches the forwarder from one of this computer's
/// addresses, not through a relay.
async fn reached_without_relay(connection: &RTCPeerConnection) -> bool {
    connection
        .sctp()
        .transport()
        .ice_transport()
        .get_selected_candidate_pair()
        .await
        .is_some_and(|pair| pair.local.typ != RTCIceCandidateType::Relay)
}

/// The forwarded part of a viewer connection.
#[derive(Default)]
pub(crate) struct Forwarding {
    active: StdMutex<Option<Active>>,
    /// Connections opened so far.
    opened: std::sync::atomic::AtomicU32,
}

impl Forwarding {
    /// Where a keyframe request for the track at `index` goes while it is
    /// forwarded: the forwarder connection and the track's SSRC there.
    pub(super) fn keyframe_target(&self, index: i32) -> Option<(Arc<RTCPeerConnection>, u32)> {
        let active = std_mutex_lock(&self.active);
        let active = active.as_ref()?;
        let ssrc = *std_mutex_lock(&active.ssrcs).get(&index)?;
        Some((active.connection.clone(), ssrc))
    }

    fn take(&self) -> Option<Active> {
        std_mutex_lock(&self.active).take()
    }

    /// Follows the host's instructions until the host connection ends.
    pub(super) async fn run(self: Arc<Self>, context: HostSide, api: ServerApi, session: String) {
        let mut instructions = context.stream_control.forward_instructions();
        tracing::info!("following the host's forwarding instructions");
        // The viewer's choice of stream follows its quality setting.
        let chooser = {
            let this = Arc::clone(&self);
            let (control, api, session) = (context.stream_control.clone(), api.clone(), session.clone());
            let stop = context.stop.clone();
            tokio::spawn(async move {
                let mut told: Option<(String, farwing_protocol::api::ForwardLayer)> = None;
                loop {
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        _ = tokio::time::sleep(CHOICE_CHECK) => {}
                    }
                    let room = std_mutex_lock(&this.active).as_ref().map(|a| a.room.clone());
                    let Some(room) = room else {
                        told = None;
                        continue;
                    };
                    let now = Some((room.clone(), chosen_layer(&control)));
                    if told == now {
                        continue;
                    }
                    let layer = now.as_ref().map(|(_, layer)| *layer).unwrap_or_default();
                    match api.sfu_layer(&room, &session, layer).await {
                        Ok(()) => {
                            tracing::info!(?layer, "the forwarder knows this viewer's choice of stream");
                            told = now;
                        }
                        Err(error) => tracing::debug!(error = %format!("{error:#}"), "choice of stream not sent"),
                    }
                }
            })
        };
        // The host repeats its last instruction on a new route.
        let mut handled = None;
        loop {
            tokio::select! {
                _ = context.stop.cancelled() => break,
                changed = instructions.changed() => if changed.is_err() { break },
            }
            let Some(instruction) = instructions.borrow_and_update().clone() else {
                continue;
            };
            if handled.replace(instruction.generation) == Some(instruction.generation) {
                continue;
            }
            // The same room and tracks with a new frame key: no new connection.
            if let Some(key) = self.rekey(&instruction) {
                let status = match key {
                    Ok(()) => ForwardStatus {
                        generation: instruction.generation,
                        receiving: true,
                        error: String::new(),
                    },
                    Err(error) => ForwardStatus {
                        generation: instruction.generation,
                        receiving: false,
                        error: format!("{error:#}"),
                    },
                };
                context.stream_control.forward_status(status);
                continue;
            }
            // Other tracks of the same room (another layer, another screen):
            // the old connection plays on until the new one's picture takes
            // over.
            let (replaced, left) = match self.take() {
                Some(old) if old.room == instruction.room => (Some(old), None),
                old => (None, old),
            };
            if let Some(old) = left {
                // The forwarder's picture stays until the host's is back.
                let pictures = pictures(&context, &old.tracks);
                for picture in &pictures {
                    picture.want(0);
                }
                let (api, session) = (api.clone(), session.clone());
                tokio::spawn(async move {
                    let back = futures_util::future::join_all(
                        pictures.iter().map(|picture| shown(picture, 0)),
                    );
                    let _ = tokio::time::timeout(LEAVE_WAIT, back).await;
                    old.stop.cancel();
                    let _ = old.connection.close().await;
                    let _ = api.sfu_unsubscribe(&old.room, &session, old.number).await;
                    tracing::info!(room = %old.room, "left the forwarding room");
                });
            }
            if instruction.room.is_empty() {
                tracing::info!("the host sends the picture directly again");
                continue;
            }
            // A newer instruction ends the wait for this room.
            let mut newer = instructions.clone();
            let reached = match &replaced {
                Some(old) => reached_without_relay(&old.connection).await,
                None => false,
            };
            // The keys the room's frames are sealed with so far: a key
            // change may still be on its way to the frames.
            let keys = replaced
                .as_ref()
                .and_then(|old| old.decrypt.as_ref())
                .map(|decrypt| std_mutex_lock(decrypt).clone());
            let status = match self.join(&context, &api, &session, &instruction, reached, keys, &mut newer).await {
                Ok(()) => ForwardStatus {
                    generation: instruction.generation,
                    receiving: true,
                    error: String::new(),
                },
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), room = %instruction.room,
                        "cannot receive through the forwarder");
                    let tracks: Vec<(u32, String)> =
                        instruction.tracks.iter().map(|t| (t.index, t.track.clone())).collect();
                    if let Some(old) = self.take() {
                        old.stop.cancel();
                        let _ = old.connection.close().await;
                        let _ = api.sfu_unsubscribe(&old.room, &session, old.number).await;
                    }
                    // The picture stays with what showed it, the host's
                    // stream or the room's old connection.
                    let before = replaced.as_ref().map_or(0, |old| u64::from(old.number));
                    for picture in pictures(&context, &tracks) {
                        picture.want(before);
                    }
                    ForwardStatus {
                        generation: instruction.generation,
                        receiving: false,
                        error: format!("{error:#}"),
                    }
                }
            };
            if let Some(old) = replaced {
                if status.receiving {
                    old.stop.cancel();
                    let _ = old.connection.close().await;
                    let _ = api.sfu_unsubscribe(&old.room, &session, old.number).await;
                } else {
                    // The host sends directly again; the old connection is
                    // left once its picture is back.
                    *std_mutex_lock(&self.active) = Some(old);
                }
            }
            context.stream_control.forward_status(status);
        }
        if let Some(old) = self.take() {
            old.stop.cancel();
            let _ = old.connection.close().await;
        }
        chooser.abort();
    }

    /// `Some` when `instruction` only changes the frame key of the active
    /// room.
    fn rekey(&self, instruction: &MediaForward) -> Option<Result<()>> {
        let active = std_mutex_lock(&self.active);
        let active = active.as_ref()?;
        let tracks: Vec<(u32, String)> = instruction
            .tracks
            .iter()
            .map(|t| (t.index, t.track.clone()))
            .collect();
        if instruction.room != active.room
            || tracks != active.tracks
            || instruction.audio != active.audio
        {
            return None;
        }
        let decrypt = active.decrypt.as_ref()?;
        Some(
            FrameKey::new(instruction.key_id as u8, &instruction.key).map(|key| {
                tracing::info!(key = instruction.key_id, "new forwarded frame key");
                std_mutex_lock(decrypt).add_key(key);
            }),
        )
    }

    async fn join(
        &self,
        context: &HostSide,
        api: &ServerApi,
        session: &str,
        instruction: &MediaForward,
        reached: bool,
        keys: Option<Decryptor>,
        newer: &mut watch::Receiver<Option<MediaForward>>,
    ) -> Result<()> {
        // The forwarder's frames take over at their first keyframe.
        let tracks: Vec<(u32, String)> =
            instruction.tracks.iter().map(|t| (t.index, t.track.clone())).collect();
        let number = self.opened.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let source = u64::from(number);
        let pictures = arrived_pictures(context, &tracks).await?;
        for picture in &pictures {
            picture.want(source);
        }
        let mut media = MediaEngine::default();
        super::negotiation::register_codecs(&mut media)?;
        super::negotiation::register_header_extensions(&mut media)?;
        let rtcp_timing = RtcpTiming::new();
        let mut registry = Registry::new();
        registry.add(Box::new(rtcp_timing.clone()));
        registry.add(Box::new(
            ReceiverReport::builder()
                .with_jittered_media_intervals(Duration::from_secs(1), Duration::from_secs(5)),
        ));
        let registry = configure_twcc_receiver_with_builder(
            registry,
            &mut media,
            TransportFeedbackReceiver::builder().with_interval(Duration::from_millis(50)),
        )?;
        let api_rtc = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(ConnectionCore::settings())
            .build();
        let connection = Arc::new(
            api_rtc
                .new_peer_connection(RTCConfiguration {
                    ice_servers: context.ice_servers.iter().cloned().map(Into::into).collect(),
                    ..Default::default()
                })
                .await
                .context("create forwarder connection")?,
        );
        for _ in &instruction.tracks {
            connection
                .add_transceiver_from_kind(
                    RTPCodecType::Video,
                    Some(RTCRtpTransceiverInit {
                        direction: RTCRtpTransceiverDirection::Recvonly,
                        send_encodings: Vec::new(),
                    }),
                )
                .await?;
        }
        if !instruction.audio.is_empty() {
            connection
                .add_transceiver_from_kind(
                    RTPCodecType::Audio,
                    Some(RTCRtpTransceiverInit {
                        direction: RTCRtpTransceiverDirection::Recvonly,
                        send_encodings: Vec::new(),
                    }),
                )
                .await?;
        }
        let stop = context.stop.child_token();
        let ssrcs = Arc::new(StdMutex::new(BTreeMap::new()));
        let ssrcs_wanted = ssrcs.clone();
        let decrypt = match instruction.mode.as_str() {
            "e2ee" => {
                let mut keys = keys.unwrap_or_default();
                keys.add_key(FrameKey::new(instruction.key_id as u8, &instruction.key)?);
                Some(Arc::new(StdMutex::new(keys)))
            }
            "compatible" => None,
            other => bail!("unknown forwarding mode {other}"),
        };
        // Forwarder track ID → this viewer's track index.
        let indexes: BTreeMap<String, i32> = instruction
            .tracks
            .iter()
            .map(|t| (t.track.clone(), t.index as i32))
            .collect();
        let (state_tx, mut state) = watch::channel(RTCPeerConnectionState::New);
        connection.on_peer_connection_state_change(Box::new(move |s| {
            tracing::info!(state = ?s, "forwarder connection state changed");
            let _ = state_tx.send(s);
            Box::pin(async {})
        }));
        let (first_tx, mut first) = watch::channel(false);
        {
            let context = context.clone();
            let stop = stop.clone();
            let ssrcs = ssrcs.clone();
            let decrypt = decrypt.clone();
            let weak = Arc::downgrade(&connection);
            connection.on_track(Box::new(move |track, receiver, _| {
                if track.kind() == RTPCodecType::Audio {
                    if let Some(owner) = context.workers.upgrade() {
                        let _ = owner.spawn_in(
                            stop.clone(),
                            play_forwarded_audio(track, context.stream_control.audio(), decrypt.clone()),
                        );
                    }
                    return Box::pin(async {});
                }
                let Some(index) = indexes.get(&track.id()).copied() else {
                    tracing::debug!(track = %track.id(), "unexpected forwarded track");
                    return Box::pin(async {});
                };
                let Some(track_source) = context.tracks.get(index) else {
                    tracing::warn!(index, "forwarded track for a track this viewer does not have");
                    return Box::pin(async {});
                };
                let (Some(owner), Some(connection)) = (context.workers.upgrade(), weak.upgrade()) else {
                    return Box::pin(async {});
                };
                std_mutex_lock(&ssrcs).insert(index, track.ssrc());
                let _ = first_tx.send(true);
                let stop = stop.clone();
                let context = context.clone();
                let rtcp_timing = rtcp_timing.clone();
                let decrypt = decrypt.clone();
                let _ = owner.spawn_in(stop.clone(), async move {
                    let parameters = ReceiveParameters::of(&receiver.get_parameters().await);
                    let codec = track.codec();
                    let remote_ntp =
                        Arc::new(StdMutex::new(RemoteNtpEstimator::new(Instant::now())));
                    let (feedback, receiver_feedback) = tokio::sync::mpsc::unbounded_channel();
                    let Some(owner) = context.workers.upgrade() else { return };
                    // The forwarder does not answer for the host's clock: the
                    // host connection's round trip stands in for this path's.
                    let (assumed, rtt, ssrc) =
                        (rtcp_timing.clone(), context.rtt_micros.clone(), track.ssrc());
                    let _ = owner.spawn_in(stop.clone(), async move {
                        let mut ticks = tokio::time::interval(Duration::from_secs(1));
                        loop {
                            ticks.tick().await;
                            assumed.assume_rtt(
                                ssrc,
                                Duration::from_micros(rtt.load(std::sync::atomic::Ordering::Relaxed)),
                            );
                        }
                    });
                    let _ = owner.spawn_in(
                        stop.clone(),
                        observe_remote_ntp(
                            Arc::clone(&receiver),
                            track.ssrc(),
                            Arc::clone(&remote_ntp),
                            rtcp_timing,
                        ),
                    );
                    tracing::info!(index, ssrc = track.ssrc(), codec = %codec.capability.mime_type,
                        "forwarded video track selected");
                    // The decoder asks the host connection; this connection
                    // asks the forwarder, which passes it on.
                    let _ = send_picture_loss_indication(&connection, track.ssrc()).await;
                    forward_remote_track(
                        track,
                        track_source.started.subscribe(),
                        track_source.keyframe_tx.clone(),
                        TrackForwardContext {
                            forwarded: true,
                            workers: context.workers.clone(),
                            stop,
                            kind: MediaKind::Video,
                            codec: codec.capability.mime_type,
                            codec_fmtp: codec.capability.sdp_fmtp_line,
                            video_payload_codecs: parameters.video_payload_codecs,
                            rtx_payload_apt: parameters.rtx_payload_apt,
                            rsfec_config: parameters.rsfec_config,
                            extmap_allow_mixed: false,
                            audio: context.stream_control.audio(),
                            audio_generation: None,
                            connection,
                            video_annexb_sinks: Arc::clone(&track_source.sinks),
                            performance: track_source.performance.clone(),
                            // The connection's measured round trip (kept current
                            // by its statistics), not a fixed default.
                            nack_rtt_micros: context.rtt_micros.clone(),
                            remote_ntp,
                            receiver_feedback: Some(receiver_feedback),
                            receiver_feedback_sender: feedback,
                            stream_control: context.stream_control.clone(),
                            decrypt,
                            picture: track_source.picture.clone(),
                            source,
                        },
                    )
                    .await;
                });
                Box::pin(async {})
            }));
        }
        *std_mutex_lock(&self.active) = Some(Active {
            number,
            room: instruction.room.clone(),
            audio: instruction.audio.clone(),
            tracks: instruction
                .tracks
                .iter()
                .map(|t| (t.index, t.track.clone()))
                .collect(),
            decrypt,
            connection: connection.clone(),
            stop,
            ssrcs,
        });
        let host = Arc::new(tokio::sync::Notify::new());
        if reached {
            let host = Arc::clone(&host);
            connection.on_ice_candidate(Box::new(move |candidate| {
                if candidate.is_some_and(|c| c.typ == RTCIceCandidateType::Host) {
                    host.notify_one();
                }
                Box::pin(async {})
            }));
        }
        let offer = connection.create_offer(None).await?;
        let mut gathered = connection.gathering_complete_promise().await;
        connection.set_local_description(offer).await?;
        // The forwarder answers checks from any address: host candidates
        // suffice, relayed ones need not be waited for (an unreachable
        // server would hold the offer back; without UDP to the forwarder the
        // join fails and the host sends directly). When the room's old
        // connection reached it without a relay, so will a host candidate.
        if reached {
            let _ = tokio::time::timeout(GATHER_WAIT, async {
                host.notified().await;
                tokio::time::sleep(HOST_SIBLINGS).await;
            })
            .await;
        } else {
            let _ = tokio::time::timeout(GATHER_WAIT, gathered.recv()).await;
        }
        let offer = connection
            .local_description()
            .await
            .context("missing forwarder offer")?
            .sdp;
        let tracks = instruction
            .tracks
            .iter()
            .map(|t| t.track.clone())
            .chain((!instruction.audio.is_empty()).then(|| instruction.audio.clone()))
            .collect();
        let subscription = api
            .sfu_subscribe(&instruction.room, session, number, offer, Some(tracks), chosen_layer(&context.stream_control))
            .await
            .context("subscribe to the forwarding room")?;
        connection
            .set_remote_description(RTCSessionDescription::answer(subscription.answer)?)
            .await
            .context("install the forwarder's answer")?;
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, async {
            let mut switched_at = None::<Instant>;
            let mut asked_at = None::<Instant>;
            loop {
                // The keyframe asked for when the connection came up may have
                // gone by just before it: ask again while none came through.
                let up = *first.borrow() && *state.borrow() == RTCPeerConnectionState::Connected;
                let since = asked_at.get_or_insert_with(Instant::now);
                if !up {
                    *since = Instant::now();
                } else if since.elapsed() >= KEYFRAME_AGAIN
                    && pictures.iter().any(|p| p.shown() != source)
                {
                    *since = Instant::now();
                    let ssrcs: Vec<u32> = std_mutex_lock(&ssrcs_wanted).values().copied().collect();
                    for ssrc in ssrcs {
                        tracing::debug!(ssrc, "no keyframe through the forwarder yet; asking again");
                        let _ = send_picture_loss_indication(&connection, ssrc).await;
                    }
                }
                // Done once the pictures come through the forwarder (the
                // host's stream stops then); a track may lag a little.
                let switched = pictures.iter().filter(|p| p.shown() == source).count();
                if switched > 0 {
                    switched_at.get_or_insert_with(Instant::now);
                }
                let pictures_done = switched == pictures.len()
                    || switched_at.is_some_and(|at| at.elapsed() >= OTHER_TRACKS_WAIT);
                if *state.borrow_and_update() == RTCPeerConnectionState::Connected
                    && *first.borrow_and_update()
                    && pictures_done
                {
                    return Ok(());
                }
                if matches!(
                    *state.borrow(),
                    RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
                ) {
                    bail!("the forwarder connection failed");
                }
                tokio::select! {
                    r = state.changed() => r?,
                    r = first.changed() => r?,
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                    r = newer.changed() => {
                        r?;
                        let generation = newer.borrow_and_update().as_ref().map(|n| n.generation);
                        if generation != Some(instruction.generation) {
                            bail!("superseded by a newer instruction");
                        }
                    }
                }
            }
        })
        .await;
        match connected {
            Ok(result) => result?,
            // Media flows but no keyframe made it (a slow downlink): the
            // picture switches at the next one, as it would have to anyway.
            Err(_)
                if *state.borrow() == RTCPeerConnectionState::Connected && *first.borrow() =>
            {
                tracing::info!("no keyframe through the forwarder yet; receiving through it anyway");
            }
            Err(_) => bail!("no media from the forwarder in time"),
        }
        tracing::info!(room = %instruction.room, %session, mode = %instruction.mode,
            tracks = instruction.tracks.len(), "receiving through the forwarder");
        Ok(())
    }
}

/// The forwarder's sound in place of the host's until the track ends
/// (`deployment-design.md` §5.4).
async fn play_forwarded_audio(
    track: Arc<webrtc::track::track_remote::TrackRemote>,
    audio: crate::media::audio::AudioPlayback,
    decrypt: Option<Arc<StdMutex<Decryptor>>>,
) {
    let codec = track.codec().capability;
    let Some(generation) = audio.select_forwarded(&codec.mime_type, codec.clock_rate, codec.channels)
    else {
        return;
    };
    tracing::info!(ssrc = track.ssrc(), "forwarded sound selected");
    /// The host's sound plays again however the track ends.
    struct Restore(crate::media::audio::AudioPlayback, u64);
    impl Drop for Restore {
        fn drop(&mut self) {
            self.0.end_forwarded(self.1);
        }
    }
    let _restore = Restore(audio.clone(), generation);
    while let Ok((packet, _)) = track.read_rtp().await {
        let data = match &decrypt {
            Some(decrypt) => match std_mutex_lock(decrypt)
                .decrypt(crate::media::frame_crypto::Codec::Opus, &packet.payload)
            {
                Ok(plain) => bytes::Bytes::from(plain),
                Err(_) => continue,
            },
            None => packet.payload,
        };
        audio.receive(
            generation,
            data,
            packet.header.timestamp,
            packet.header.sequence_number,
        );
    }
}
