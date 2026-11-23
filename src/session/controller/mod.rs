use crate::account::api::RealtimeTicket;
use crate::account::client::AuthenticatedClient;
#[cfg(windows)]
use crate::application::viewer::{
    ConnectionProgress, NativeViewerSession, ViewerDisplayHandle, ViewerLaunchConfig,
};
#[cfg(target_os = "linux")]
use crate::connection_progress::ConnectionProgress;
use crate::features::stream_control::StreamControlHandle;
use crate::i18n::t;
use crate::media::{ConnectionMediaOptions, ConnectionMediaProfile, LocalDisplayInfo, VideoCodec};
#[cfg(windows)]
use crate::media::detect_local_display;
use crate::transport::rtc::{
    ForwardedTrack, IceServer, MediaKind, NativePeer, RtpForwardConfig, RtpForwarder,
};
use crate::transport::signal::{NegotiationEvent, SignalFailure, SignalRole, SignalSession};
use anyhow::{Context, Result, anyhow, bail};
#[cfg(windows)]
use connection::ResolvedConnection;
#[cfg(windows)]
pub use connection::{connect_saved_alias, connect_saved_alias_with_progress};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
#[cfg(windows)]
pub use direct::{DirectRequest, run_direct_viewer_window};
#[cfg(windows)]
pub use viewing::{run_assist_viewer_window, run_native_viewer_session, run_saved_viewer_window};
#[cfg(windows)]
pub(crate) use assist::target_label as assist_target_label;

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
#[cfg(windows)]
mod assist;
#[cfg(windows)]
pub(crate) mod direct;
#[cfg(windows)]
mod record;
mod shared;
#[cfg(target_os = "linux")]
pub(crate) use shared::shutdown_all;
#[cfg(windows)]
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
}

pub struct PlaybackSummary {
    pub track_id: String,
    pub codec: &'static str,
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
    matches!(error.downcast_ref::<SignalFailure>(), Some(failure) if !matches!(failure, SignalFailure::Kicked(_)))
}

/// How a lost or failed session is recovered (`docs/protocol/v2-design.md`
/// §11): rebuilt with backoff, or after waiting for the device to come back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recovery {
    Retry,
    WaitDevice,
    Fatal,
}

fn recovery(error: &anyhow::Error) -> Recovery {
    use crate::account::api::ApiFailure;
    use farwing_protocol::error::ErrorCode;
    for cause in error.chain() {
        if let Some(failure) = cause.downcast_ref::<SignalFailure>() {
            return match failure {
                // The host restarting or quitting comes back on its own.
                SignalFailure::Kicked(reason)
                    if reason == farwing_protocol::realtime::reason::HOST_CLOSED =>
                {
                    Recovery::WaitDevice
                }
                // The host lost this connection: build a new one.
                SignalFailure::Kicked(reason)
                    if reason == farwing_protocol::realtime::reason::CONNECTION_LOST =>
                {
                    Recovery::Retry
                }
                SignalFailure::Kicked(_) => Recovery::Fatal,
                SignalFailure::PublisherDisconnected => Recovery::WaitDevice,
                // Only the server's busy relay may free up.
                SignalFailure::NoRelay(reason)
                    if reason != farwing_protocol::realtime::relay_denied::BUSY =>
                {
                    Recovery::Fatal
                }
                _ => Recovery::Retry,
            };
        }
        if let Some(failure) = cause.downcast_ref::<ApiFailure>() {
            return match failure.code {
                ErrorCode::DeviceOffline => Recovery::WaitDevice,
                ErrorCode::Network | ErrorCode::Busy | ErrorCode::Internal => Recovery::Retry,
                _ => Recovery::Fatal,
            };
        }
    }
    Recovery::Fatal
}

