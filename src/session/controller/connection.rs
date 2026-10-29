//! Target resolution, restored preferences and connection setup.
use super::{
    ConnectionProgressReporter, ConnectionSummary, ControllerConnection, assist,
    await_media_startup, cancellable, report_progress, shared,
};
use crate::account::api::{ApiFailure, ErrorCode};
use crate::account::client::AuthenticatedClient;
use crate::application::viewer::ConnectionProgress;
use crate::i18n::t;
use crate::media::{
    ConnectionMediaOptions, ConnectionMediaProfile, LocalDisplayInfo, detect_local_display,
};
use anyhow::{Context as _, Result, bail};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub(super) struct ResolvedConnection {
    pub(super) client: Arc<AuthenticatedClient>,
    /// This connection's record (`docs/connection-records.md`): its
    /// sessions, reconnections included, make one.
    pub(super) visit: String,
    pub(super) target_device_id: String,
    pub(super) controller_device_id: String,
    pub(super) profile: ConnectionMediaProfile,
    pub(super) default_codec: crate::media::CodecPreference,
    pub(super) transport: crate::media::TransportChoice,
    pub(super) summary: ConnectionSummary,
    pub(super) assist: Option<assist::AssistConnection>,
    pub(super) preferences: Option<crate::features::stream_control::StreamControlPreferences>,
    pub(super) audio_preferences: Option<crate::media::audio::AudioSettings>,
    pub(super) target_platform: farwing_protocol::Platform,
    pub(super) refresh_after_upgrade: bool,
    pub(super) background: Option<crate::application::wallpaper::Source>,
}

