//! Receiving the host's picture through the server's forwarder
//! (`docs/protocol/deployment-design.md` §5.4). When the host says so, a
//! second connection — to the forwarder, receive-only — carries the video
//! tracks; their frames go into the same receivers and decoders as the
//! host's own tracks, which send nothing meanwhile. Audio, input and every
//! other channel stay on the host connection.

use super::clock::{RemoteNtpEstimator, observe_remote_ntp};
use super::core::ConnectionCore;
use super::feedback::{RTP_DEFAULT_RTT, send_picture_loss_indication};
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
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;

/// The forwarder connection must carry media within this.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

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
    room: String,
    tracks: Vec<(u32, String)>,
    /// End-to-end mode: the frame keys.
    decrypt: Option<Arc<StdMutex<Decryptor>>>,
    connection: Arc<RTCPeerConnection>,
    stop: CancellationToken,
    /// Forwarder SSRC of each forwarded track, by this viewer's track index.
    ssrcs: Arc<StdMutex<BTreeMap<i32, u32>>>,
}

/// The forwarded part of a viewer connection.
#[derive(Default)]
pub(crate) struct Forwarding {
    active: StdMutex<Option<Active>>,
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
        loop {
            tokio::select! {
                _ = context.stop.cancelled() => break,
                changed = instructions.changed() => if changed.is_err() { break },
            }
            let Some(instruction) = instructions.borrow_and_update().clone() else {
                continue;
            };
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
            if let Some(old) = self.take() {
                old.stop.cancel();
                let _ = old.connection.close().await;
                let _ = api.sfu_unsubscribe(&old.room, &session).await;
                tracing::info!(room = %old.room, "left the forwarding room");
            }
            if instruction.room.is_empty() {
                tracing::info!("the host sends the picture directly again");
                continue;
            }
            // A newer instruction ends the wait for this room.
            let mut newer = instructions.clone();
            let status = match self.join(&context, &api, &session, &instruction, &mut newer).await {
                Ok(()) => ForwardStatus {
                    generation: instruction.generation,
                    receiving: true,
                    error: String::new(),
                },
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), room = %instruction.room,
                        "cannot receive through the forwarder");
                    if let Some(old) = self.take() {
                        old.stop.cancel();
                        let _ = old.connection.close().await;
                    }
                    ForwardStatus {
                        generation: instruction.generation,
                        receiving: false,
                        error: format!("{error:#}"),
                    }
                }
            };
            context.stream_control.forward_status(status);
        }
        if let Some(old) = self.take() {
            old.stop.cancel();
            let _ = old.connection.close().await;
        }
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
        if instruction.room != active.room || tracks != active.tracks {
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
        newer: &mut watch::Receiver<Option<MediaForward>>,
    ) -> Result<()> {
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
        let stop = context.stop.child_token();
        let ssrcs = Arc::new(StdMutex::new(BTreeMap::new()));
        let decrypt = match instruction.mode.as_str() {
            "e2ee" => {
                let mut keys = Decryptor::default();
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
                let Some(index) = indexes.get(&track.id()).copied() else {
                    tracing::debug!(track = %track.id(), "unexpected forwarded track");
                    return Box::pin(async {});
                };
                let Some(source) = context.tracks.get(index) else {
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
                        source.started.subscribe(),
                        source.keyframe_tx.clone(),
                        TrackForwardContext {
                            workers: context.workers.clone(),
                            stop,
                            kind: MediaKind::Video,
                            codec: codec.capability.mime_type,
                            codec_fmtp: codec.capability.sdp_fmtp_line,
                            video_payload_codecs: parameters.video_payload_codecs,
                            rtx_payload_apt: parameters.rtx_payload_apt,
                            red_payload_types: parameters.red_payload_types,
                            ulpfec_payload_types: parameters.ulpfec_payload_types,
                            flexfec_payload_types: parameters.flexfec_payload_types,
                            rsfec_config: parameters.rsfec_config,
                            extmap_allow_mixed: false,
                            audio: context.stream_control.audio(),
                            audio_generation: None,
                            connection,
                            video_annexb_sinks: Arc::clone(&source.sinks),
                            performance: source.performance.clone(),
                            nack_rtt_micros: Arc::new(AtomicU64::new(
                                RTP_DEFAULT_RTT.as_micros() as u64,
                            )),
                            remote_ntp,
                            receiver_feedback: Some(receiver_feedback),
                            receiver_feedback_sender: feedback,
                            stream_control: context.stream_control.clone(),
                            decrypt,
                        },
                    )
                    .await;
                });
                Box::pin(async {})
            }));
        }
        *std_mutex_lock(&self.active) = Some(Active {
            room: instruction.room.clone(),
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
        let offer = connection.create_offer(None).await?;
        let mut gathered = connection.gathering_complete_promise().await;
        connection.set_local_description(offer).await?;
        // The forwarder answers checks from any address: host candidates
        // suffice, relayed ones need not be waited for.
        let _ = tokio::time::timeout(Duration::from_secs(1), gathered.recv()).await;
        let offer = connection
            .local_description()
            .await
            .context("missing forwarder offer")?
            .sdp;
        let tracks = instruction.tracks.iter().map(|t| t.track.clone()).collect();
        let subscription = api
            .sfu_subscribe(&instruction.room, session, offer, Some(tracks))
            .await
            .context("subscribe to the forwarding room")?;
        connection
            .set_remote_description(RTCSessionDescription::answer(subscription.answer)?)
            .await
            .context("install the forwarder's answer")?;
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, async {
            loop {
                if *state.borrow_and_update() == RTCPeerConnectionState::Connected
                    && *first.borrow_and_update()
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
                    r = newer.changed() => {
                        r?;
                        bail!("superseded by a newer instruction");
                    }
                }
            }
        })
        .await;
        match connected {
            Ok(result) => result?,
            Err(_) => bail!("no media from the forwarder in time"),
        }
        tracing::info!(room = %instruction.room, mode = %instruction.mode,
            tracks = instruction.tracks.len(), "receiving through the forwarder");
        Ok(())
    }
}