/// Recovery of an assistance session. Its target is not in the device list
/// and its connect ID is unknown while the host restarts, so waiting for the
/// device becomes rebuilding with backoff (the saved code is reused).
fn assist_recovery(error: &anyhow::Error) -> Recovery {
    use crate::account::api::ApiFailure;
    use farwing_protocol::error::ErrorCode;
    match recovery(error) {
        Recovery::WaitDevice => Recovery::Retry,
        Recovery::Fatal
            if error.chain().any(|cause| {
                cause.downcast_ref::<ApiFailure>().is_some_and(|failure| {
                    matches!(failure.code, ErrorCode::NotFound | ErrorCode::DeviceOffline)
                })
            }) =>
        {
            Recovery::Retry
        }
        other => other,
    }
}

/// How a lost session of this kind recovers.
fn recovery_for(error: &anyhow::Error, assist: bool) -> Recovery {
    if assist { assist_recovery(error) } else { recovery(error) }
}

/// The viewer keeps recovering a lost session this long.
const RECOVERY_LIMIT: Duration = Duration::from_secs(300);

/// Delay before rebuild attempt `attempt` (1-based): 1, 2, 4, 8, then 15 s.
fn recovery_backoff(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.saturating_sub(1).min(4)).min(Duration::from_secs(15))
}

fn room_released(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<SignalFailure>(),
        Some(SignalFailure::Kicked(_))
    )
}

/// The same controller device took this session over in another window.
fn room_replaced(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<SignalFailure>(),
        Some(SignalFailure::Kicked(reason)) if reason == farwing_protocol::realtime::reason::REPLACED
    )
}

