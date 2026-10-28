//! Viewing-window attachment, reconnect ownership and cancellation.
use super::connection::{ResolvedConnection, resolve_connection_with_client};
use super::{
    ConnectionProgressReporter, ControllerConnection, RECOVERY_LIMIT, Recovery, assist,
    await_media_startup, cancellable, recovery_backoff, recovery_for, retry_session_failure,
    room_released, room_replaced, windows,
};
use crate::account::client::AuthenticatedClient;
use crate::application::viewer::{
    ConnectionProgress, NativeViewerSession, ViewerDisplayHandle, ViewerWindowEvent,
    run_connecting_viewer_window,
};
use crate::i18n::t;
use crate::media::ConnectionMediaOptions;
use anyhow::{Context as _, Result, anyhow};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub async fn run_saved_viewer_window(
    alias: String,
    options: ConnectionMediaOptions,
    target_id: Option<String>,
) -> Result<()> {
    run_viewer_window(alias, options, target_id, None, None).await
}

/// Standalone viewer for a remote-assistance target (`assist` command).
pub async fn run_assist_viewer_window(
    alias: String,
    options: ConnectionMediaOptions,
    request: crate::account::assist::AssistRequest,
) -> Result<()> {
    run_viewer_window(alias, options, None, Some(request), None).await
}

pub(super) async fn run_viewer_window(
    alias: String,
    options: ConnectionMediaOptions,
    target_id: Option<String>,
    assist: Option<crate::account::assist::AssistRequest>,
    hosted: Option<windows::WindowContext>,
) -> Result<()> {
    run_viewer_window_with(alias, options, target_id, assist, None, hosted).await
}

/// [`run_viewer_window`], optionally for a host reached directly.
pub(super) async fn run_viewer_window_with(
    alias: String,
    options: ConnectionMediaOptions,
    target_id: Option<String>,
    assist: Option<crate::account::assist::AssistRequest>,
    direct: Option<super::direct::DirectRequest>,
    hosted: Option<windows::WindowContext>,
) -> Result<()> {
    let owns_presence = hosted.is_none() && direct.is_none();
    let background = hosted.as_ref().and_then(|h| h.background.clone());
    let window_key = hosted.as_ref().map(|h| h.key.clone());
    let (progress_sender, progress_receiver) = std::sync::mpsc::channel();
    let (viewer_sender, viewer_receiver) = std::sync::mpsc::channel();
    let reporter: ConnectionProgressReporter = Arc::new(move |progress| {
        let _ = progress_sender.send(progress);
    });
    if let Some(background) =
        background.filter(|s| target_id.as_deref() == Some(s.device_id.as_str()))
    {
        reporter(ConnectionProgress::background(background));
    }
    let task_alias = alias.clone();
    let (display_sender, display_receiver) = oneshot::channel();
    let cancel = hosted
        .as_ref()
        .map(|h| h.cancel.clone())
        .unwrap_or_default();
    let owner_cancel = cancel.clone();
    let close_sender = viewer_sender.clone();
    let (monitor_sender, _monitor_receiver) = hosted
        .as_ref()
        .map(|h| (h.monitor.clone(), h.monitor.subscribe()))
        .unwrap_or_else(|| tokio::sync::watch::channel(None));
    let (target_sender, _target_receiver) = hosted
        .as_ref()
        .map(|h| (h.target.clone(), h.target.subscribe()))
        .unwrap_or_else(|| tokio::sync::watch::channel(None));
    let owner_task = tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = owner_cancel.cancelled() => {let _=close_sender.send(ViewerWindowEvent::Close);},
            _ = async {
                if let Err(error) = tokio::signal::ctrl_c().await {
                    tracing::warn!(%error, "Ctrl+C listener unavailable");
                    std::future::pending::<()>().await;
                }
            } => {
                owner_cancel.cancel();
                let _ = close_sender.send(ViewerWindowEvent::Close);
            },
        }
    });
    let task_cancel = cancel.clone();
    let terminal_sender = viewer_sender.clone();
    let terminal_alias = alias.clone();
    let connection_task = tokio::spawn(async move {
        let mut reporter = reporter;
        let result = run_viewer_connection_owner(
            task_alias,
            options,
            ViewerConnectionWindow {
                sender: viewer_sender,
                display: display_receiver,
                owns_presence,
                monitor: monitor_sender,
                target: target_sender,
                target_id,
                assist,
                direct,
                client: hosted.and_then(|h| h.client),
            },
            &task_cancel,
            &mut reporter,
        )
        .await;
        if !task_cancel.is_cancelled()
            && let Err(error) = &result
        {
            if room_replaced(error) {
                // The same device reconnected in another window: this one
                // closes without an error.
                let _ = terminal_sender.send(ViewerWindowEvent::Close);
            } else if room_released(error) {
                // Ended by the other side (kicked, access revoked, ...): say
                // why in this window instead of letting it vanish.
                tracing::info!(reason = %error, "viewer session ended by the other side");
                let (progress_tx, progress_rx) = std::sync::mpsc::channel();
                let (display_tx, display_rx) = oneshot::channel();
                if terminal_sender
                    .send(ViewerWindowEvent::Reconnect {
                        alias: terminal_alias,
                        window: None,
                        progress: progress_rx,
                        display: display_tx,
                    })
                    .is_ok()
                {
                    let _ = progress_tx.send(ConnectionProgress::ended(format!("{error}")));
                    // The window fails a reconnect nobody acknowledges; take
                    // its display (and drop it) so the notice stays up.
                    tokio::spawn(async move {
                        let _ = display_rx.await;
                    });
                }
            } else {
                tracing::warn!(error = %format!("{error:#}"), "viewer connection failed");
                reporter(ConnectionProgress::from_error(error));
            }
        }
        result
    });

    let window_result = if let Some(key) = window_key {
        crate::ui::window_manager::viewer(
            key,
            crate::application::viewer::windows_presenter::ConnectingWindowsRunConfig {
                alias,
                progress: progress_receiver,
                session: viewer_receiver,
                display_sender,
            },
        )
        .await
    } else {
        tokio::task::block_in_place(|| {
            run_connecting_viewer_window(alias, progress_receiver, viewer_receiver, display_sender)
        })
    };
    cancel.cancel();
    let _ = owner_task.await;
    // The owner observes cancellation in every network wait and joins cleanup;
    // aborting this task would discard an in-flight room/peer owner.
    let connection_result = connection_task
        .await
        .context("controller connection task stopped unexpectedly")?;
    if let Err(error) = &connection_result {
        tracing::debug!(%error, "connection owner finished");
    }
    window_result.and(connection_result)
}

