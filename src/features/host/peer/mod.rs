//! Controlled desktop connection: session-scoped peer, capture owner and RTP sender.
use super::{VideoConfig, capture, lock};
use crate::transport::rtc::ConnectionCore;
use anyhow::Result;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use webrtc::{
    api::media_engine::MediaEngine,
    data_channel::RTCDataChannel,
    ice_transport::{ice_candidate::RTCIceCandidateInit, ice_server::RTCIceServer},
    peer_connection::{
        configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState,
        policy::ice_transport_policy::RTCIceTransportPolicy,
        sdp::session_description::RTCSessionDescription,
    },
    rtp_transceiver::{
        RTCPFeedback,
        rtp_codec::{
            RTCRtpCodecCapability, RTCRtpCodecParameters, RTCRtpHeaderExtensionCapability,
            RTPCodecType,
        },
    },
    track::track_local::{TrackLocal, track_local_static_rtp::TrackLocalStaticRTP},
};

pub(crate) struct Peer {
    activity: Option<crate::platform::host_service::activity::Work>,
    core: ConnectionCore,
    pub candidates: mpsc::UnboundedReceiver<RTCIceCandidateInit>,
    cancel: CancellationToken,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    screens: Arc<tokio::sync::Mutex<screens::Screens>>,
    reports: Arc<screens::Reports>,
    handle: super::SessionLease,
    control_receiver: crate::transport::uu_kcp::ControlReceiver,
    remote_ice: tokio::sync::Mutex<RemoteIce>,
    transport: super::transport::Transport,
    ice_servers: Vec<RTCIceServer>,
    relay_only: AtomicBool,
    input: super::input::Session,
    audio: super::audio::Audio,
    microphone: super::microphone::Session,
    microphone_tracks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

#[derive(Default)]
struct RemoteIce {
    current: std::collections::HashSet<String>,
    retired: std::collections::HashSet<String>,
    pending: std::collections::VecDeque<RTCIceCandidateInit>,
}

#[derive(Clone, PartialEq, Eq)]
struct Published {
    screen: capture::Screen,
    capturing: bool,
    visible: bool,
    quality: i32,
    fps: u32,
    format: super::format::Format,
    budget_size: (u32, u32),
    encoder: Option<super::format::Backend>,
    capture: String,
}
#[derive(Clone, Default)]
pub(super) struct ReportRoutes {
    pub(super) text: Option<std::sync::Weak<RTCDataChannel>>,
    pub(super) clipboard: i32,
    control: Option<std::sync::Weak<RTCDataChannel>>,
    control_screens: bool,
    handshake_complete: bool,
    handshake_answered: bool,
    revision: u64,
    secure_revision: u64,
}
impl ReportRoutes {
    fn received(
        &mut self,
        received: &crate::features::stream_control::publisher::Received,
    ) -> bool {
        let opened = received.handshake_complete && !self.handshake_complete;
        self.handshake_complete |= received.handshake_complete;
        if let Some(level) = received.clipboard {
            self.clipboard = level;
        }
        let reroute = received
            .control_screen_reports
            .is_some_and(|v| v != self.control_screens);
        if let Some(value) = received.control_screen_reports {
            self.control_screens = value;
        }
        if opened || reroute || received.refresh_state || received.clipboard.is_some() {
            self.revision = self.revision.wrapping_add(1);
        }
        if received.refresh_secure {
            self.secure_revision = self.secure_revision.wrapping_add(1);
        }
        opened
            || reroute
            || received.refresh_state
            || received.refresh_secure
            || received.clipboard.is_some()
    }
}
type ReportTarget = tokio::sync::watch::Sender<ReportRoutes>;
mod upgrade;
pub(crate) use upgrade::UpdateNotice;

impl Peer {
    pub(crate) async fn new(
        screen: Option<capture::Screen>,
        owner: super::SessionLease,
        cancel: CancellationToken,
        displays: Arc<super::displays::Session>,
        ice_servers: Vec<RTCIceServer>,
        relay: bool,
        initial: VideoConfig,
        control_screens: bool,
        negotiated: Arc<super::format::Negotiated>,
        network: super::network::Policy,
        input_policy: super::input::wire::Policy,
        deferred: Option<screens::Deferred>,
        audio_control: bool,
        annotation_extension: bool,
        display_geometry: bool,
        diagnostics_extension: bool,
        drag_extension: bool,
        controlling_features: Option<crate::account::feature_ability::FeatureCatalog>,
        clipboard_platform: crate::protocol::peer_platform::PeerPlatform,
        clipboard_level: i32,
        file_capabilities: crate::features::file_transfer::host::Capabilities,
        data_only: bool,
        // `CT_TERMINAL`: this session may open a shell on this machine.
        terminal: bool,
        update_resume: Option<super::update_resume::Context>,
    ) -> Result<Self> {
        let audio_only = deferred.is_some() && !data_only;
        let handle = owner.with_cancellation(cancel.clone());
        tracing::info!(?network, "host network switch policy");
        let initial_quality = if initial.quality == 5 {
            initial.auto_quality
        } else {
            initial.quality
        };
        let initial_fps = initial
            .fps
            .min(screen.as_ref().map_or(144, |s| s.fps))
            .max(1);
        let size = screen.as_ref().map_or((0, 0), |s| (s.width, s.height));
        let initial_bounds = if screen.is_none() {
            super::parameters::Bounds {
                reservation: 0,
                maximum: 0,
                initial: 0,
                probe: 0,
            }
        } else if initial.quality == 5 {
            super::parameters::automatic(
                initial.format,
                initial_quality,
                crate::media::geometry::fit_size(size.0, size.1, initial.maximum),
                initial_fps,
                true,
            )
        } else {
            super::parameters::fixed(
                initial.format,
                initial_quality,
                initial.bitrate,
                crate::media::geometry::fit_size(size.0, size.1, initial.maximum),
                initial_fps,
            )
        };
        let transport = super::transport::Transport::new(
            handle.clone(),
            cancel.clone(),
            initial_bounds,
            network,
        );
        let mut registry = webrtc::interceptor::registry::Registry::new();
        registry.add(Box::new(transport.clone()));
        registry.add(Box::new(crate::transport::rtcp_timing::RtcpTiming::new()));
        let mut media = MediaEngine::default();
        media.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: "audio/opus".into(),
                    clock_rate: 48_000,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;stereo=1;useinbandfec=1".into(),
                    rtcp_feedback: vec![RTCPFeedback {
                        typ: "transport-cc".into(),
                        parameter: String::new(),
                    }],
                    ..Default::default()
                },
                payload_type: 111,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )?;
        media.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: "video/H264".into(),
                    clock_rate: 90_000,
                    sdp_fmtp_line:
                        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                            .into(),
                    rtcp_feedback: vec![
                        RTCPFeedback {
                            typ: "rrtr".into(),
                            parameter: String::new(),
                        },
                        RTCPFeedback {
                            typ: "transport-cc".into(),
                            parameter: String::new(),
                        },
                        RTCPFeedback {
                            typ: "nack".into(),
                            parameter: String::new(),
                        },
                        RTCPFeedback {
                            typ: "nack".into(),
                            parameter: "pli".into(),
                        },
                        RTCPFeedback {
                            typ: "ccm".into(),
                            parameter: "fir".into(),
                        },
                    ],
                    ..Default::default()
                },
                payload_type: 98,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
        for (kind, mime, pt, fmtp, rtx) in [
            (super::format::Codec::H265, "video/H265", 96, "", "apt=96"),
            (
                super::format::Codec::Av1,
                "video/AV1",
                104,
                "profile=0;level-idx=17;tier=0",
                "apt=104",
            ),
            (
                super::format::Codec::Av1,
                "video/AV1",
                106,
                "profile=1;level-idx=17;tier=0",
                "apt=106",
            ),
        ] {
            if screen.is_none()
                || (negotiated.supports_codec(kind)
                    && (pt != 106
                        || negotiated.choices.iter().any(|c| {
                            c.capability.format.codec == super::format::Codec::Av1
                                && c.capability.format.chroma == 3
                        })))
            {
                media.register_codec(
                    RTCRtpCodecParameters {
                        capability: RTCRtpCodecCapability {
                            mime_type: mime.into(),
                            clock_rate: 90_000,
                            sdp_fmtp_line: fmtp.into(),
                            rtcp_feedback: vec![
                                RTCPFeedback {
                                    typ: "rrtr".into(),
                                    parameter: String::new(),
                                },
                                RTCPFeedback {
                                    typ: "transport-cc".into(),
                                    parameter: String::new(),
                                },
                                RTCPFeedback {
                                    typ: "nack".into(),
                                    parameter: String::new(),
                                },
                                RTCPFeedback {
                                    typ: "nack".into(),
                                    parameter: "pli".into(),
                                },
                                RTCPFeedback {
                                    typ: "ccm".into(),
                                    parameter: "fir".into(),
                                },
                            ],
                            ..Default::default()
                        },
                        payload_type: pt,
                        ..Default::default()
                    },
                    RTPCodecType::Video,
                )?;
                media.register_codec(
                    RTCRtpCodecParameters {
                        capability: RTCRtpCodecCapability {
                            mime_type: "video/rtx".into(),
                            clock_rate: 90_000,
                            sdp_fmtp_line: rtx.into(),
                            ..Default::default()
                        },
                        payload_type: pt + 1,
                        ..Default::default()
                    },
                    RTPCodecType::Video,
                )?;
            }
        }
        media.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: "video/rtx".into(),
                    clock_rate: 90_000,
                    sdp_fmtp_line: "apt=98".into(),
                    ..Default::default()
                },
                payload_type: 99,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
        for uri in [
            "urn:ietf:params:rtp-hdrext:ssrc-audio-level",
            "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time",
            "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
            "urn:ietf:params:rtp-hdrext:sdes:mid",
        ] {
            media.register_header_extension(
                RTCRtpHeaderExtensionCapability { uri: uri.into() },
                RTPCodecType::Audio,
                None,
            )?;
        }
        for uri in [
            "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time",
            "urn:ietf:params:rtp-hdrext:toffset",
            "http://www.webrtc.org/experiments/rtp-hdrext/video-timing",
            "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-sending-delay",
            "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
            "urn:ietf:params:rtp-hdrext:sdes:mid",
            "http://www.webrtc.org/experiments/rtp-hdrext/color-space",
            "http://www.webrtc.org/experiments/rtp-hdrext/video-capture-index",
            "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-is-new-frame",
            "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay",
        ] {
            media.register_header_extension(
                RTCRtpHeaderExtensionCapability { uri: uri.into() },
                RTPCodecType::Video,
                None,
            )?;
        }
        media.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: "video/rs-fec-cm256".into(),
                    clock_rate: 90_000,
                    sdp_fmtp_line: "max-k=109;repair-window=10000000;rtx-as-source=1".into(),
                    ..Default::default()
                },
                payload_type: 36,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
        let mut settings = ConnectionCore::settings();
        settings.enable_sender_rtx(true);
        settings.enable_sender_rsfec(true);
        let core = ConnectionCore::new(
            media,
            registry,
            settings,
            RTCConfiguration {
                ice_servers: ice_servers.clone(),
                ice_transport_policy: if relay {
                    RTCIceTransportPolicy::Relay
                } else {
                    RTCIceTransportPolicy::All
                },
                ..Default::default()
            },
        )
        .await?;
        let connection = core.connection.clone();
        // Failed construction cancels role workers; the core owns transport cleanup.
        let construction = cancel.clone().drop_guard();
        let (candidate_tx, candidates) = mpsc::unbounded_channel();
        connection.on_ice_candidate(Box::new(move |c| {
            if let Some(c) = c.and_then(|c| c.to_json().ok()) {
                let _ = candidate_tx.send(c);
            }
            Box::pin(async {})
        }));
        let connected = Arc::new(AtomicBool::new(false));
        let audio_track = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: "audio/opus".into(),
                clock_rate: 48_000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;stereo=1;useinbandfec=1".into(),
                ..Default::default()
            },
            "audio_0".into(),
            "audio_0".into(),
        ));
        let audio_sender = connection
            .add_track(audio_track.clone() as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        let audio = super::audio::Audio::new(
            handle.clone(),
            cancel.clone(),
            connected.clone(),
            transport.clone(),
        );
        audio.enable_quality_control(audio_control, audio_only);
        audio.set_media_allowed(!data_only);
        let screen_pool = screens::Screens::new(
            connection.clone(),
            handle.clone(),
            cancel.clone(),
            connected.clone(),
            initial,
            negotiated.clone(),
            transport.clone(),
            screen.clone(),
            displays,
            deferred,
        )
        .await?;
        let sender = screen_pool.slots[0].sender.clone();
        let reports = screen_pool.reports.clone();
        let input_reports = reports.clone();
        let input = super::input::Session::new(
            handle.clone(),
            cancel.clone(),
            connected.clone(),
            input_policy,
            move || super::input::Geometry {
                current: input_reports.current.load(Ordering::Acquire),
                screens: {
                    // A compositor resolves an absolute pointer position in its
                    // own layout, which the display server's rectangles are not.
                    // Only a snapshot is read here: this runs under the input gate.
                    let layout = crate::platform::display::compositor_layout();
                    lock(&input_reports.catalog)
                        .iter()
                        .map(|s| super::input::Screen {
                            id: s.screen.id,
                            left: s.screen.left,
                            top: s.screen.top,
                            width: s.screen.width,
                            height: s.screen.height,
                            logical: layout
                                .as_ref()
                                .and_then(|layout| layout.output(&s.screen.device_name))
                                .and_then(|output| {
                                    Some(super::input::Rect {
                                        left: output.left,
                                        top: output.top,
                                        right: output
                                            .left
                                            .checked_add(i32::try_from(output.width).ok()?)?,
                                        bottom: output
                                            .top
                                            .checked_add(i32::try_from(output.height).ok()?)?,
                                    })
                                }),
                        })
                        .collect()
                },
            },
        )?;
        let microphone =
            super::microphone::Session::new(handle.clone(), cancel.clone(), connected.clone())?;
        let microphone_tracks = Arc::new(Mutex::new(Vec::new()));
        let track_tasks = microphone_tracks.clone();
        let track_microphone = microphone.receiver();
        let track_cancel = cancel.clone();
        connection.on_track(Box::new(move |track, receiver, _| {
            let microphone = track_microphone.clone();
            let stop = track_cancel.clone();
            let tasks = track_tasks.clone();
            Box::pin(async move {
                if track.kind() != RTPCodecType::Audio || stop.is_cancelled() { return; }
                let codec = track.codec().capability;
                if !codec.mime_type.eq_ignore_ascii_case("audio/opus") || codec.clock_rate != 48000 || !(1..=2).contains(&codec.channels) {
                    tracing::warn!(codec=%codec.mime_type,"unsupported remote microphone codec");
                    return;
                }
                let generation = microphone.select_source();
                let receive_stop = stop.clone();
                let task = tokio::spawn(async move {
                    loop {
                        let incoming = tokio::select! { _=receive_stop.cancelled()=>break, p=track.read_rtp()=>p };
                        let Ok((packet, _)) = incoming else { break; };
                        microphone.receive(generation, packet.payload, packet.header.sequence_number, packet.header.timestamp);
                    }
                });
                let reports = tokio::spawn(async move {
                    loop {
                        let result = tokio::select! { _=stop.cancelled()=>break, r=receiver.read_rtcp()=>r };
                        if result.is_err() { break; }
                    }
                });
                let mut tasks = lock(&tasks);
                for old in tasks.drain(..) { let old: tokio::task::JoinHandle<()> = old; old.abort(); }
                tasks.extend([task, reports]);
            })
        }));
        let stream_transports: Vec<_> = screen_pool
            .slots
            .iter()
            .map(|s| s.transport.clone())
            .collect();
        let screens = Arc::new(tokio::sync::Mutex::new(screen_pool));
        let ready = connected.clone();
        let state_handle = handle.clone();
        let state_transport = transport.clone();
        let state_dtls = Arc::downgrade(&connection.sctp().transport());
        let stopped = cancel.clone();
        let state_input = input.receiver();
        let state_audio = audio.clone();
        let state_microphone = microphone.receiver();
        let (ports, ports_input) =
            crate::features::port_mapping::host::Receiver::new(handle.clone(), connected.clone());
        let ports_worker = tokio::spawn(crate::features::port_mapping::host::run(
            ports.clone(),
            ports_input,
            cancel.clone(),
        ));
        let annotation_reports = reports.clone();
        let annotation_lease = handle.clone();
        let annotation_connected = connected.clone();
        let (annotation, annotation_worker) = super::annotation::Receiver::start(
            !data_only && !audio_only,
            annotation_extension,
            move || super::annotation::Context {
                allowed: annotation_lease.requested()
                    && annotation_connected.load(Ordering::Acquire),
                current: annotation_reports.current.load(Ordering::Acquire),
                screens: lock(&annotation_reports.catalog)
                    .iter()
                    .map(|s| s.screen.clone())
                    .collect(),
            },
            cancel.clone(),
        );
        let state_annotation = annotation.clone();
        let state_ports = ports.clone();
        connection.on_peer_connection_state_change(Box::new(move |state| {
            tracing::info!(?state, "host peer connection state changed");
            state_transport.network(state == RTCPeerConnectionState::Connected);
            let was_ready =
                ready.swap(state == RTCPeerConnectionState::Connected, Ordering::AcqRel);
            if was_ready && state != RTCPeerConnectionState::Connected {
                state_ports.invalidate();
                state_annotation.invalidate();
                state_input.transport_lost();
                state_audio.transport_lost();
                state_microphone.transport_lost();
            }
            state_handle.update(
                true,
                state == RTCPeerConnectionState::Connected,
                match state {
                    RTCPeerConnectionState::Connected => "正在被远程访问",
                    RTCPeerConnectionState::Disconnected => "连接暂时中断",
                    RTCPeerConnectionState::Failed => "连接已中断",
                    RTCPeerConnectionState::Closed => "等待连接",
                    _ => "正在建立远程连接",
                },
            );
            if matches!(
                state,
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
            ) {
                stopped.cancel();
            }
            let dtls = state_dtls.clone();
            let annotation = state_annotation.clone();
            let transport = state_transport.clone();
            Box::pin(async move {
                if state == RTCPeerConnectionState::Connected {
                    annotation.notify_hello().await;
                    if let Some(dtls) = dtls.upgrade() {
                        transport.srtp_overhead(dtls.rtp_authentication_overhead().await);
                    }
                }
            })
        }));

        let route_transport = transport.clone();
        sender
            .transport()
            .ice_transport()
            .on_selected_candidate_pair_change(Box::new(move |pair| {
                tracing::info!(local_type=?pair.local.typ, remote_type=?pair.remote.typ,
                    local_relay_protocol=%pair.local.relay_protocol,
                    remote_relay_protocol=%pair.remote.relay_protocol,
                    "host selected transport path");
                route_transport.route_changed();
                route_transport.selected_route(&pair);
                let transport = route_transport.clone();
                Box::pin(async move {
                    let ip = if pair
                        .local
                        .address
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_ipv6())
                    {
                        40
                    } else {
                        20
                    };
                    let network = ip
                        + if pair.local.protocol
                            == webrtc::ice_transport::ice_protocol::RTCIceProtocol::Tcp
                        {
                            20
                        } else {
                            8
                        };
                    transport.route_overhead(network);
                })
            }));
        let config = Arc::new(Mutex::new(initial));
        let kcp = core.control.clone();
        let (report_target, report_receiver) = tokio::sync::watch::channel(ReportRoutes {
            control_screens,
            clipboard: clipboard_level,
            ..Default::default()
        });
        handle.set_update_notice(UpdateNotice::new(
            report_target.clone(),
            cancel.clone(),
            update_resume,
            screens.clone(),
        ));
        let cursor = tokio::spawn(cursor::run(
            reports.clone(),
            report_receiver.clone(),
            cancel.clone(),
            handle.clone(),
            kcp.clone(),
        ));
        let clipboard_receiver = super::clipboard::Receiver::default();
        // Capability belongs to the connection. The clipboard worker gates execution
        // on the current video sources, including viewers attached after audio/files.
        clipboard_receiver.native(drag_extension);
        if !drag_extension {
            clipboard_receiver.official_drop(controlling_features);
        }
        let clipboard_screens = reports.clone();
        let clipboard_worker = tokio::spawn(super::clipboard::run(
            clipboard_receiver.clone(),
            report_receiver.clone(),
            handle.clone(),
            connected.clone(),
            cancel.clone(),
            clipboard_platform,
            input.receiver(),
            move || {
                let viewing = clipboard_screens
                    .media()
                    .iter()
                    .any(|(_, media)| media.capturing);
                let screens = lock(&clipboard_screens.catalog)
                    .iter()
                    .map(|s| s.screen.clone())
                    .collect();
                (screens, viewing)
            },
        ));
        let files = super::files::Receiver::default();
        files.capabilities(file_capabilities);
        let file_worker = tokio::spawn(super::files::run(
            files.clone(),
            handle.clone(),
            connected.clone(),
            cancel.clone(),
            handle.file_scope(),
            reports.sequence.clone(),
        ));
        let publisher = tokio::spawn(publish_state(
            screens.clone(),
            reports.clone(),
            stream_transports.clone(),
            report_receiver,
            cancel.clone(),
            kcp.clone(),
            screen.clone(),
            input.receiver().mouse_policy(),
            audio.clone(),
            microphone.receiver().status(),
            display_geometry,
        ));
        let (control_receiver, control_responses) = ingress::ControlIngress {
            screen: screen.clone(),
            config: config.clone(),
            negotiated: negotiated.clone(),
            input: input.receiver(),
            files: files.clone(),
            annotation: annotation.clone(),
            screens: screens.clone(),
            report_target: report_target.clone(),
            kcp: kcp.clone(),
            cancel: cancel.clone(),
        }
        .start();
        let channel_screens = screens.clone();
        let channel_cancel = cancel.clone();
        let channel_kcp = kcp.clone();
        let channel_input = input.receiver();
        let channel_microphone = microphone.receiver();
        let channel_audio = audio.clone();
        let channel_files = files.clone();
        let channel_ports = ports.clone();
        let channel_annotation = annotation.clone();
        let channel_clipboard = clipboard_receiver.clone();
        let channel_control = control_receiver.clone();
        let diagnostics_binding: Arc<Mutex<Option<CancellationToken>>> = Default::default();
        let diagnostics_lease = handle.clone();
        let diagnostics_connected = connected.clone();
        connection.on_data_channel(Box::new(move |channel| {
            if diagnostics_extension && channel.label() == crate::diagnostics::remote::CHANNEL {
                channel_kcp.bind_stream(channel.id(), false);
                let token = crate::diagnostics::remote::bind_host(
                    channel,
                    diagnostics_lease.clone(),
                    diagnostics_connected.clone(),
                    channel_cancel.clone(),
                );
                if let Some(previous) = lock(&diagnostics_binding).replace(token) {
                    previous.cancel();
                }
                return Box::pin(async {});
            }
            // The terminal rides the same low-priority BINARY channel the
            // diagnostics transfer uses; when the controller did not ask for
            // diagnostics, that channel carries the terminal requests.
            if terminal && !diagnostics_extension && channel.label() == "BINARY_DATA_CHANNEL" {
                crate::features::host::terminal::bind(
                    &channel,
                    channel_screens.clone(),
                    channel_cancel.clone(),
                );
                return Box::pin(async {});
            }
            let screens = channel_screens.clone();
            let stop = channel_cancel.clone();
            let kcp = channel_kcp.clone();
            let report_target = report_target.clone();
            let input = channel_input.clone();
            let microphone = channel_microphone.clone();
            let audio = channel_audio.clone();
            let clipboard = channel_clipboard.clone();
            let files = channel_files.clone();
            let ports = channel_ports.clone();
            let annotation = channel_annotation.clone();
            let control = channel_control.clone();
            Box::pin(async move {
                bind_channel(
                    channel,
                    screens,
                    stop,
                    kcp,
                    report_target,
                    input,
                    microphone,
                    audio,
                    clipboard,
                    files,
                    ports,
                    annotation,
                    control,
                )
                .await;
            })
        }));
        let mut stream_tasks = Vec::new();
        for transport in &stream_transports {
            let repairs = transport.clone();
            stream_tasks.push(tokio::spawn(async move {
                repairs.repairs().await;
            }));
            let fec = transport.clone();
            stream_tasks.push(tokio::spawn(async move {
                fec.fec_worker().await;
            }));
        }
        let control_transport = transport.clone();
        let control_worker = tokio::spawn(async move {
            control_transport.control().await;
        });
        let probe_transport = transport.clone();
        let probe_worker = tokio::spawn(async move {
            probe_transport.probes().await;
        });
        // Transport-cc may use media_ssrc=0, which is separate from the video
        // sender's RTCP stream. Consume it through the authenticated SRTCP session.
        let zero_transport = transport.clone();
        let zero_cancel = cancel.clone();
        let zero_dtls = sender.transport();
        let zero_feedback = tokio::spawn(async move {
            let stream = loop {
                if zero_cancel.is_cancelled() {
                    return;
                }
                if let Some(stream) = zero_dtls.rtcp_read_stream(0).await {
                    break stream;
                }
                tokio::select! { _=zero_cancel.cancelled()=>return, _=tokio::time::sleep(Duration::from_millis(20))=>{} }
            };
            let mut bytes = vec![0u8; 65536];
            loop {
                let reports = tokio::select! { _=zero_cancel.cancelled()=>break, r=stream.read_rtcp(&mut bytes)=>r };
                let reports = match reports {
                    Ok(reports) => reports,
                    Err(webrtc_srtp::Error::Rtcp(error)) => {
                        tracing::debug!(%error,"discarded malformed host transport feedback");
                        continue;
                    }
                    Err(_) => break,
                };
                for report in reports {
                    if let Some(feedback)=report.as_any().downcast_ref::<webrtc::rtcp::transport_feedbacks::transport_layer_cc::TransportLayerCc>() {
                        zero_transport.feedback(feedback);
                    }
                }
            }
        });
        let capture_audio = audio.clone();
        let activity = crate::platform::host_service::activity::Work::new();
        stream_tasks.push(tokio::spawn(async move {
            if let Err(error) = tokio::task::spawn_blocking(move || {
                let _activity = activity;
                capture_audio.run()
            })
            .await
            {
                tracing::error!(%error,"desktop audio worker failed");
            }
        }));
        let send_audio = audio.clone();
        let send_cancel = cancel.clone();
        stream_tasks.push(tokio::spawn(async move {
            tokio::select! { _=send_cancel.cancelled()=>{}, _=send_audio.transmitter().send(&send_audio,audio_track)=>{} }
        }));
        let report_audio = audio.clone();
        let report_sender = audio_sender.clone();
        let report_peer = Arc::downgrade(&connection);
        let report_cancel = cancel.clone();
        stream_tasks.push(tokio::spawn(async move {
            tokio::select! { _=report_cancel.cancelled()=>{}, _=report_audio.transmitter().send_reports(&report_audio,report_peer,report_sender)=>{} }
        }));
        let feedback_audio = audio.clone();
        let feedback_cancel = cancel.clone();
        stream_tasks.push(tokio::spawn(async move {
            tokio::select! { _=feedback_cancel.cancelled()=>{}, _=feedback_audio.transmitter().feedback(audio_sender)=>{} }
        }));
        let peer_transport = transport.clone();
        // Only the installed Windows service hosts on behalf of a separate
        // user GUI process.
        #[cfg(windows)]
        if crate::platform::host_service::resident::is_owner() {
            let user_cancel = cancel.clone();
            let user_lease = handle.clone();
            stream_tasks.push(tokio::spawn(async move {
                let mut observed:Option<crate::platform::windows::host_service::user_backend::Observer>=None;
                loop {
                    tokio::select!{_=user_cancel.cancelled()=>break,_=tokio::time::sleep(Duration::from_millis(100))=>{}}
                    if let Some(owner)=&observed {
                        if !owner.alive() {
                            // A restarted GUI is a new user execution authority.
                            // Do not retain old file/clipboard/input operations.
                            tracing::warn!("user backend exited; retiring controlled connection");
                            // Retire the execution lease so signaling sends
                            // clear_out; a bare media cancel leaves viewers
                            // waiting to reconnect to an invalid old session.
                            user_lease.finish();
                            user_cancel.cancel();break;
                        }
                    } else {
                        match crate::platform::windows::host_service::user_backend::observe() {
                            Ok(owner)=>observed=owner.filter(|owner|owner.alive()),
                            Err(error)=>{tracing::warn!(%error,"user backend observation failed");user_lease.finish();user_cancel.cancel();break;}
                        }
                    }
                }
            }));
        }
        construction.disarm();
        Ok(Self {
            activity: Some(crate::platform::host_service::activity::Work::new()),
            core,
            candidates,
            cancel,
            tasks: {
                stream_tasks.extend([
                    zero_feedback,
                    control_worker,
                    probe_worker,
                    control_responses,
                    publisher,
                    cursor,
                    clipboard_worker,
                    file_worker,
                    ports_worker,
                    annotation_worker,
                ]);
                stream_tasks
            },
            screens,
            reports,
            handle: owner,
            control_receiver,
            remote_ice: Default::default(),
            transport: peer_transport,
            ice_servers,
            relay_only: AtomicBool::new(relay),
            input,
            audio,
            microphone,
            microphone_tracks,
        })
    }

    async fn restart_network(&self, network_type: u8) -> Result<()> {
        // T B055D0: answerer policy is distinct from the controller switch API.
        let (relay, tls) = match network_type {
            0 => (true, self.transport.network_attempt() == 2),
            1 => (true, true),
            2 | 3 => (false, false),
            _ => anyhow::bail!("无效的ICE网络类型"),
        };
        let mut servers = self.ice_servers.clone();
        if tls {
            for server in &mut servers {
                server
                    .urls
                    .retain(|url| url.to_ascii_lowercase().contains("turns:"));
            }
            servers.retain(|server| !server.urls.is_empty());
            anyhow::ensure!(!servers.is_empty(), "当前会话未提供TURNS服务器");
        }
        let policy = if relay {
            RTCIceTransportPolicy::Relay
        } else {
            RTCIceTransportPolicy::All
        };
        self.core.configure_ice(servers, policy).await?;
        self.relay_only.store(relay, Ordering::Release);
        tracing::info!(network_type, relay, tls, "host ICE restart policy applied");
        Ok(())
    }
    pub(crate) async fn local_candidate(
        &self,
        mut candidate: RTCIceCandidateInit,
    ) -> Option<RTCIceCandidateInit> {
        if self.ended() || !self.handle.requested() || candidate.candidate.is_empty() {
            return None;
        }
        if self.relay_only.load(Ordering::Acquire)
            && !candidate
                .candidate
                .to_ascii_lowercase()
                .contains(" typ relay")
        {
            return None;
        }
        let sdp = self.core.connection.local_description().await?.sdp;
        if candidate
            .username_fragment
            .as_ref()
            .is_some_and(|fragment| {
                !sdp.lines()
                    .filter_map(|line| line.strip_prefix("a=ice-ufrag:"))
                    .any(|f| f == fragment)
            })
        {
            return None;
        }
        if candidate.sdp_mid.as_ref().is_none_or(String::is_empty) {
            let index = usize::from(candidate.sdp_mline_index.unwrap_or(0));
            candidate.sdp_mid = sdp
                .split("m=")
                .skip(1)
                .nth(index)
                .and_then(|media| media.lines().find_map(|line| line.strip_prefix("a=mid:")))
                .map(str::to_owned);
        }
        Some(candidate)
    }
    pub(crate) async fn answer(
        &self,
        sdp: String,
        restart: bool,
        network_type: u8,
    ) -> Result<String> {
        tracing::info!(sdp_bytes = sdp.len(), "host received video offer");
        let mixed_kcp = crate::transport::rtc::negotiated_mixed_kcp_version(&sdp)?;
        let fragments: std::collections::HashSet<String> = sdp
            .lines()
            .filter_map(|l| l.strip_prefix("a=ice-ufrag:"))
            .map(str::to_owned)
            .collect();
        anyhow::ensure!(!fragments.is_empty(), "主控SDP缺少ICE代次");
        let description = RTCSessionDescription::offer(sdp)?;
        let audio_config = crate::media::audio::encoder::remote_config(&description)?;
        if restart {
            if let Err(error) = self.restart_network(network_type).await {
                // Native B055D0 logs SetConfig failure and retains the live peer.
                tracing::warn!(%error,network_type,"host ICE restart policy rejected; retaining active configuration");
            }
        }
        let mut ice = self.remote_ice.lock().await;
        self.core
            .connection
            .set_remote_description(description)
            .await?;
        let previous = std::mem::replace(&mut ice.current, fragments);
        for fragment in previous {
            if !ice.current.contains(&fragment) {
                ice.retired.insert(fragment);
            }
        }
        let mut pending = std::mem::take(&mut ice.pending);
        while let Some(candidate) = pending.pop_front() {
            if candidate
                .username_fragment
                .as_ref()
                .is_none_or(|f| ice.current.contains(f))
            {
                if let Err(error) = self.core.connection.add_ice_candidate(candidate).await {
                    // T AFA310: a rejected candidate is local to that candidate,
                    // including when it arrived before the offer. Keep answering.
                    tracing::debug!(%error, "discarded queued host ICE candidate; SDP negotiation retained");
                }
            } else if candidate
                .username_fragment
                .as_ref()
                .is_some_and(|f| !ice.retired.contains(f))
            {
                ice.pending.push_back(candidate);
            }
        }
        drop(ice);
        let sdp = self
            .core
            .answer("audio_0 video_0 video_1 video_2 video_3 video_4", mixed_kcp)
            .await?;
        anyhow::ensure!(!self.ended() && self.handle.requested(), "画面会话已取消");
        self.core
            .activate_control(mixed_kcp, self.control_receiver.clone())?;
        self.audio.configure(audio_config);
        Ok(sdp)
    }
    pub(crate) async fn add_candidate(&self, mut candidate: RTCIceCandidateInit) -> Result<()> {
        if self.ended() || !self.handle.requested() {
            return Ok(());
        }
        candidate.username_fragment = candidate.effective_username_fragment();
        let mut ice = self.remote_ice.lock().await;
        if candidate
            .username_fragment
            .as_ref()
            .is_some_and(|f| ice.retired.contains(f))
        {
            return Ok(());
        }
        if ice.current.is_empty()
            || candidate
                .username_fragment
                .as_ref()
                .is_some_and(|f| !ice.current.contains(f))
        {
            anyhow::ensure!(ice.pending.len() < 256, "等待SDP的ICE候选过多");
            ice.pending.push_back(candidate);
            return Ok(());
        }
        self.core.connection.add_ice_candidate(candidate).await?;
        Ok(())
    }
    pub(crate) fn access_revoked(&self) -> bool {
        !self.handle.requested()
    }
    pub(crate) fn displays(&self) -> Vec<crate::protocol::capability::DisplayCapability> {
        lock(&self.reports.catalog)
            .iter()
            .map(|info| crate::protocol::capability::DisplayCapability {
                id: info.screen.id,
                fps: info.screen.fps,
                kind: info.kind,
                hdr: i32::from(info.screen.hdr),
            })
            .collect()
    }
    pub(crate) fn take_prepared_capabilities(&self) -> Option<Vec<super::format::Capability>> {
        lock(&self.reports.capabilities).take()
    }
    pub(crate) fn ended(&self) -> bool {
        self.cancel.is_cancelled()
    }
    pub(crate) fn network_change(&self) -> Option<u8> {
        if self.ended() {
            None
        } else {
            self.transport.network_change()
        }
    }
    pub(crate) async fn close(mut self) {
        let started = std::time::Instant::now();
        self.cancel.cancel();
        self.input.close();
        self.microphone.close();
        let input_done = started.elapsed();
        let _ = self.core.close().await;
        let transport_done = started.elapsed();
        let tracks = std::mem::take(&mut *lock(&self.microphone_tracks));
        for track in tracks {
            let _ = track.await;
        }
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
        let workers_done = started.elapsed();
        self.screens.lock().await.close().await;
        self.handle.finish();
        tracing::info!(
            input_ms = input_done.as_millis(),
            transport_ms = (transport_done - input_done).as_millis(),
            workers_ms = (workers_done - transport_done).as_millis(),
            screens_ms = (started.elapsed() - workers_done).as_millis(),
            total_ms = started.elapsed().as_millis(),
            "host connection shutdown completed"
        );
    }
    pub(crate) fn load_input_configuration(
        &mut self,
        client: crate::session::host_client::HostClient,
    ) {
        let input = self.input.receiver();
        let cancel = self.cancel.clone();
        self.tasks.push(tokio::spawn(async move{
            let result=tokio::select!{_=cancel.cancelled()=>return,result=client.host_input_configuration()=>result};
            match result{Ok(configuration)=>input.configure(configuration),Err(error)=>tracing::warn!(%error,"host input configuration unavailable; defaults retained")}
        }));
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.input.close();
        self.microphone.close();
        for track in lock(&self.microphone_tracks).drain(..) {
            track.abort();
        }
        for task in &self.tasks {
            task.abort();
        }
        // Normal close has already joined these owners. This also covers a
        // cancelled close future or an exceptional drop by the signal owner.
        let closing = self.core.close_detached();
        let screens = self.screens.clone();
        let activity = self.activity.take();
        tokio::spawn(async move {
            if let Some(closing) = closing {
                let _ = closing.await;
            }
            screens.lock().await.close().await;
            drop(activity);
        });
    }
}