impl ResolvedConnection {
    async fn wait_for_updated_target(
        &self,
        reporter: Option<&ConnectionProgressReporter>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        // Only a previously authenticated ReportError(-6) enables this wait.
        // It polls ownership/readiness and never reissues an installation.
        // Closing the retained viewer cancels the wait immediately.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        let operation = async {
            let mut readiness = crate::features::remote_upgrade::UpdateReadiness::default();
            loop {
                let devices = self.client.list_devices().await?;
                let device = devices
                    .devices
                    .iter()
                    .find(|d| d.device_id == self.target_device_id)
                    .context(t().session.error_updated_device_missing)?;
                device.validated_device_id()?;
                anyhow::ensure!(device.platform == farwing_protocol::Platform::Windows, "{}", t().session.error_updated_platform_changed);
                if readiness.observe(
                    device.is_connected() && device.controlled_support && device.controllable,
                    std::time::Instant::now(),
                ) {
                    tracing::info!(
                        "updated target advertises readiness; attempting media connection"
                    );
                    return Ok(());
                }
                report_progress(reporter, 1, t().session.progress_waiting_update, "");
                tokio::time::sleep(crate::features::remote_upgrade::UPDATE_PROBE_INTERVAL).await;
            }
        };
        cancellable(cancel, async {
            tokio::time::timeout_at(deadline, operation)
                .await
                .context(t().session.error_update_wait_timeout)?
        })
        .await
    }
    pub(super) async fn connect(
        &mut self,
        reporter: Option<&ConnectionProgressReporter>,
        cancel: &CancellationToken,
        retries: &mut u32,
    ) -> Result<ControllerConnection> {
        let key = shared::key(&self.controller_device_id, &self.target_device_id);
        let _gate = shared::acquire_connection(&key, cancel).await?;
        if self.refresh_after_upgrade && self.assist.is_none() {
            self.wait_for_updated_target(reporter, cancel).await?;
        }
        if self.assist.is_none()
            && let Some(session) = shared::get(&key)
        {
            let mut connection = ControllerConnection::from_shared(session, self.profile, true)?;
            let session = Arc::clone(&connection.forwarder.session);
            let setup = async {
                let handle = connection.stream_control_handle();
                // A file/port session can be created before any viewer. Its
                // input platform is still unset until viewing takes ownership.
                handle.mouse().set_keyboard_platform(self.target_platform);
                let store = self.client.viewing_settings_store(&self.target_device_id)?;
                if let Some(saved) = store.load().await? {
                    let preferences =
                        crate::features::stream_control::StreamControlPreferences::from_saved(
                            saved,
                            self.profile,
                        );
                    if !handle.snapshot().ready {
                        let _ = handle.restore_preferences(preferences);
                    }
                }
                let mut audio =
                    store
                        .load_audio()
                        .await?
                        .unwrap_or(crate::media::audio::AudioSettings {
                            volume: 100,
                            muted: false,
                        });
                audio.muted |= self.profile.muted;
                handle.audio().set_settings(audio);
                handle.set_feature_policy(
                    self.client
                        .feature_catalog()
                        .policy(&self.target_device_id),
                );
                if self.target_platform == farwing_protocol::Platform::Windows {
                    handle.set_remote_upgrade(crate::features::remote_upgrade::RemoteUpgrade::new(cancel));
                }
                store.restore_device(&handle).await;
                connection.preference_writer = Some(store.clone().bind(handle.clone()));
                connection.local_preference_writer = Some(store.bind_local_preferences(handle));
                connection.activate_viewing().await
            };
            if let Err(error) =
                cancellable(cancel, await_media_startup(session.ended(), setup)).await
            {
                drop(session);
                return Err(connection.close_after_startup_error(error).await);
            }
            return Ok(connection);
        }
        if self.target_device_id == self.controller_device_id {
            bail!("{}", t().session.error_connect_to_self);
        }
        // Diagnostics: `FARWING_SIGNAL_PRESENCE=wait` lets a standalone
        // viewer's own presence connection come up first, so that the session
        // rides on it (§11).
        if std::env::var("FARWING_SIGNAL_PRESENCE").is_ok_and(|v| v.eq_ignore_ascii_case("wait")) {
            let ready = crate::transport::signal::wait_presence(
                &self.controller_device_id,
                self.client.user_id(),
                Duration::from_secs(10),
            )
            .await;
            tracing::info!(ready, "waited for the presence connection");
        }
        tracing::info!("creating server session");
        report_progress(reporter, 3, t().session.progress_creating_session, "");
        let mut pake_key = None;
        let room = if let Some(assist) = &mut self.assist {
            let reply = assist
                .join(&self.client, &self.controller_device_id, &self.visit, reporter, cancel)
                .await?;
            pake_key = reply.pake_key;
            self.target_device_id = reply.host.device_id.clone();
            self.target_platform = reply.host.platform;
            if !reply.host.alias.is_empty() {
                self.summary.alias = reply.host.alias.clone();
            }
            reply.ticket
        } else {
            loop {
                match cancellable(
                    cancel,
                    self.client.join_device(
                        &self.target_device_id,
                        if self.profile.audio_only {
                            crate::account::api::Purpose::Audio
                        } else {
                            crate::account::api::Purpose::Desktop
                        },
                        &self.visit,
                    ),
                )
                .await
                {
                    Ok(room) => break room,
                    Err(error) => {
                        // Only retry outcomes the server marks as transient.
                        let retryable = error.downcast_ref::<ApiFailure>().is_some_and(|failure| {
                            failure.code.retryable() && failure.code != ErrorCode::Network
                                || matches!(failure.code, ErrorCode::Conflict)
                        });
                        if cancel.is_cancelled() || !retryable || *retries >= 5 {
                            return Err(error);
                        }
                        *retries += 1;
                        report_progress(
                            reporter,
                            3,
                            t().session.progress_requesting_session_again,
                            (t().session.progress_session_retry)(&error.to_string(), *retries),
                        );
                        cancellable(cancel, async {
                            tokio::time::sleep(Duration::from_secs(3)).await;
                            Ok(())
                        })
                        .await?;
                    }
                }
            }
        };
        if self.refresh_after_upgrade {
            // Rejoining the room establishes that the target is back online.
            // Refresh its real version before rebuilding feature policy.
            let devices = cancellable(cancel, self.client.list_devices()).await?;
            let device = devices
                .devices
                .iter()
                .find(|device| device.device_id == self.target_device_id)
                .context(t().session.error_updated_device_missing)?;
            anyhow::ensure!(device.platform == farwing_protocol::Platform::Windows, "{}", t().session.error_updated_platform_changed);
            self.refresh_after_upgrade = false;
        }
        // Server policy may cap the video bitrate of this session.
        let bitrate_limit = room.bitrate_limit_mbps;
        let mut persistence_error = None;
        let store = match self.client.viewing_settings_store(&self.target_device_id) {
            Ok(store) => Some(store),
            Err(error) => {
                persistence_error = Some(error.to_string());
                None
            }
        };
        self.profile.codec = self.default_codec;
        // This device's saved choices (weak-network strategy, cinema, input)
        // go with the session request: the host starts with them.
        let mut saved_device = None;
        if let Some(store) = &store {
            match cancellable(cancel, store.load_device()).await {
                Ok(saved) => {
                    if !self.default_codec.restricted()
                        && let Some(codec) = saved.codec
                    {
                        self.profile.codec = codec;
                    }
                    saved_device = Some(saved);
                }
                Err(error) if cancel.is_cancelled() => return Err(error),
                Err(error) => persistence_error = Some(error.to_string()),
            }
        }
        if self.preferences.is_none()
            && let Some(store) = &store
        {
            match cancellable(cancel, store.load()).await {
                Ok(Some(settings)) => {
                    self.preferences = Some(
                        crate::features::stream_control::StreamControlPreferences::from_saved(
                            settings,
                            self.profile,
                        ),
                    )
                }
                Ok(None) => {}
                Err(error) if cancel.is_cancelled() => return Err(error),
                Err(error) => persistence_error = Some(error.to_string()),
            }
        }
        if let Some(device) = saved_device {
            let profile = self.profile;
            let preferences = self.preferences.get_or_insert_with(|| {
                crate::features::stream_control::StreamControlPreferences::from_saved(
                    crate::features::stream_control::default_settings(profile),
                    profile,
                )
            });
            preferences.device.get_or_insert(device);
        }
        if let Some(preferences) = self.preferences.as_mut() {
            preferences.custom_bitrate_limit = bitrate_limit;
        }
        let mut audio_persistence_error = None;
        if self.audio_preferences.is_none() {
            let mut audio = crate::media::audio::AudioSettings {
                volume: 100,
                muted: false,
            };
            if let Some(store) = &store {
                match cancellable(cancel, store.load_audio()).await {
                    Ok(Some(saved)) => {
                        tracing::debug!(?saved, "loaded audio settings for this device");
                        audio = saved;
                    }
                    Ok(None) => {}
                    Err(error) if cancel.is_cancelled() => return Err(error),
                    Err(error) => audio_persistence_error = Some(error.to_string()),
                }
            }
            // Startup --mute is temporary; only subsequent user edits persist.
            audio.muted |= self.profile.muted;
            self.audio_preferences = Some(audio);
        }
        if let Some(preferences) = self.preferences {
            self.profile.stream_fps = preferences
                .settings
                .frame_rate
                .value(self.profile.local_display);
            self.profile.decoder_fps_cap = self
                .profile
                .local_display
                .refresh_hz
                .max(self.profile.stream_fps);
        }

        let local = crate::transport::signal::identity::Local {
            identity: self.client.client_identity()?,
            pake: pake_key,
        };
        anyhow::ensure!(
            local.identity.device_id == self.controller_device_id,
            "{}",
            t().session.error_local_identity_changed
        );
        let session_id = room.session_id.clone();
        let mut connection = ControllerConnection::establish(
            room,
            local,
            self.profile,
            self.client
                .feature_catalog()
                .policy(&self.target_device_id),
            self.transport,
            self.preferences,
            self.audio_preferences.expect("resolved audio settings"),
            reporter,
            cancel,
            if self.profile.audio_only {
                crate::session::negotiation::ControlPurpose::Audio
            } else {
                crate::session::negotiation::ControlPurpose::Viewing
            },
        )
        .await?;
        connection.follow_forwarding(&self.client, session_id).await;
        let handle = connection.stream_control_handle();
        if self.assist.is_none() {
            shared::register(key, &connection.forwarder.session, self.client.ended());
        }
        if self.assist.is_none() && self.target_platform == farwing_protocol::Platform::Windows {
            handle.set_remote_upgrade(crate::features::remote_upgrade::RemoteUpgrade::new(cancel));
        }
        handle.set_custom_bitrate_limit(bitrate_limit);
        handle.mouse().set_keyboard_platform(self.target_platform);
        handle.set_persistence_error(persistence_error);
        handle.set_audio_persistence_error(audio_persistence_error);
        connection.preference_writer = store.clone().map(|store| store.bind(handle.clone()));
        if let Some(store) = &store {
            store.restore_device(&handle).await;
            match store.load_microphone().await {
                Ok(quality) => {
                    handle.microphone().set_quality(quality)?;
                }
                Err(error) => handle.set_audio_persistence_error(Some(error.to_string())),
            }
        }
        connection.local_preference_writer =
            store.map(|store| store.bind_local_preferences(handle));
        Ok(connection)
    }
}

