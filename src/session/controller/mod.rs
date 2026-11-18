use crate::account::api::RoomSession;
use crate::account::client::AuthenticatedClient;
use crate::application::viewer::{
    ConnectionProgress, NativeViewerSession, ViewerDisplayHandle, ViewerLaunchConfig,
};
use crate::features::stream_control::StreamControlHandle;
use crate::media::{
    ConnectionMediaOptions, ConnectionMediaProfile, LocalDisplayInfo, VideoCodec,
    detect_local_display,
};
use crate::transport::rtc::{
    ForwardedTrack, IceServer, MediaKind, NativePeer, RtpForwardConfig, RtpForwarder,
};
use crate::transport::signal::{NegotiationEvent, SignalFailure, SignalRole, SignalSession};
use anyhow::{Context, Result, anyhow, bail};
use connection::ResolvedConnection;
pub use connection::{connect_saved_alias, connect_saved_alias_with_progress};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use viewing::run_viewer_window;
pub use viewing::{run_native_viewer_session, run_saved_viewer_window};

pub type ConnectionProgressReporter = Arc<dyn Fn(ConnectionProgress) + Send + Sync>;
pub(crate) fn has_gui_connection(controller: &str, target: &str) -> bool {
    shared::get(&shared::key(controller, target)).is_some()
}
pub(crate) struct LocalConnectionActivity {
    pub viewing: bool,
    pub controlling: bool,
}
pub(crate) fn gui_connection_activity(
    controller: &str,
    target: &str,
) -> Option<LocalConnectionActivity> {
    shared::get(&shared::key(controller, target)).map(|session| session.activity())
}
mod assist;
mod shared;
pub(crate) mod takeover;
pub(crate) mod windows;

fn report_progress(
    reporter: Option<&ConnectionProgressReporter>,
    step: u8,
    title: impl Into<String>,
    detail: impl Into<String>,
) {
    if let Some(reporter) = reporter {
        reporter(ConnectionProgress::working(step, title, detail));
    }
}

pub struct ControllerConnection {
    peer: Arc<NativePeer>,
    forwarder: shared::ForwarderLease,
    profile: ConnectionMediaProfile,
    preference_writer: Option<crate::features::viewing_settings::PreferenceWriter>,
    local_preference_writer: Option<crate::features::viewing_settings::PreferenceWriter>,
}

pub struct ConnectionSummary {
    pub alias: String,
    pub stream_fps: u32,
    pub codec: &'static str,
    pub display_detection_warning: Option<String>,
}

pub struct PlaybackSummary {
    pub track_id: String,
    pub codec: &'static str,
    pub payload_type: u8,
    pub requested_keyframe: bool,
    pub player: &'static str,
}

async fn cancellable<T>(
    cancel: &CancellationToken,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(anyhow!("connection cancelled")),
        result = future => result,
    }
}

fn retry_session_failure(error: &anyhow::Error) -> bool {
    matches!(error.downcast_ref::<SignalFailure>(), Some(failure) if !matches!(failure, SignalFailure::Kicked))
}

fn room_released(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<SignalFailure>(),
        Some(SignalFailure::Kicked)
    )
}

async fn await_media_startup<T>(
    ended: impl std::future::Future<Output = Result<()>>,
    startup: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        result = ended => Err(result.err().unwrap_or_else(|| anyhow!("设备连接已结束"))),
        result = startup => result,
    }
}

