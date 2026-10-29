//! Viewing a host reached directly, without the server
//! (`docs/protocol/v2-design.md` §12): connect to its direct endpoint,
//! negotiate as usual, and recover a lost session by connecting again.
use super::{
    ConnectionProgressReporter, ControllerConnection, RECOVERY_LIMIT, Recovery, cancellable,
    recovery, recovery_backoff, report_progress, viewing, windows,
};
use crate::application::viewer::{ConnectionProgress, ViewerDisplayHandle, ViewerWindowEvent};
use crate::i18n::t;
use crate::media::{ConnectionMediaOptions, ConnectionMediaProfile, LocalDisplayInfo, detect_local_display};
use crate::transport::direct::{self, Target, client::Auth};
use anyhow::{Context as _, Result};
use farwing_protocol::api::Relation;
use farwing_protocol::direct::DirectHello;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

/// A host address and how to authorise there.
#[derive(Clone, Debug)]
pub struct DirectRequest {
    pub(crate) target: Target,
    pub(crate) auth: Auth,
    /// The host's name, when known before connecting (a nearby device).
    pub(crate) name: Option<String>,
    /// The device chosen (a nearby or known device): whatever answers at
    /// the address, only it is connected to.
    pub(crate) expected: Option<String>,
}

impl DirectRequest {
    /// `address` with the host's access code, or `ask` to have the host's
    /// user confirm; otherwise by this device's key (own device, friend,
    /// trusted device).
    pub fn parse(address: &str, code: Option<String>, ask: bool) -> Result<Self> {
        let target = Target::parse(address)?;
        let auth = match code {
            Some(code) => Auth::Code(Some(code)),
            None if ask => Auth::Code(None),
            None => Auth::Key,
        };
        Ok(Self {
            target,
            auth,
            name: None,
            expected: None,
        })
    }

    pub fn named(mut self, name: &str) -> Self {
        self.name = Some(name.to_owned()).filter(|name| !name.is_empty());
        self
    }

    pub fn expecting(mut self, device_id: &str) -> Self {
        self.expected = Some(device_id.to_owned()).filter(|id| !id.is_empty());
        self
    }

    pub fn label(&self) -> String {
        self.target.to_string()
    }

    /// What the viewer window is called.
    pub fn title(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.label())
    }
}

/// The settings store of directly reached hosts (not tied to an account).
const SETTINGS_OWNER: &str = "direct";