pub async fn connect_saved_alias(
    alias: &str,
    options: ConnectionMediaOptions,
) -> Result<(ControllerConnection, ConnectionSummary)> {
    connect_saved_alias_with_progress(alias, options, None).await
}

pub async fn connect_saved_alias_with_progress(
    alias: &str,
    options: ConnectionMediaOptions,
    reporter: Option<&ConnectionProgressReporter>,
) -> Result<(ControllerConnection, ConnectionSummary)> {
    let mut resolved = resolve_saved_connection(alias, options, reporter).await?;
    let mut retries = 0;
    let connection = resolved
        .connect(reporter, &CancellationToken::new(), &mut retries)
        .await;
    resolved.client.close().await;
    Ok((connection?, resolved.summary))
}

pub(super) async fn resolve_saved_connection(
    alias: &str,
    options: ConnectionMediaOptions,
    reporter: Option<&ConnectionProgressReporter>,
) -> Result<ResolvedConnection> {
    let client = Arc::new(AuthenticatedClient::from_saved_session()?);
    let result =
        resolve_connection_with_client(Arc::clone(&client), alias, options, reporter, None).await;
    if result.is_err() {
        client.close().await;
    }
    result
}

pub(super) async fn resolve_connection_with_client(
    client: Arc<AuthenticatedClient>,
    alias: &str,
    options: ConnectionMediaOptions,
    reporter: Option<&ConnectionProgressReporter>,
    target_id: Option<&str>,
) -> Result<ResolvedConnection> {
    report_progress(reporter, 1, t().session.progress_reading_local, "");
    report_progress(reporter, 2, t().session.progress_checking_device, "");
    let devices = client.list_devices().await?;
    if let Some(id) = target_id {
        crate::account::api::validate_device_id(id)?;
    }
    if target_id.map_or(devices.current_device.alias == alias, |id| {
        devices.current_device.device_id == id
    }) {
        bail!("{}", t().session.error_connect_to_self);
    }

    let matches = devices
        .devices
        .iter()
        .chain(&devices.shared)
        .filter(|device| target_id.map_or(device.alias == alias, |id| device.device_id == id))
        .collect::<Vec<_>>();
    let device = match matches.as_slice() {
        [] => bail!("{}", (t().session.error_no_device_named)(alias)),
        [device] => *device,
        _ => bail!("{}", (t().session.error_several_devices_named)(alias)),
    };
    let background =
        crate::application::wallpaper::Source::new(&device.device_id, &device.wallpaper_url);
    if let Some(reporter) = reporter {
        reporter(ConnectionProgress::background(background.clone()));
    }
    if !device.platform.is_desktop() {
        bail!("{}", t().session.error_management_only_device);
    }
    if !device.is_connected() {
        bail!("{}", (t().session.error_device_offline)(alias));
    }
    if !device.controlled_support || !device.controllable {
        bail!("{}", (t().session.error_device_not_controllable)(alias));
    }
    if device.is_shared() && !device.granted(farwing_protocol::caps::VIEW) {
        bail!("{}", (t().session.error_device_not_shared_for_viewing)(alias));
    }
    let _gate = shared::connection_gate(&shared::key(&client.device_id(), &device.device_id))
        .lock_owned()
        .await;

    let display = detect_local_display().unwrap_or_else(|error| {
        tracing::warn!(error = %format!("{error:#}"), "local display not detected; using 1080p60");
        LocalDisplayInfo::FALLBACK
    });
    let profile = options.resolve(display)?;
    report_progress(
        reporter,
        2,
        t().session.progress_device_available,
        (t().session.progress_local_display)(
            display.width,
            display.height,
            display.refresh_hz,
            profile.stream_fps,
            profile.codec.label(),
        ),
    );
    let target_device_id = device.validated_device_id()?.to_owned();
    let controller_device_id = devices.current_device.validated_device_id()?.to_owned();
    let summary = ConnectionSummary {
        alias: device.alias.clone(),
    };
    Ok(ResolvedConnection {
        client,
        visit: super::record::new_visit(),
        target_device_id,
        controller_device_id,
        profile,
        default_codec: profile.codec,
        transport: options.transport,
        summary,
        assist: None,
        preferences: None,
        audio_preferences: None,
        target_platform: device.platform,
        refresh_after_upgrade: false,
        background: Some(background),
    })
}