pub(super) struct ViewerConnectionWindow {
    pub(super) client: Option<Arc<AuthenticatedClient>>,
    pub(super) sender: std::sync::mpsc::Sender<ViewerWindowEvent>,
    pub(super) display: oneshot::Receiver<ViewerDisplayHandle>,
    pub(super) owns_presence: bool,
    pub(super) monitor:
        tokio::sync::watch::Sender<Option<crate::diagnostics::performance::PerformanceMonitor>>,
    pub(super) target: tokio::sync::watch::Sender<Option<windows::ViewerTarget>>,
    pub(super) target_id: Option<String>,
    pub(super) assist: Option<crate::account::assist::AssistRequest>,
    pub(super) direct: Option<super::direct::DirectRequest>,
}

pub(super) async fn run_viewer_connection_owner(
    alias: String,
    options: ConnectionMediaOptions,
    window: ViewerConnectionWindow,
    cancel: &CancellationToken,
    reporter: &mut ConnectionProgressReporter,
) -> Result<()> {
    let ViewerConnectionWindow {
        sender: viewer_sender,
        display: display_receiver,
        owns_presence,
        monitor,
        target,
        target_id,
        assist,
        direct,
        client: hosted_client,
    } = window;
    if let Some(request) = direct {
        // No account and no server: the direct endpoint is the session owner.
        return super::direct::run_owner(
            alias,
            options,
            request,
            viewer_sender,
            display_receiver,
            monitor,
            target,
            cancel,
            reporter,
        )
        .await;
    }
    let presence_stop = CancellationToken::new();
    let mut presence_task = None;
    let mut account_owner = None;
    let mut record = super::record::VisitRecord::start(monitor.subscribe());
    let result = async {
        let client = match hosted_client.clone() {
            Some(client) => client,
            None => match AuthenticatedClient::from_saved_session() {
                Ok(client) => Arc::new(client),
                // Without an account this device can still ask a host to
                // admit it (trusted device or visitor).
                Err(error)
                    if assist.is_some()
                        && error
                            .downcast_ref::<crate::account::client::NoSavedSession>()
                            .is_some() =>
                {
                    Arc::new(AuthenticatedClient::guest_with_runtime()?)
                }
                Err(error) => return Err(error),
            },
        };
        account_owner = Some(Arc::clone(&client));
        let mut resolved = if let Some(request) = assist {
            cancellable(cancel, assist::resolve(client, &alias, options, request, Some(reporter))).await?
        } else {
            cancellable(cancel, resolve_connection_with_client(client, &alias, options, Some(reporter), target_id.as_deref())).await?
        };
        // Standalone processes keep the host presence room (设备在线状态；
        // 被控权限开关已从界面移除，不开放被控). The account-ended token
        // still closes the viewer so a revoked session cannot keep watching.
        if owns_presence && !resolved.client.is_guest() {
            let client = Arc::clone(&resolved.client);
            let stop = presence_stop.clone();
            let cancel = cancel.clone();
            let sender = viewer_sender.clone();
            presence_task = Some(tokio::spawn(async move {
                let ended = client.ended();
                let presence = crate::session::presence::ActivePresence::start(client);
                let mut tick = tokio::time::interval(Duration::from_millis(250));
                loop {
                    tokio::select! {
                        biased;
                        _ = ended.cancelled() => {
                            cancel.cancel();
                            let _ = sender.send(ViewerWindowEvent::Close);
                            break;
                        },
                        _ = stop.cancelled() => break,
                        _ = cancel.cancelled() => break,
                        _ = tick.tick() => {
                            while let Ok(event) = presence.events.try_recv() {
                                if let crate::session::presence::PresenceEvent::Warning(message) = event {
                                    tracing::warn!(%message, "standalone device presence");
                                }
                            }
                        }
                    }
                }
                presence.close().await;
            }));
        }
        let mut display = cancellable(cancel, async { display_receiver.await.context("player display was not created") }).await?;
        let (switch_sender, mut switch_receiver) = tokio::sync::mpsc::channel::<crate::application::viewer::device_switch::SwitchRequest>(1);
        let mut retries = 0;
        let mut update_started = None;
        // Set while a lost session is being recovered.
        let mut recovering = None::<std::time::Instant>;
        loop {
            monitor.send_replace(None);
            target.send_replace(Some(windows::ViewerTarget {
                device_id: resolved.target_device_id.clone(), alias: resolved.summary.alias.clone(),
            }));
            let update_attempt = resolved.refresh_after_upgrade;
            let mut controller = match resolved.connect(Some(reporter), cancel, &mut retries).await {
                Ok(controller) => controller,
                // A session lost earlier keeps recovering: rebuild with
                // backoff, or wait for the device, for up to five minutes.
                Err(error) if !cancel.is_cancelled()
                    && !update_attempt
                    && recovering.is_some_and(|since: std::time::Instant| since.elapsed() < RECOVERY_LIMIT)
                    && recovery_for(&error, resolved.assist.is_some()) != Recovery::Fatal =>
                {
                    retries += 1;
                    let since = recovering.expect("checked above");
                    wait_before_rebuild(&resolved, &error, retries, since, reporter, cancel).await?;
                    continue;
                }
                Err(error) if !cancel.is_cancelled() && retry_session_failure(&error) && retries < 5 => {
                    retries += 1;
                    resolved.refresh_after_upgrade |= update_attempt;
                    reporter(ConnectionProgress::working(4, t().session.progress_reconnecting, (t().session.progress_retry_count)(&format!("{error:#}"), retries)));
                    if update_attempt {
                        cancellable(cancel,async {tokio::time::sleep(crate::features::remote_upgrade::UPDATE_PROBE_INTERVAL).await;Ok(())}).await?;
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            // Reset the full-session retry budget on peer connected.
            monitor.send_replace(Some(controller.performance_monitor()));
            // During an update, a connected peer is not enough: retain the
            // retry budget until media startup succeeds, otherwise repeated
            // disconnects before the first frame could reset it indefinitely.
            if !update_attempt { retries = 0; }
            let switcher = crate::application::viewer::device_switch::DeviceSwitcher::new(
                Arc::clone(&resolved.client), resolved.target_device_id.clone(), switch_sender.clone(), cancel.clone());
            let session_control = controller.stream_control_handle();
            record.connected(&session_control);
            if let Some((on, off)) = crate::features::stream_control::take_dev_audio_switch() {
                let control = session_control.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(on)).await;
                    tracing::info!("dev: audio only");
                    control.request_audio_only(true);
                    tokio::time::sleep(Duration::from_secs(off - on)).await;
                    tracing::info!("dev: the picture again");
                    control.request_audio_only(false);
                });
            }
            record.bind(
                Some(Arc::clone(&resolved.client)),
                &resolved.visit,
                &resolved.target_device_id,
                &resolved.controller_device_id,
                None,
            );
            // Marked host input is valid when controlling another device from
            // this desktop. Keep it out of an actual local loopback session.
            session_control.mouse().set_host_input_allowed(
                resolved.target_device_id != resolved.client.device_id());
            session_control.request_audio_only(resolved.profile.audio_only);
            let mut modes = session_control.audio_mode_requests();
            let mut audio_only = resolved.profile.audio_only;
            let startup = if audio_only {
                cancellable(cancel, await_media_startup(controller.forwarder.session.end_waiter(), controller.start_listening())).await.map(|_| None)
            } else {
                present_video(&mut controller, &resolved.summary.alias, display, reporter, cancel, &viewer_sender, Some(&switcher)).await.map(Some)
            };
            let mut close=match startup {
                Ok(close)=>close,
                Err(error)=>{
                    let error=controller.close_after_startup_error(error).await;
                    if update_attempt && !cancel.is_cancelled() && retry_session_failure(&error) && retries<5 {
                        retries+=1;
                        resolved.refresh_after_upgrade=true;
                        let started = update_started.context(t().session.error_update_state_lost)?;
                        display = replace_room(&viewer_sender, &resolved.summary.alias, None, ConnectionProgress::updating(started), resolved.background.clone(), reporter, cancel).await?;
                        cancellable(cancel,async {tokio::time::sleep(crate::features::remote_upgrade::UPDATE_PROBE_INTERVAL).await;Ok(())}).await?;
                        continue;
                    }
                    return Err(error);
                }
            };
            if audio_only {
                reporter(ConnectionProgress::ready(t().session.connected_audio_only));
                viewer_sender.send(ViewerWindowEvent::Listening(crate::application::viewer::AudioView::new(session_control.clone(), None), resolved.summary.alias.clone()))
                    .map_err(|_| anyhow!("{}", t().session.error_audio_window_closed))?;
            }
            if update_attempt {tracing::info!("media resumed after controlled update");update_started=None;}
            if let Some(since) = recovering.take() {
                tracing::info!(elapsed_ms = since.elapsed().as_millis(), "session recovered");
            }
            retries=0;
            if !cancel.is_cancelled() && let Some(assist) = &mut resolved.assist
                && let Err(error) = assist.remember_success(&resolved.client).await {
                tracing::warn!(%error, "connected assistance code was not saved");
                reporter(ConnectionProgress::ready((t().session.connected_code_not_saved)(&error.to_string())));
            }
            let upgrade = session_control.remote_upgrade();
            let upgrade_deadline = async {
                if let Some(upgrade) = &upgrade {
                    upgrade.wait_for_disconnect_grace().await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::pin!(upgrade_deadline);
            let alive = controller.forwarder.session.end_waiter();
            tokio::pin!(alive);
            let mut next_connection = None;
            let result = loop {
                tokio::select! {
                    result = &mut alive => break result,
                    _ = &mut upgrade_deadline => {
                        break Ok(());
                    }
                    _ = cancel.cancelled() => {
                        break Ok(());
                    }
                    changed = modes.changed() => {
                        if changed.is_err() { continue; }
                        let wanted = *modes.borrow_and_update();
                        if wanted == audio_only { continue; }
                        session_control.mouse().disable();
                        let progress = ConnectionProgress::working(
                            1,
                            if wanted { t().session.progress_switching_to_audio } else { t().session.progress_restoring_picture },
                            t().session.progress_switching_media,
                        );
                        let mode_display = replace_room(&viewer_sender, &resolved.summary.alias, None, progress, None, reporter, cancel).await?;
                        close = None;
                        let switched = if wanted {
                            cancellable(cancel, await_media_startup(controller.forwarder.session.end_waiter(), controller.start_listening())).await
                        } else {
                            let started = cancellable(cancel, session_control.set_video_enabled(true)).await;
                            match started {
                                Err(error) => Err(error),
                                Ok(()) => {
                                    present_video(&mut controller, &resolved.summary.alias, mode_display, reporter, cancel, &viewer_sender, Some(&switcher)).await.map(|ready| { close = Some(ready); })
                                }
                            }
                        };
                        if let Err(error) = switched {
                            if cancel.is_cancelled() { break Err(error); }
                            let _ = session_control.set_video_enabled(false).await;
                            audio_only = true;
                            session_control.request_audio_only(true);
                            viewer_sender.send(ViewerWindowEvent::Listening(crate::application::viewer::AudioView::new(session_control.clone(), Some((t().session.error_media_switch_failed)(&format!("{error:#}")))), resolved.summary.alias.clone())).map_err(|_| anyhow!("{}", t().session.error_media_window_closed))?;
                        } else {
                            audio_only = wanted;
                            if wanted {
                                viewer_sender.send(ViewerWindowEvent::Listening(crate::application::viewer::AudioView::new(session_control.clone(), None), resolved.summary.alias.clone()))
                                    .map_err(|_| anyhow!("{}", t().session.error_audio_window_closed))?;
                            }
                        }
                        resolved.profile.audio_only = audio_only;
                        controller.profile.audio_only = audio_only;
                    }
                    Some(request) = switch_receiver.recv() => {
                        if request.from != resolved.target_device_id || request.device.device_id == resolved.target_device_id { continue; }
                        // Revalidate the real ID while the old room continues playing.
                        let next = cancellable(cancel, resolve_connection_with_client(
                            Arc::clone(&resolved.client), &request.device.alias, options, None,
                            Some(&request.device.device_id))).await;
                        let next = match next {
                            Ok(next) => next,
                            Err(error) => {
                                switcher.failed((t().session.error_switch_failed)(&error.to_string()));
                                continue;
                            }
                        };
                        // The UI releases input and joins every old screen/decoder,
                        // retaining the window in which the user selected the device.
                        let progress = ConnectionProgress::working(
                            1,
                            t().session.progress_switching_device,
                            (t().session.progress_connecting_to)(&next.summary.alias),
                        );
                        let new_display = replace_room(
                            &viewer_sender, &next.summary.alias, Some(request.window), progress,
                            next.background.clone(), reporter, cancel,
                        ).await?;
                        next_connection = Some((next, new_display));
                        break Ok(());
                    }
                }
            };
            let result = result.and(controller.close().await);
            if cancel.is_cancelled() {
                if let Some(close) = close.as_ref() { close.close(); }
                // Closing the HWND can race the observed server leave. Keep
                // that reason for the main window instead of losing it here.
                return match result {
                    Err(error) if room_released(&error) => Err(error),
                    _ => Ok(()),
                };
            }
            if let Some((next, new_display)) = next_connection {
                if let Some(upgrade) = &upgrade { upgrade.retire(); }
                display = new_display;
                // Another device: another record.
                std::mem::replace(&mut record, super::record::VisitRecord::start(monitor.subscribe()))
                    .finish()
                    .await;
                resolved = next;
                retries = 0;
                continue;
            }
            if let Some(started) = upgrade.as_ref().and_then(|upgrade| upgrade.started_at()) {
                update_started = Some(started);
                // A preceding update-start notice makes loss of the OLD room
                // expected. Once it ends, immediately check readiness instead of
                // waiting out a UI countdown. Ordinary room leave remains final.
                tracing::info!("old update session ended; checking target readiness immediately");
                upgrade.as_ref().unwrap().retire();
                resolved.preferences = Some(session_control.preferences());
                resolved.audio_preferences = Some(session_control.audio().settings());
                resolved.refresh_after_upgrade = true;
                retries = 0;
                display = replace_room(&viewer_sender, &resolved.summary.alias, None, ConnectionProgress::updating(started), resolved.background.clone(), reporter, cancel).await?;
                continue;
            }
            if let Some(upgrade) = &upgrade { upgrade.retire(); }
            match result {
                Err(error) if recovery_for(&error, resolved.assist.is_some()) != Recovery::Fatal => {
                    resolved.preferences = Some(session_control.preferences());
                    resolved.audio_preferences = Some(session_control.audio().settings());
                    retries += 1;
                    if recovering.is_none() {
                        record.reconnecting();
                    }
                    recovering.get_or_insert_with(std::time::Instant::now);
                    let progress = ConnectionProgress::working(4, t().session.progress_reconnecting, format!("{error:#}"));
                    display = replace_room(&viewer_sender, &resolved.summary.alias, None, progress, resolved.background.clone(), reporter, cancel).await?;
                }
                result => { if let Some(close) = close.as_ref() { close.close(); } return result; }
            }
        }
    }.await;
    record.finish().await;
    monitor.send_replace(None);
    presence_stop.cancel();
    if let Some(task) = presence_task {
        let _ = task.await;
    }
    if hosted_client.is_none()
        && let Some(client) = account_owner
    {
        client.close().await;
    }
    if cancel.is_cancelled() && !result.as_ref().is_err_and(|error| room_released(error)) {
        Ok(())
    } else {
        result
    }
}

/// Before rebuilding a lost session: back off, or wait until the device is
/// online again. A device push (presence) triggers the check at once; a slow
/// poll covers a process without a presence connection.
async fn wait_before_rebuild(
    resolved: &ResolvedConnection,
    error: &anyhow::Error,
    attempt: u32,
    since: std::time::Instant,
    reporter: &mut ConnectionProgressReporter,
    cancel: &CancellationToken,
) -> Result<()> {
    let deadline = since + RECOVERY_LIMIT;
    let remaining = || deadline.saturating_duration_since(std::time::Instant::now()).as_secs();
    let retry = crate::application::viewer::RetryNow::default();
    let skipped = || async {
        retry.requested().await;
        tracing::info!("reconnecting now at the user's request");
        Ok::<(), anyhow::Error>(())
    };
    if recovery_for(error, resolved.assist.is_some()) == Recovery::WaitDevice {
        tracing::info!(attempt, %error, "waiting for the device to come back online");
        let mut pushes = crate::session::presence::device_pushes();
        loop {
            reporter(ConnectionProgress::working(
                1,
                t().session.progress_waiting_device,
                (t().session.progress_device_offline_wait)(remaining()),
            ).with_retry(&retry));
            let online = cancellable(cancel, async {
                Ok(resolved
                    .client
                    .list_devices()
                    .await
                    .ok()
                    .and_then(|list| {
                        list.devices
                            .into_iter()
                            .chain(list.shared)
                            .find(|device| device.device_id == resolved.target_device_id)
                    })
                    .is_some_and(|device| device.is_connected()))
            })
            .await?;
            if online {
                return Ok(());
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "{}",
                (t().session.error_device_not_back)(RECOVERY_LIMIT.as_secs() / 60)
            );
            let waited = cancellable(cancel, async {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => Ok(false),
                    Ok(()) = pushes.changed() => {
                        tracing::debug!("device push; checking whether the device is back");
                        Ok(false)
                    }
                    result = skipped() => result.map(|()| true),
                }
            })
            .await?;
            if waited {
                return Ok(());
            }
        }
    }
    let delay = recovery_backoff(attempt);
    tracing::info!(attempt, delay_ms = delay.as_millis(), %error, "rebuilding the session after a delay");
    reporter(ConnectionProgress::working(
        4,
        t().session.progress_reconnecting,
        (t().session.progress_retry_in)(&format!("{error:#}"), delay.as_secs(), attempt),
    ).with_retry(&retry));
    cancellable(cancel, async {
        tokio::select! {
            _ = tokio::time::sleep(delay) => Ok(()),
            result = skipped() => result,
        }
    })
    .await
}

/// Ask the player for a new room's display: the window (`window`, a
/// device switch keeps the one it was asked in) shows `progress` and then
/// the new room's picture.
pub(super) async fn replace_room(
    sender: &std::sync::mpsc::Sender<ViewerWindowEvent>,
    alias: &str,
    window: Option<winit::window::WindowId>,
    progress: ConnectionProgress,
    background: Option<crate::application::wallpaper::Source>,
    reporter: &mut ConnectionProgressReporter,
    cancel: &CancellationToken,
) -> Result<ViewerDisplayHandle> {
    let (progress_tx, progress_rx) = std::sync::mpsc::channel();
    let (display_tx, display_rx) = oneshot::channel();
    *reporter = Arc::new(move |progress| {
        let _ = progress_tx.send(progress);
    });
    reporter(progress);
    if let Some(background) = background {
        reporter(ConnectionProgress::background(background));
    }
    sender
        .send(ViewerWindowEvent::Reconnect {
            alias: alias.to_owned(),
            window,
            progress: progress_rx,
            display: display_tx,
        })
        .map_err(|_| anyhow!("{}", t().session.error_media_window_closed))?;
    cancellable(cancel, async {
        display_rx.await.context(t().session.error_media_window_closed)
    })
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn present_video(
    controller: &mut ControllerConnection,
    alias: &str,
    display: ViewerDisplayHandle,
    reporter: &ConnectionProgressReporter,
    cancel: &CancellationToken,
    sender: &std::sync::mpsc::Sender<ViewerWindowEvent>,
    switcher: Option<&crate::application::viewer::device_switch::DeviceSwitcher>,
) -> Result<crate::application::viewer::ViewerCloseHandle> {
    let session = Arc::clone(&controller.forwarder.session);
    let (mut viewer, playback) = cancellable(
        cancel,
        await_media_startup(
            session.ended(),
            controller.start_native_viewer_with_progress(alias, Some(reporter), display),
        ),
    )
    .await?;
    cancellable(
        cancel,
        await_media_startup(session.ended(), async {
            tokio::time::timeout(Duration::from_secs(30), viewer.startup())
                .await
                .context(t().session.error_picture_timeout)?
        }),
    )
    .await?;
    let route = controller
        .peer
        .selected_route_details()
        .await
        .unwrap_or_else(|| t().session.connected.into());
    reporter(ConnectionProgress::ready(format!(
        "{route} · {} · {}",
        playback.codec,
        controller.performance_monitor().snapshot().decoder
    )));
    tracing::info!(device=alias,codec=playback.codec,track=%playback.track_id,"viewer entered playback");
    if let Some(switcher) = switcher {
        viewer.set_device_switch(switcher.clone());
    }
    let close = viewer.close_handle();
    let wake = viewer.window_wake();
    // Measurement aid (with FARWING_DEV_MIC_TONE): the microphone starts with
    // the picture instead of from the window's menu.
    if std::env::var_os("FARWING_DEV_MIC_ON").is_some()
        && let Err(error) = controller.stream_control_handle().set_microphone_enabled(true)
    {
        tracing::warn!(%error, "development microphone start failed");
    }
    sender
        .send(ViewerWindowEvent::Playing(Box::new(viewer)))
        .map_err(|_| anyhow!("{}", t().session.error_video_window_closed))?;
    // The window takes the session at once instead of at its next UI update
    // (up to 33 ms while connecting).
    if let Some(wake) = wake {
        wake();
    }
    Ok(close)
}

pub async fn run_native_viewer_session(
    connection: ControllerConnection,
    viewer: NativeViewerSession,
) -> Result<()> {
    let close_handle = viewer.close_handle();
    let close_on_session_end = close_handle.clone();
    let close_on_interrupt = close_handle;
    let (shutdown, shutdown_rx) = oneshot::channel();
    let keep_alive = tokio::spawn(async move {
        let result = connection.keep_alive(shutdown_rx).await;
        close_on_session_end.close();
        result
    });
    let interrupt = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            close_on_interrupt.close();
        }
    });
    let viewer_result = viewer.run();
    interrupt.abort();
    let _ = shutdown.send(());
    keep_alive
        .await
        .context("controller shutdown task failed")??;
    viewer_result
}