impl ControllerConnection {
    async fn close_after_startup_error(self, error: anyhow::Error) -> anyhow::Error {
        // Peer teardown can close RTP/decoder input before the signal task has
        // finished cleanup. Preserve the explicit leave instead of that symptom.
        match self.close().await {
            Err(ended) if room_released(&ended) => ended,
            _ => error,
        }
    }
    pub(crate) async fn connect_mapping(
        client: &AuthenticatedClient,
        device: &crate::account::api::DeviceInfo,
        policy: crate::account::feature_ability::FeaturePolicy,
        options: ConnectionMediaOptions,
        cancel: &CancellationToken,
        takeover: Option<takeover::Approval>,
    ) -> Result<Self> {
        Self::connect_business(
            client,
            device,
            policy,
            options,
            cancel,
            takeover,
            crate::session::negotiation::ControlPurpose::PortMapping,
            false,
        )
        .await
    }
    pub(crate) async fn connect_files(
        client: &AuthenticatedClient,
        device: &crate::account::api::DeviceInfo,
        policy: crate::account::feature_ability::FeaturePolicy,
        options: ConnectionMediaOptions,
        cancel: &CancellationToken,
        takeover: Option<takeover::Approval>,
    ) -> Result<Self> {
        Self::connect_business(
            client,
            device,
            policy,
            options,
            cancel,
            takeover,
            crate::session::negotiation::ControlPurpose::FileTransfer,
            false,
        )
        .await
    }
    /// 终端专用连接。被控端只在 `kind = 8` 的会话上绑定 TERM 帧，所以这条
    /// 连接不能复用观看会话；它也不进 shared 池，关掉时不会动别人的观看。
    pub(crate) async fn connect_terminal(
        client: &AuthenticatedClient,
        device: &crate::account::api::DeviceInfo,
        policy: crate::account::feature_ability::FeaturePolicy,
        options: ConnectionMediaOptions,
        cancel: &CancellationToken,
        takeover: Option<takeover::Approval>,
    ) -> Result<Self> {
        Self::connect_business(
            client,
            device,
            policy,
            options,
            cancel,
            takeover,
            crate::session::negotiation::ControlPurpose::Terminal,
            true,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn connect_business(
        client: &AuthenticatedClient,
        device: &crate::account::api::DeviceInfo,
        policy: crate::account::feature_ability::FeaturePolicy,
        options: ConnectionMediaOptions,
        cancel: &CancellationToken,
        takeover: Option<takeover::Approval>,
        purpose: crate::session::negotiation::ControlPurpose,
        dedicated: bool,
    ) -> Result<Self> {
        let key = shared::key(&client.device_id(), &device.device_id);
        let _gate = shared::acquire_connection(&key, cancel).await?;
        let display = detect_local_display().unwrap_or(LocalDisplayInfo::FALLBACK);
        let mut profile = options.resolve(display)?;
        if let Ok(store) = client.viewing_settings_store(&device.device_id)
            && let Ok(Some(saved)) = store.load().await
            && let Some(settings) = saved.settings
        {
            profile.stream_fps = settings.frame_rate.value(display);
            profile.decoder_fps_cap = display.refresh_hz.max(profile.stream_fps);
        }
        if !dedicated && let Some(session) = shared::get(&key) {
            return Self::from_shared(session, profile, false);
        }
        if device.participant_count() > 0 && takeover.is_none() {
            return Err(takeover::Required(device.clone()).into());
        }
        let force_join = if let Some(approval) = takeover {
            cancellable(cancel, approval.verify(client, &device.device_id)).await?
        } else {
            false
        };
        let room = cancellable(cancel, client.join_device(&device.device_id, force_join))
            .await
            .map_err(|error| {
                if force_join {
                    error.context("接管请求未确认成功，未自动重试；请检查设备状态后重新确认")
                } else {
                    error
                }
            })?;
        let connection = Self::establish(
            room,
            &client.device_id(),
            profile,
            policy,
            crate::session::negotiation::ControlConnectType::Normal,
            options.transport,
            None,
            crate::media::audio::AudioSettings {
                volume: 0,
                muted: true,
            },
            None,
            cancel,
            purpose,
        )
        .await?;
        if !dedicated {
            shared::register(key, &connection.forwarder.session, client.ended());
        }
        Ok(connection)
    }
    fn from_shared(
        session: Arc<shared::Session>,
        profile: ConnectionMediaProfile,
        viewing: bool,
    ) -> Result<Self> {
        Ok(Self {
            peer: Arc::clone(&session.peer),
            forwarder: session.lease(viewing)?,
            profile,
            preference_writer: None,
            local_preference_writer: None,
        })
    }
    async fn activate_viewing(&self) -> Result<()> {
        if self.profile.audio_only {
            return self.start_listening().await;
        }
        self.wait_port_mapping_ready().await?;
        let handle = self.stream_control_handle();
        handle.set_viewing_enabled(true);
        let snapshot = handle.snapshot();
        let screen = snapshot
            .screens
            .first()
            .context("被控端尚未提供显示器列表")?;
        handle.set_screen_capture(screen.id, true).await?;
        handle.set_viewing_enabled(true);
        Ok(())
    }
    async fn start_listening(&self) -> Result<()> {
        let handle = self.stream_control_handle();
        handle.wait_media_control().await?;
        self.peer.pause_video();
        handle.set_video_enabled(false).await?;
        self.forwarder.start_audio();
        handle.audio().start()?;
        self.performance_monitor().set_decoder("仅音频");
        Ok(())
    }
    pub(crate) fn port_mapping_transport(&self) -> Arc<crate::features::port_mapping::Transport> {
        self.peer.port_mapping()
    }
    pub(crate) fn terminal_transport(&self) -> Arc<crate::features::terminal::client::Transport> {
        self.peer.terminal_transport()
    }
    pub(crate) async fn wait_port_mapping_ready(&self) -> Result<()> {
        let control = self.stream_control_handle();
        let changed = control.protocol_notifications();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let wake = changed.notified();
                tokio::pin!(wake);
                wake.as_mut().enable();
                if control.handshake_status().connected {
                    break;
                }
                wake.await;
            }
        })
        .await
        .context("端口转发协议握手未完成")?;
        self.port_mapping_transport().wait_ready().await
    }
    pub fn stream_control_handle(&self) -> StreamControlHandle {
        self.peer.stream_control_handle()
    }

    pub fn performance_monitor(&self) -> crate::diagnostics::performance::PerformanceMonitor {
        self.peer.performance_monitor()
    }

    async fn select_video_track(&mut self) -> Result<(ForwardedTrack, VideoCodec)> {
        let video = tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let snapshot = self.stream_control_handle().snapshot();
                let current = snapshot.remote_display.as_ref().map(|d| d.screen_id);
                let preferred = snapshot.screens.iter()
                    .filter(|s| s.video_track_index >= 0)
                    .min_by_key(|s| (Some(s.id) != current, !s.primary))
                    .map(|s| s.video_track_index);
                if let Some(index) = preferred
                    && let Some(track) = self.forwarder.select_registered_video(index)
                {
                    return Ok::<_, anyhow::Error>(track);
                }
                let track = if preferred.is_none() && let Some(track) = self.forwarder.selected_metadata() {
                    track
                } else {
                    tokio::select! {
                        track = self.forwarder.next_track() => track.ok_or_else(|| anyhow!("remote RTP track channel closed"))?,
                        _ = tokio::time::sleep(Duration::from_millis(20)) => continue,
                    }
                };
                match track.kind {
                    MediaKind::Audio => {}
                    MediaKind::Video if preferred.is_none() || track.id == format!("video_{}", preferred.unwrap()) => return Ok::<_, anyhow::Error>(track),
                    MediaKind::Video => {}
                }
            }
        })
        .await
        .context("remote device sent no video RTP within 12 seconds")??;

        let codec = match video.codec.to_ascii_lowercase() {
            value if value.contains("h265") || value.contains("hevc") => VideoCodec::H265,
            value if value.contains("h264") => VideoCodec::H264,
            value if value.contains("av1") => VideoCodec::Av1,
            _ => bail!(
                "remote device selected unsupported video codec {}",
                video.codec
            ),
        };
        let video_track_index = video
            .id
            .strip_prefix("video_")
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or(0);
        self.peer.set_stream_video_stream(codec, video_track_index);
        Ok((video, codec))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn establish(
        room: RoomSession,
        controller_device_id: &str,
        profile: ConnectionMediaProfile,
        features: crate::account::feature_ability::FeaturePolicy,
        connect_type: crate::session::negotiation::ControlConnectType,
        transport: crate::media::TransportChoice,
        preferences: Option<crate::features::stream_control::StreamControlPreferences>,
        audio_settings: crate::media::audio::AudioSettings,
        reporter: Option<&ConnectionProgressReporter>,
        cancel: &CancellationToken,
        purpose: crate::session::negotiation::ControlPurpose,
    ) -> Result<Self> {
        tracing::info!(?profile, ?transport, "establishing controller connection");
        report_progress(
            reporter,
            4,
            "连接信令服务",
            "正在完成 TLS、WebSocket、Engine.IO 与 Socket.IO 房间握手",
        );
        let mut signal = SignalSession::connect_cancellable(room, SignalRole::Controller, cancel)
            .await
            .context("connect controller signaling")?;
        report_progress(
            reporter,
            4,
            "信令通道已连接",
            "Socket.IO 房间验证完成，保活与断线重连密钥已就绪",
        );
        report_progress(
            reporter,
            5,
            "控制握手",
            format!(
                "正在探测并上报解码能力；帧率上限 {} FPS，{}",
                profile.stream_fps,
                profile.codec.label()
            ),
        );
        let control = match cancellable(
            cancel,
            signal.start_control(
                controller_device_id,
                profile,
                connect_type,
                preferences,
                purpose,
            ),
        )
        .await
        {
            Ok(control) => control,
            Err(error) => {
                let _ = signal.close().await;
                return Err(error).context("complete controller handshake");
            }
        };
        report_progress(
            reporter,
            5,
            "控制握手已接受",
            format!(
                "远端返回 {} 组 ICE 服务；自动切路={}，强制 Relay={}",
                control.ice_servers.len(),
                if control.auto_switch_network {
                    "启用"
                } else {
                    "关闭"
                },
                if control.force_relay { "是" } else { "否" }
            ),
        );
        let transport = if control.force_relay {
            crate::media::TransportChoice::Relay
        } else {
            transport
        };
        report_progress(
            reporter,
            6,
            "创建安全传输栈",
            "正在初始化 ICE、DTLS-SRTP、SCTP 数据通道与 RTP/RTX/FEC 接收链",
        );
        let peer = match NativePeer::new_with_profile(
            control
                .ice_servers
                .iter()
                .map(|server| IceServer {
                    urls: vec![server.urls.clone()],
                    username: server.username.clone(),
                    credential: server.credential.clone(),
                })
                .collect(),
            transport,
            profile,
            purpose == crate::session::negotiation::ControlPurpose::Terminal,
        )
        .await
        {
            Ok(peer) => Arc::new(peer),
            Err(error) => {
                let _ = signal.close().await;
                return Err(error).context("create native WebRTC peer");
            }
        };
        peer.stream_control_handle().set_feature_policy(features);
        peer.stream_control_handle()
            .set_viewing_enabled(purpose == crate::session::negotiation::ControlPurpose::Viewing);
        peer.stream_control_handle()
            .set_display_connection_type(connect_type);
        if let Some(preferences) = preferences
            && let Err(error) = peer
                .stream_control_handle()
                .restore_preferences(preferences)
        {
            let _ = peer.close().await;
            let _ = signal.close().await;
            return Err(error);
        }
        // Restore before media can arrive or the playback window is exposed.
        peer.stream_control_handle()
            .audio()
            .set_settings(audio_settings);
        peer.configure_network_control(transport, control.force_relay);
        report_progress(
            reporter,
            6,
            "安全传输栈已就绪",
            "本地媒体能力与控制数据通道已经创建，等待远端协商",
        );
        let forward_config = RtpForwardConfig {
            video_track_id: None,
        };
        let forwarder = match peer.install_rtp_forwarder(forward_config).await {
            Ok(forwarder) => forwarder,
            Err(error) => {
                let _ = peer.close().await;
                let _ = signal.close().await;
                return Err(error).context("install decrypted RTP forwarder");
            }
        };
        report_progress(
            reporter,
            7,
            "交换媒体能力",
            format!(
                "正在发送 SDP Offer 并协商 {}、RTP 扩展、RTX/FEC 与数据通道",
                profile.codec.label()
            ),
        );
        let answer_installed = AtomicBool::new(false);
        let negotiation_progress = |event| match event {
            NegotiationEvent::OfferCreated {
                sdp_bytes,
                compressed_bytes,
            } => report_progress(
                reporter,
                7,
                "本地媒体能力已生成",
                format!(
                    "SDP {sdp_bytes} 字节，压缩后 {compressed_bytes} 字节；包含视频、RTP 扩展、RTX/FEC 与数据通道"
                ),
            ),
            NegotiationEvent::OfferSent => report_progress(
                reporter,
                7,
                "等待远端媒体答复",
                "SDP Offer 已通过 SOAC 信令发送，正在等待 Answer 与远端候选",
            ),
            NegotiationEvent::LocalCandidateSent { count } => report_progress(
                reporter,
                if answer_installed.load(Ordering::Acquire) {
                    8
                } else {
                    7
                },
                "收集本地网络候选",
                format!("已向远端发送 {count} 个 ICE 候选，LAN/P2P/Relay 探测继续进行"),
            ),
            NegotiationEvent::AnswerInstalled { sdp_bytes } => {
                answer_installed.store(true, Ordering::Release);
                report_progress(
                    reporter,
                    8,
                    "远端媒体能力已确认",
                    format!(
                        "已安装 {sdp_bytes} 字节 SDP Answer，正在检查候选对并完成 DTLS-SRTP 握手"
                    ),
                );
            }
            NegotiationEvent::RemoteCandidateInstalled { count } => report_progress(
                reporter,
                8,
                "判断最佳连接线路",
                format!(
                    "已安装 {count} 个远端 ICE 候选，正在按优先级与连通性选择 LAN、P2P 或 Relay"
                ),
            ),
            NegotiationEvent::Connected => report_progress(
                reporter,
                8,
                "安全媒体通道已建立",
                "ICE 候选对、DTLS 握手与 SRTP 密钥协商均已完成",
            ),
        };
        if let Err(error) = cancellable(
            cancel,
            signal.negotiate(&control, &peer, Some(&negotiation_progress)),
        )
        .await
        {
            let _ = peer.close().await;
            let _ = signal.close().await;
            return Err(error).context("negotiate native WebRTC session");
        }
        let route = peer
            .selected_route_details()
            .await
            .unwrap_or_else(|| "安全媒体通道已连接，候选对详情尚未发布".to_owned());
        report_progress(reporter, 8, "连接线路已选定", route);
        peer.stream_control_handle()
            .network_control()
            .connected(true);
        let (signal_shutdown, signal_shutdown_receiver) = oneshot::channel();
        let signal_peer = Arc::clone(&peer);
        let signal_control = control.clone();
        let signal_task = tokio::spawn(signal.keep_alive(
            signal_shutdown_receiver,
            Some(signal_peer),
            Some(signal_control),
        ));
        tracing::info!("controller WebRTC negotiation complete");
        let session = shared::Session::new(peer, forwarder, signal_shutdown, signal_task);
        Self::from_shared(
            session,
            profile,
            matches!(
                purpose,
                crate::session::negotiation::ControlPurpose::Viewing
                    | crate::session::negotiation::ControlPurpose::Audio
            ),
        )
    }

    pub async fn start_native_viewer(
        &mut self,
        alias: &str,
    ) -> Result<(NativeViewerSession, PlaybackSummary)> {
        let (mut viewer, summary) = self
            .start_native_viewer_with_progress(alias, None, ViewerDisplayHandle::default())
            .await?;
        // The progress-window entry awaits startup separately. This direct
        // API must consume it before run() selects the native presenter.
        tokio::time::timeout(Duration::from_secs(30), viewer.startup())
            .await
            .context("first-frame decoder startup timeout")??;
        Ok((viewer, summary))
    }

    pub(crate) async fn start_native_viewer_with_progress(
        &mut self,
        alias: &str,
        reporter: Option<&ConnectionProgressReporter>,
        display: ViewerDisplayHandle,
    ) -> Result<(NativeViewerSession, PlaybackSummary)> {
        let hardware_decode = self.profile.decoder.mode.hardware();
        report_progress(
            reporter,
            9,
            "等待视频轨道",
            "安全媒体通道已建立，正在等待远端发布桌面视频轨道",
        );
        let (video, codec) = self.select_video_track().await?;
        report_progress(
            reporter,
            10,
            "视频轨道已协商",
            format!(
                "{} · RTP PT {} · SSRC {} · 轨道 {}",
                match codec {
                    VideoCodec::H264 => "H.264/AVC",
                    VideoCodec::H265 => "H.265/HEVC",
                    VideoCodec::Av1 => "AV1",
                },
                video.payload_type,
                video.ssrc,
                video.id
            ),
        );
        let profile = self.profile;
        let track_index = video
            .id
            .strip_prefix("video_")
            .and_then(|id| id.parse().ok())
            .unwrap_or(0);
        let performance = self
            .peer
            .performance_monitor()
            .for_video_track(track_index as u64);
        performance.set_video_codec(match codec {
            VideoCodec::H264 => format!("H.264/AVC · RTP PT {}", video.payload_type),
            VideoCodec::H265 => format!("H.265/HEVC · RTP PT {}", video.payload_type),
            VideoCodec::Av1 => format!("AV1 · RTP PT {}", video.payload_type),
        });
        let stream_control = self.peer.stream_control_handle();
        let receiver_feedback = self.forwarder.video_receiver_feedback();
        let title = format!("{}{alias}", crate::VIEWER_TITLE_PREFIX);
        report_progress(
            reporter,
            11,
            "初始化视频解码器",
            format!(
                "正在打开 {} {} 解码路径，实际画面尺寸以收到的码流为准",
                if hardware_decode {
                    "平台硬件优先"
                } else {
                    "软件"
                },
                match codec {
                    VideoCodec::H264 => "H.264",
                    VideoCodec::H265 => "H.265",
                    VideoCodec::Av1 => "AV1",
                },
            ),
        );
        let mut viewer = NativeViewerSession::launch(ViewerLaunchConfig {
            codec,
            decoder: profile.decoder,
            title,
            initial_width: profile.local_display.width,
            initial_height: profile.local_display.height,
            frame_rate: profile.stream_fps,
            receiver_feedback,
            performance: performance.clone(),
            stream_control,
            display,
        })
        .await?;
        viewer.attach_screen_playback(&self.peer, profile, alias, track_index);

        report_progress(
            reporter,
            11,
            "视频解码器已就绪",
            format!(
                "{} · {}",
                performance.snapshot().decoder,
                if hardware_decode {
                    "允许平台硬解，失败时由解码层报告实际后端"
                } else {
                    "已按用户设置禁用硬解优先"
                }
            ),
        );
        self.forwarder.add_video_sink(viewer.video_sink()).await;
        viewer.ensure_running()?;
        let keyframe_generation = self.forwarder.video_keyframe_generation();
        self.forwarder.start();
        if let Err(error) = self.peer.stream_control_handle().audio().start() {
            tracing::warn!(%error, "native audio did not start; video remains available");
        }
        report_progress(
            reporter,
            12,
            "同步首个完整画面",
            "已启动 RTP 接收，正在发送 PLI 并等待参数集、完整关键帧与首帧解码",
        );
        self.peer.request_keyframe(video.ssrc).await?;
        let keyframe_ready = self
            .forwarder
            .wait_for_video_keyframe_after(keyframe_generation, Duration::from_secs(2))
            .await;
        viewer.ensure_running()?;
        if !keyframe_ready {
            tracing::warn!("PLI 后 2 秒内未识别到完整参数集和关键帧，已继续等待后续关键帧");
            report_progress(
                reporter,
                12,
                "继续等待关键帧",
                "远端尚未在 2 秒内返回完整关键帧；接收链保持运行并继续请求恢复",
            );
        } else {
            report_progress(
                reporter,
                12,
                "首个完整画面已到达",
                "参数集与关键帧已通过接收门禁，播放器可以开始显示",
            );
        }
        let summary = PlaybackSummary {
            track_id: video.id,
            codec: match codec {
                VideoCodec::H264 => "H.264",
                VideoCodec::H265 => "H.265",
                VideoCodec::Av1 => "AV1",
            },
            payload_type: video.payload_type,
            requested_keyframe: true,
            player: "原生窗口",
        };

        Ok((viewer, summary))
    }

    pub async fn keep_alive(mut self, mut shutdown: oneshot::Receiver<()>) -> Result<()> {
        let result = tokio::select! {
            result = self.forwarder.session.ended() => result,
            _ = &mut shutdown => Ok(()),
        };
        if let Some(writer) = &mut self.preference_writer {
            writer.finish().await;
        }
        if let Some(writer) = &mut self.local_preference_writer {
            writer.finish().await;
        }
        let closed = self.close().await;
        result.and(closed)
    }

    pub async fn close(mut self) -> Result<()> {
        if self.forwarder.viewing {
            let handle = self.stream_control_handle();
            handle.mouse().disable();
            handle.audio().suspend();
            // Local capture and clipboard permissions end before any network
            // wait. A shared file/port session must not keep a closing viewer's
            // microphone alive while screen-stop messages are backpressured.
            handle.set_viewing_enabled(false);
            if Arc::strong_count(&self.forwarder.session) > 1 {
                for screen in handle.snapshot().screens {
                    let _ = handle.set_screen_capture(screen.id, false).await;
                }
            }
        }
        let result = if Arc::strong_count(&self.forwarder.session) == 1 {
            self.forwarder.session.request_close();
            self.forwarder.session.ended().await
        } else {
            Ok(())
        };
        if let Some(writer) = &mut self.preference_writer {
            writer.finish().await;
        }
        if let Some(writer) = &mut self.local_preference_writer {
            writer.finish().await;
        }
        result
    }
}

fn flatten_signal_task(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    result.context("controller signaling task failed")?
}

mod connection;
mod viewing;