async fn await_media_startup<T>(
    ended: impl std::future::Future<Output = Result<()>>,
    startup: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        result = ended => Err(result.err().unwrap_or_else(|| anyhow!("{}", t().session.error_connection_ended))),
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
    ) -> Result<Self> {
        Self::connect_business(
            client,
            device,
            policy,
            options,
            cancel,
            crate::session::negotiation::ControlPurpose::PortMapping,
        )
        .await
    }
    pub(crate) async fn connect_files(
        client: &AuthenticatedClient,
        device: &crate::account::api::DeviceInfo,
        policy: crate::account::feature_ability::FeaturePolicy,
        options: ConnectionMediaOptions,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        Self::connect_business(
            client,
            device,
            policy,
            options,
            cancel,
            crate::session::negotiation::ControlPurpose::FileTransfer,
        )
        .await
    }
    /// A session for the host's diagnostic bundle: needs only the
    /// diagnostics permission.
    pub(crate) async fn connect_diagnostics(
        client: &AuthenticatedClient,
        device: &crate::account::api::DeviceInfo,
        policy: crate::account::feature_ability::FeaturePolicy,
        options: ConnectionMediaOptions,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        Self::connect_business(
            client,
            device,
            policy,
            options,
            cancel,
            crate::session::negotiation::ControlPurpose::Diagnostics,
        )
        .await
    }
    /// A viewing session without a window or playback, for scripted input
    /// (`dev input`). Video is negotiated but no screen is captured.
    pub(crate) async fn connect_headless(
        client: &AuthenticatedClient,
        device: &crate::account::api::DeviceInfo,
        policy: crate::account::feature_ability::FeaturePolicy,
        options: ConnectionMediaOptions,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let connection = Self::connect_business(
            client,
            device,
            policy,
            options,
            cancel,
            crate::session::negotiation::ControlPurpose::Viewing,
        )
        .await?;
        connection
            .stream_control_handle()
            .mouse()
            .set_keyboard_platform(device.platform);
        Ok(connection)
    }
    /// A session for the host's cameras only, without a window.
    pub(crate) async fn connect_headless_camera(
        client: &AuthenticatedClient,
        device: &crate::account::api::DeviceInfo,
        policy: crate::account::feature_ability::FeaturePolicy,
        options: ConnectionMediaOptions,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        Self::connect_business(
            client,
            device,
            policy,
            options,
            cancel,
            crate::session::negotiation::ControlPurpose::Camera,
        )
        .await
    }
    async fn connect_business(
        client: &AuthenticatedClient,
        device: &crate::account::api::DeviceInfo,
        policy: crate::account::feature_ability::FeaturePolicy,
        options: ConnectionMediaOptions,
        cancel: &CancellationToken,
        purpose: crate::session::negotiation::ControlPurpose,
    ) -> Result<Self> {
        let key = shared::key(&client.device_id(), &device.device_id);
        let _gate = shared::acquire_connection(&key, cancel).await?;
        #[cfg(windows)]
        let display = detect_local_display().unwrap_or(LocalDisplayInfo::FALLBACK);
        #[cfg(target_os = "linux")]
        let display = LocalDisplayInfo::FALLBACK;
        let mut profile = options.resolve(display)?;
        if let Ok(store) = client.viewing_settings_store(&device.device_id)
            && let Ok(Some(settings)) = store.load().await
        {
            profile.stream_fps = settings.frame_rate.value(display);
            profile.decoder_fps_cap = display.refresh_hz.max(profile.stream_fps);
        }
        if let Some(session) = shared::get(&key) {
            return Self::from_shared(session, profile, false);
        }
        let room = cancellable(cancel, client.join_device(&device.device_id, purpose.wire(), ""))
            .await?;
        let session_id = room.session_id.clone();
        let connection = Self::establish(
            room,
            crate::transport::signal::identity::Local::new(client.client_identity()?),
            profile,
            policy,
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
        shared::register(key, &connection.forwarder.session, client.ended());
        // The host may forward the picture through the server (§5.4 of
        // `docs/protocol/deployment-design.md`).
        connection.follow_forwarding(client, session_id).await;
        Ok(connection)
    }
    /// Follow the host's forwarding instructions for server session
    /// `session_id` (`docs/protocol/deployment-design.md` §5.4).
    pub(super) async fn follow_forwarding(
        &self,
        client: &AuthenticatedClient,
        session_id: Option<String>,
    ) {
        let Some(session) = session_id else { return };
        match client.request(|api| async move { Ok(api) }).await {
            Ok(api) => self.peer.start_forwarding(api, session),
            Err(error) => tracing::warn!(%error, "no server API for forwarding"),
        }
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
    /// A camera session shows the host's first camera that can be opened.
    async fn open_first_camera(&self) -> Result<()> {
        let handle = self.stream_control_handle();
        handle.wait_media_control().await?;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let cameras = handle.cameras();
            if let Some(camera) = cameras.iter().find(|c| c.unavailable.is_empty()) {
                return handle.open_camera(camera.id).await;
            }
            if let Some(camera) = cameras.first() {
                anyhow::bail!("{}", camera.unavailable);
            }
            anyhow::ensure!(std::time::Instant::now() < deadline, "{}", t().camera.not_found);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    async fn activate_viewing(&self) -> Result<()> {
        if self.profile.audio_only {
            return self.start_listening().await;
        }
        if self.profile.camera_only {
            return self.open_first_camera().await;
        }
        self.wait_port_mapping_ready().await?;
        let handle = self.stream_control_handle();
        handle.set_viewing_enabled(true);
        let snapshot = handle.snapshot();
        let screen = snapshot
            .screens
            .first()
            .context(t().session.error_no_display_list)?;
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
        self.performance_monitor().set_decoder(t().session.audio_only);
        Ok(())
    }
    pub(crate) fn port_mapping_transport(&self) -> Arc<crate::features::port_mapping::Transport> {
        self.peer.port_mapping()
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
        .context(t().session.error_port_forwarding_not_ready)?;
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
        room: RealtimeTicket,
        local: crate::transport::signal::identity::Local,
        profile: ConnectionMediaProfile,
        features: crate::account::feature_ability::FeaturePolicy,
        transport: crate::media::TransportChoice,
        preferences: Option<crate::features::stream_control::StreamControlPreferences>,
        audio_settings: crate::media::audio::AudioSettings,
        reporter: Option<&ConnectionProgressReporter>,
        cancel: &CancellationToken,
        purpose: crate::session::negotiation::ControlPurpose,
    ) -> Result<Self> {
        tracing::info!(?profile, ?transport, via_presence = room.via_presence, "establishing controller connection");
        let signal = if room.via_presence {
            // The session rides on this process's presence connection (§11).
            let sid = room.session_id.as_deref().context(t().session.error_session_without_id)?;
            report_progress(reporter, 4, t().session.progress_connecting_server, "");
            SignalSession::carried(&local.identity.device_id, sid)
                .context("signal over the presence connection")?
        } else {
            report_progress(reporter, 4, t().session.progress_connecting_server, "");
            SignalSession::connect_cancellable(room, SignalRole::Controller, cancel)
                .await
                .context("connect controller signaling")?
        };
        report_progress(reporter, 4, t().session.progress_server_connected, "");
        Self::establish_on(
            signal,
            local,
            profile,
            features,
            transport,
            preferences,
            audio_settings,
            reporter,
            cancel,
            purpose,
        )
        .await
    }

    /// Negotiate a session on an open signalling connection: the server's,
    /// or a host's direct endpoint (`docs/protocol/v2-design.md` §12).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn establish_on(
        mut signal: SignalSession,
        local: crate::transport::signal::identity::Local,
        profile: ConnectionMediaProfile,
        features: crate::account::feature_ability::FeaturePolicy,
        transport: crate::media::TransportChoice,
        preferences: Option<crate::features::stream_control::StreamControlPreferences>,
        audio_settings: crate::media::audio::AudioSettings,
        reporter: Option<&ConnectionProgressReporter>,
        cancel: &CancellationToken,
        purpose: crate::session::negotiation::ControlPurpose,
    ) -> Result<Self> {
        report_progress(
            reporter,
            5,
            t().session.progress_requesting,
            format!("{} FPS · {}", profile.stream_fps, profile.codec.label()),
        );
        let control = match cancellable(
            cancel,
            signal.start_control(local, profile, preferences, purpose),
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
            t().session.progress_accepted,
            if control.force_relay { t().session.progress_relay_only } else { "" },
        );
        let transport = if control.force_relay {
            crate::media::TransportChoice::Relay
        } else if transport == crate::media::TransportChoice::Relay && !control.relay.allowed {
            // This viewer prefers the relay but the session may not use it.
            crate::media::TransportChoice::Auto
        } else {
            transport
        };
        report_progress(reporter, 6, t().session.progress_preparing, "");
        let peer = match NativePeer::new_with_profile(
            control
                .ice_servers
                .iter()
                .map(|server| IceServer {
                    urls: server.urls.clone(),
                    username: server.username.clone(),
                    credential: server.credential.clone(),
                })
                .collect(),
            transport,
            profile,
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
        peer.stream_control_handle().set_offer(&control.offer);
        peer.stream_control_handle()
            .set_viewing_enabled(matches!(
                purpose,
                crate::session::negotiation::ControlPurpose::Viewing
                    | crate::session::negotiation::ControlPurpose::Camera
            ));
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
        peer.configure_network_control(
            transport,
            control.force_relay,
            crate::features::network_control::relay_denied(&control.relay),
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
        report_progress(reporter, 7, t().session.progress_negotiating, "");
        let answer_installed = AtomicBool::new(false);
        // One line per stage: later candidates add no row of their own.
        let negotiation_progress = |event| match event {
            NegotiationEvent::OfferSent => report_progress(reporter, 7, t().session.progress_waiting_response, ""),
            NegotiationEvent::LocalCandidateSent { count } => {
                if count == 1 {
                    report_progress(
                        reporter,
                        if answer_installed.load(Ordering::Acquire) {
                            8
                        } else {
                            7
                        },
                        t().session.progress_finding_route,
                        "",
                    );
                }
            }
            NegotiationEvent::AnswerInstalled => {
                answer_installed.store(true, Ordering::Release);
                report_progress(reporter, 8, t().session.progress_responded, "");
            }
            NegotiationEvent::RemoteCandidateInstalled { count } => {
                if count == 1 {
                    report_progress(reporter, 8, t().session.progress_choosing_route, "");
                }
            }
            NegotiationEvent::Connected => report_progress(reporter, 8, t().session.progress_connected, ""),
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
            .unwrap_or_default();
        report_progress(reporter, 8, t().session.progress_route_chosen, route);
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
                    | crate::session::negotiation::ControlPurpose::Camera
            ),
        )
    }

    #[cfg(windows)]
    pub(crate) async fn start_native_viewer_with_progress(
        &mut self,
        alias: &str,
        reporter: Option<&ConnectionProgressReporter>,
        display: ViewerDisplayHandle,
    ) -> Result<(NativeViewerSession, PlaybackSummary)> {
        let hardware_decode = self.profile.decoder.mode.hardware();
        report_progress(reporter, 9, t().session.progress_preparing_picture, "");
        if self.profile.camera_only {
            self.open_first_camera().await?;
        }
        let (video, codec) = self.select_video_track().await?;
        report_progress(
            reporter,
            10,
            t().session.progress_format_chosen,
            match codec {
                VideoCodec::H264 => "H.264/AVC",
                VideoCodec::H265 => "H.265/HEVC",
                VideoCodec::Av1 => "AV1",
            },
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
            t().session.progress_preparing_decoder,
            format!(
                "{} · {}",
                match codec {
                    VideoCodec::H264 => "H.264",
                    VideoCodec::H265 => "H.265",
                    VideoCodec::Av1 => "AV1",
                },
                if hardware_decode {
                    t().session.hardware_decoding_preferred
                } else {
                    t().session.software_decoding
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
            t().session.progress_decoder_ready,
            performance.snapshot().decoder.clone(),
        );
        self.forwarder.add_video_sink(viewer.video_sink()).await;
        viewer.ensure_running()?;
        let keyframe_generation = self.forwarder.video_keyframe_generation();
        self.forwarder.start();
        if let Err(error) = self.peer.stream_control_handle().audio().start() {
            tracing::warn!(%error, "native audio did not start; video remains available");
        }
        report_progress(reporter, 12, t().session.progress_waiting_picture, "");
        self.peer.request_keyframe(video.ssrc).await?;
        let keyframe_ready = self
            .forwarder
            .wait_for_video_keyframe_after(keyframe_generation, Duration::from_secs(2))
            .await;
        viewer.ensure_running()?;
        if !keyframe_ready {
            tracing::warn!("PLI 后 2 秒内未识别到完整参数集和关键帧，已继续等待后续关键帧");
            report_progress(reporter, 12, t().session.progress_still_waiting_picture, "");
        } else {
            report_progress(reporter, 12, t().session.progress_picture_received, "");
        }
        let summary = PlaybackSummary {
            track_id: video.id,
            codec: match codec {
                VideoCodec::H264 => "H.264",
                VideoCodec::H265 => "H.265",
                VideoCodec::Av1 => "AV1",
            },
        };

        Ok((viewer, summary))
    }

    #[cfg(target_os = "linux")]
    pub(crate) async fn start_linux_viewer(
        &mut self,
        ctx: egui::Context,
    ) -> Result<crate::application::viewer::Playback> {
        let control = self.stream_control_handle();
        let startup = async {
            control.wait_media_control().await?;
            let changed = control.protocol_notifications();
            loop {
                let notified = changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if let Some(screen) = control.snapshot().screens.into_iter()
                    .min_by_key(|screen| !screen.primary)
                {
                    break Ok::<_, anyhow::Error>(screen.id);
                }
                notified.await;
            }
        };
        let screen = tokio::time::timeout(Duration::from_secs(10), startup)
            .await.context(t().session.error_no_display_list)??;
        control.set_screen_capture(screen, true).await?;
        let (track, codec) = self.select_video_track().await?;
        let playback = crate::application::viewer::Playback::start(
            ctx,
            codec,
            self.profile,
            self.peer.performance_monitor(),
            self.forwarder.video_receiver_feedback(),
            control.clone(),
            screen,
        )?;
        self.forwarder.add_video_sink(playback.sink()).await;
        self.forwarder.start();
        control.audio().set_settings(crate::media::audio::AudioSettings { volume: 100, muted: false });
        if let Err(error) = control.audio().start() {
            tracing::warn!(%error, "could not start native audio playback");
        }
        self.peer.request_keyframe(track.ssrc).await?;
        Ok(playback)
    }

    /// `dev video`: capture the host's first screen and decode it without a
    /// window. Every decoded frame goes to `observer`.
    #[cfg(windows)]
    pub(crate) async fn start_headless_video(
        &mut self,
        observer: std::sync::mpsc::Sender<crate::application::viewer::dev_video::DecodedObservation>,
    ) -> Result<(
        crate::application::viewer::dev_video::HeadlessDecoder,
        VideoCodec,
        crate::diagnostics::performance::PerformanceMonitor,
    )> {
        use crate::application::viewer::dev_video::{HeadlessDecoder, HeadlessDecoderConfig};
        let handle = self.stream_control_handle();
        handle.wait_media_control().await?;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while handle.snapshot().screens.is_empty() {
            anyhow::ensure!(std::time::Instant::now() < deadline, "{}", t().session.error_no_display_list);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.activate_viewing().await?;
        let (video, codec) = self.select_video_track().await?;
        let track_index: u64 = video
            .id
            .strip_prefix("video_")
            .and_then(|id| id.parse().ok())
            .unwrap_or(0);
        let performance = self.peer.performance_monitor().for_video_track(track_index);
        let decoder = HeadlessDecoder::start(HeadlessDecoderConfig {
            codec,
            decoder: self.profile.decoder,
            size: (self.profile.local_display.width, self.profile.local_display.height),
            frame_rate: self.profile.stream_fps,
            receiver_feedback: self.forwarder.video_receiver_feedback(),
            performance: performance.clone(),
            observer,
        });
        self.forwarder.add_video_sink(decoder.sink()).await;
        self.forwarder.start();
        self.peer.request_keyframe(video.ssrc).await?;
        Ok((decoder, codec, performance))
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

#[cfg(windows)]
mod connection;
#[cfg(windows)]
mod viewing;

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_fifteen_seconds() {
        let delays: Vec<_> = (1..=7).map(|n| recovery_backoff(n).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 15, 15, 15]);
    }

    #[test]
    fn failures_map_to_recovery() {
        use crate::account::api::ApiFailure;
        use farwing_protocol::error::ErrorCode;
        let offline = anyhow::Error::new(ApiFailure::new(ErrorCode::DeviceOffline, ""))
            .context("create session");
        assert_eq!(recovery(&offline), Recovery::WaitDevice);
        let network = anyhow::Error::new(ApiFailure::new(ErrorCode::Network, ""));
        assert_eq!(recovery(&network), Recovery::Retry);
        let kicked = anyhow::Error::new(SignalFailure::Kicked(String::new()));
        assert_eq!(recovery(&kicked), Recovery::Fatal);
        let gone = anyhow::Error::new(SignalFailure::PublisherDisconnected);
        assert_eq!(recovery(&gone), Recovery::WaitDevice);
        assert_eq!(recovery(&anyhow::anyhow!("other")), Recovery::Fatal);
    }

    #[test]
    fn assistance_rebuilds_while_the_host_restarts() {
        use crate::account::api::ApiFailure;
        use farwing_protocol::error::ErrorCode;
        let unknown = anyhow::Error::new(ApiFailure::new(ErrorCode::NotFound, ""));
        assert_eq!(recovery_for(&unknown, false), Recovery::Fatal);
        assert_eq!(recovery_for(&unknown, true), Recovery::Retry);
        let gone = anyhow::Error::new(SignalFailure::PublisherDisconnected);
        assert_eq!(recovery_for(&gone, true), Recovery::Retry);
        let kicked = anyhow::Error::new(SignalFailure::Kicked(String::new()));
        assert_eq!(recovery_for(&kicked, true), Recovery::Fatal);
        let denied = anyhow::Error::new(ApiFailure::new(ErrorCode::Forbidden, ""));
        assert_eq!(recovery_for(&denied, true), Recovery::Fatal);
    }
}