fn maximum_header(
    extensions: &[webrtc::rtp_transceiver::rtp_codec::RTCRtpHeaderExtensionParameters],
    mid: usize,
    color: usize,
    two_byte: bool,
) -> usize {
    let prefix = if two_byte { 2 } else { 1 };
    let size: usize = extensions
        .iter()
        .filter_map(|e| {
            let size = match e.uri.as_str() {
                "urn:ietf:params:rtp-hdrext:sdes:mid" => mid,
                "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01" => 2,
                "http://www.webrtc.org/experiments/rtp-hdrext/color-space" => color,
                "http://www.webrtc.org/experiments/rtp-hdrext/video-capture-index" => 2,
                "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-is-new-frame" => 1,
                "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay" => 3,
                "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time" => 3,
                "urn:ietf:params:rtp-hdrext:toffset" => 3,
                "http://www.webrtc.org/experiments/rtp-hdrext/video-timing" => 13,
                "http://www.webrtc.org/experiments/rtp-hdrext/video-frame-sending-delay" => 2,
                _ => return None,
            };
            (size > 0).then_some(prefix + size)
        })
        .sum();
    12 + if size == 0 {
        0
    } else {
        4 + size.div_ceil(4) * 4
    }
}

// RTP and RTCP SR share one wrapping 90kHz clock. Float-to-u32 casts saturate
// after about 13h15m instead of wrapping, breaking long-running SR mappings.
fn video_timestamp(origin: u32, elapsed_100ns: u64) -> u32 {
    origin.wrapping_add((u128::from(elapsed_100ns) * 9 / 1000) as u32)
}

mod capture_worker;
mod channels;
mod cursor;
mod ingress;
mod media;
pub(crate) mod screens;
use channels::bind_channel;
use channels::publish_state;