/// Standalone viewer for a host reached directly (`connect --direct`).
pub async fn run_direct_viewer_window(
    request: DirectRequest,
    options: ConnectionMediaOptions,
) -> Result<()> {
    viewing::run_viewer_window_with(request.label(), options, None, None, Some(request), None).await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_owner(
    alias: String,
    options: ConnectionMediaOptions,
    request: DirectRequest,
    sender: std::sync::mpsc::Sender<ViewerWindowEvent>,
    display: oneshot::Receiver<ViewerDisplayHandle>,
    monitor: watch::Sender<Option<crate::diagnostics::performance::PerformanceMonitor>>,
    target: watch::Sender<Option<windows::ViewerTarget>>,
    cancel: &CancellationToken,
    reporter: &mut ConnectionProgressReporter,
) -> Result<()> {
    report_progress(Some(reporter), 1, t().session.progress_preparing_direct, "");
    let local = direct::local_device()?;
    let display_info = detect_local_display().unwrap_or(LocalDisplayInfo::FALLBACK);
    let profile = options.resolve(display_info)?;
    let mut display = cancellable(cancel, async {
        display.await.context("player display was not created")
    })
    .await?;
    let mut alias = alias;
    let visit = super::record::new_visit();
    let record = super::record::VisitRecord::start(monitor.subscribe());
    let started_at = chrono::Utc::now().timestamp();
    let result = async {
    let mut recovering = None::<std::time::Instant>;
    let mut attempt = 0u32;
    let mut preferences = None;
    let mut audio = None;
    loop {
        monitor.send_replace(None);
        let connected = connect_once(
            &request,
            &alias,
            &local,
            profile,
            options.transport,
            preferences,
            audio,
            &visit,
            reporter,
            cancel,
        )
        .await;
        let (mut controller, host, relation) = match connected {
            Ok(connected) => connected,
            Err(error)
                if !cancel.is_cancelled()
                    && recovering.is_some_and(|since| since.elapsed() < RECOVERY_LIMIT)
                    && recovery(&error) != Recovery::Fatal =>
            {
                attempt += 1;
                wait(&error, attempt, recovering.expect("checked above"), reporter, cancel).await?;
                continue;
            }
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), target = %request.target, "direct connection failed");
                return Err(error);
            }
        };
        alias = host.name.clone();
        target.send_replace(Some(windows::ViewerTarget {
            device_id: host.device_id.clone(),
            alias: alias.clone(),
        }));
        monitor.send_replace(Some(controller.performance_monitor()));
        let session_control = controller.stream_control_handle();
        session_control.mouse().set_host_input_allowed(true);
        record.connected(&session_control);
        record.bind(
            None,
            &visit,
            &host.device_id,
            &local.identity.device_id,
            Some(farwing_protocol::api::DirectRecord {
                started_at,
                relation,
                host_name: host.name.clone(),
                controller_name: local.name.clone(),
                controller_platform: farwing_protocol::Platform::Windows,
                ..farwing_protocol::api::DirectRecord::default()
            }),
        );
        let close = match viewing::present_video(
            &mut controller,
            &alias,
            display,
            reporter,
            cancel,
            &sender,
            None,
        )
        .await
        {
            Ok(close) => close,
            Err(error) => {
                let error = controller.close_after_startup_error(error).await;
                if cancel.is_cancelled() || recovery(&error) == Recovery::Fatal {
                    return Err(error);
                }
                attempt += 1;
                if recovering.is_none() {
                    record.reconnecting();
                }
                recovering.get_or_insert_with(std::time::Instant::now);
                display = viewing::replace_room(
                    &sender,
                    &alias,
                    None,
                    ConnectionProgress::working(4, t().session.progress_reconnecting, format!("{error:#}")),
                    None,
                    reporter,
                    cancel,
                )
                .await?;
                continue;
            }
        };
        if let Some(since) = recovering.take() {
            tracing::info!(elapsed_ms = since.elapsed().as_millis(), "direct session recovered");
        }
        attempt = 0;
        let alive = controller.forwarder.session.end_waiter();
        let result = tokio::select! {
            result = alive => result,
            _ = cancel.cancelled() => Ok(()),
        };
        preferences = Some(session_control.preferences());
        audio = Some(session_control.audio().settings());
        let result = result.and(controller.close().await);
        if cancel.is_cancelled() {
            close.close();
            return Ok(());
        }
        match result {
            Err(error) if recovery(&error) != Recovery::Fatal => {
                attempt += 1;
                if recovering.is_none() {
                    record.reconnecting();
                }
                recovering.get_or_insert_with(std::time::Instant::now);
                display = viewing::replace_room(
                    &sender,
                    &alias,
                    None,
                    ConnectionProgress::working(4, t().session.progress_reconnecting, format!("{error:#}")),
                    None,
                    reporter,
                    cancel,
                )
                .await?;
            }
            result => {
                close.close();
                return result;
            }
        }
    }
    }
    .await;
    record.finish().await;
    result
}

/// A direct session without a window or playback, for scripted input
/// (`dev input`).
pub(crate) async fn connect_headless(
    request: &DirectRequest,
    options: ConnectionMediaOptions,
    cancel: &CancellationToken,
) -> Result<(ControllerConnection, DirectHello)> {
    let local = direct::local_device()?;
    let display = detect_local_display().unwrap_or(LocalDisplayInfo::FALLBACK);
    let profile = options.resolve(display)?;
    let reporter: ConnectionProgressReporter = Arc::new(|_| {});
    connect_once(
        request,
        &request.label(),
        &local,
        profile,
        options.transport,
        None,
        Some(crate::media::audio::AudioSettings {
            volume: 0,
            muted: true,
        }),
        "",
        &reporter,
        cancel,
    )
    .await
    .map(|(connection, host, _)| (connection, host))
}

/// Connect to the host and negotiate one session.
#[allow(clippy::too_many_arguments)]
async fn connect_once(
    request: &DirectRequest,
    alias: &str,
    local: &direct::LocalDevice,
    profile: ConnectionMediaProfile,
    transport: crate::media::TransportChoice,
    preferences: Option<crate::features::stream_control::StreamControlPreferences>,
    audio: Option<crate::media::audio::AudioSettings>,
    visit: &str,
    reporter: &ConnectionProgressReporter,
    cancel: &CancellationToken,
) -> Result<(ControllerConnection, DirectHello, Relation)> {
    report_progress(
        Some(reporter),
        3,
        t().session.progress_connecting_direct,
        (t().session.progress_connecting_to_address)(alias, &request.target.to_string()),
    );
    let connected = direct::client::connect(
        &request.target,
        local,
        request.auth.clone(),
        request.expected.as_deref(),
        cancel,
    )
    .await?;
    let host = connected.host.clone();
    report_progress(
        Some(reporter),
        4,
        t().session.progress_direct_authorized,
        format!(
            "{} · {}",
            host.name,
            match connected.relation {
                Relation::Own => t().session.relation_own,
                Relation::Trusted => t().session.relation_trusted,
                Relation::Assist => t().session.relation_guest,
                Relation::Friend => t().session.relation_friend,
            },
        ),
    );
    let store = crate::features::viewing_settings::ViewingSettingsStore::new(SETTINGS_OWNER, &host.device_id).ok();
    let mut preferences = preferences;
    let mut audio = audio;
    if let Some(store) = &store {
        if preferences.is_none()
            && let Ok(Some(saved)) = store.load().await
        {
            preferences = Some(
                crate::features::stream_control::StreamControlPreferences::from_saved(saved, profile),
            );
        }
        if audio.is_none() {
            audio = store.load_audio().await.ok().flatten();
        }
        // The saved weak-network strategy, cinema and input choices go with
        // the session request.
        if let Ok(device) = store.load_device().await {
            preferences
                .get_or_insert_with(|| {
                    crate::features::stream_control::StreamControlPreferences::from_saved(
                        crate::features::stream_control::default_settings(profile),
                        profile,
                    )
                })
                .device
                .get_or_insert(device);
        }
    }
    let mut audio = audio.unwrap_or(crate::media::audio::AudioSettings {
        volume: 100,
        muted: false,
    });
    audio.muted |= profile.muted;
    let mut signal = connected.signal;
    signal.set_visit(visit);
    let mut connection = ControllerConnection::establish_on(
        signal,
        crate::transport::signal::identity::Local {
            identity: local.identity.clone(),
            pake: connected.pake_key,
        },
        profile,
        crate::account::feature_ability::FeaturePolicy::direct(),
        transport,
        preferences,
        audio,
        Some(reporter),
        cancel,
        crate::session::negotiation::ControlPurpose::Viewing,
    )
    .await?;
    let handle = connection.stream_control_handle();
    handle.mouse().set_keyboard_platform(host.platform);
    if let Some(store) = store {
        store.restore_device(&handle).await;
        connection.preference_writer = Some(store.clone().bind(handle.clone()));
        connection.local_preference_writer = Some(store.bind_local_preferences(handle));
    }
    Ok((connection, host, connected.relation))
}

/// Before connecting again: wait for the host (it may be restarting) or back off.
async fn wait(
    error: &anyhow::Error,
    attempt: u32,
    since: std::time::Instant,
    reporter: &ConnectionProgressReporter,
    cancel: &CancellationToken,
) -> Result<()> {
    anyhow::ensure!(
        since.elapsed() < RECOVERY_LIMIT,
        "{}",
        (t().session.error_host_not_back)(RECOVERY_LIMIT.as_secs() / 60)
    );
    let delay = if recovery(error) == Recovery::WaitDevice {
        Duration::from_secs(3)
    } else {
        recovery_backoff(attempt)
    };
    tracing::info!(attempt, delay_ms = delay.as_millis(), %error, "reconnecting the direct session after a delay");
    let retry = crate::application::viewer::RetryNow::default();
    reporter(ConnectionProgress::working(
        if recovery(error) == Recovery::WaitDevice { 1 } else { 4 },
        if recovery(error) == Recovery::WaitDevice {
            t().session.progress_waiting_host
        } else {
            t().session.progress_reconnecting
        },
        (t().session.progress_retry_in)(&format!("{error:#}"), delay.as_secs(), attempt),
    ).with_retry(&retry));
    cancellable(cancel, async {
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = retry.requested() => tracing::info!("reconnecting now at the user's request"),
        }
        Ok(())
    })
    .await
}

