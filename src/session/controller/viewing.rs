//! Viewing-window attachment, reconnect ownership and cancellation.
use super::connection::{ResolvedConnection, resolve_connection_with_client};
use super::{
    ConnectionProgressReporter, ControllerConnection, assist, await_media_startup, cancellable,
    retry_session_failure, room_released, takeover, windows,
};
use crate::account::client::AuthenticatedClient;
use crate::application::viewer::{
    ConnectionProgress, NativeViewerSession, ViewerDisplayHandle, ViewerWindowEvent,
    run_connecting_viewer_window,
};
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
    force_takeover: bool,
) -> Result<()> {
    run_viewer_window(alias, options, target_id, None, None, force_takeover).await
}

pub(super) async fn run_viewer_window(
    alias: String,
    options: ConnectionMediaOptions,
    target_id: Option<String>,
    assist: Option<crate::account::assist::AssistRequest>,
    mut hosted: Option<windows::WindowContext>,
    force_takeover: bool,
) -> Result<()> {
    let owns_presence = hosted.is_none();
    let background = hosted.as_ref().and_then(|h| h.background.clone());
    let takeover = hosted.as_mut().and_then(|h| h.takeover.take());
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
    // Only a standalone `connect` process ends with its viewer. Once tokio
    // listens for SIGTERM the default action is gone for the whole process,
    // so a viewer hosted in the GUI must not, or the GUI would outlive it.
    let terminate = owns_presence;
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
            // SIGTERM (例如 timeout、服务管理结束进程) 也走干净关闭：
            // 数据通道正常关闭后主机立即释放会话，避免 ~40s 的
            // keepalive 超时锁导致重连被"设备正在被其他设备使用"拒绝。
            _ = async {
                if !terminate {
                    std::future::pending::<()>().await;
                }
                #[cfg(unix)]
                {
                    match tokio::signal::unix::signal(
                        tokio::signal::unix::SignalKind::terminate(),
                    ) {
                        Ok(mut terminate) => {
                            terminate.recv().await;
                        }
                        Err(error) => {
                            tracing::warn!(%error, "SIGTERM listener unavailable");
                            std::future::pending::<()>().await;
                        }
                    }
                }
                #[cfg(not(unix))]
                {
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
                client: hosted.map(|h| h.client),
                takeover,
                force_takeover,
            },
            &task_cancel,
            &mut reporter,
        )
        .await;
        if !task_cancel.is_cancelled()
            && let Err(error) = &result
        {
            if room_released(error) || error.downcast_ref::<takeover::Required>().is_some() {
                // Return terminal leave / explicit takeover confirmation to
                // the owning UI without presenting a decoder/connection error.
                let _ = terminal_sender.send(ViewerWindowEvent::Close);
            } else {
                reporter(ConnectionProgress::failed(format!("{error:#}")));
            }
        }
        result
    });

    let window_result = if let Some(key) = window_key {
        crate::ui::window_manager::viewer(
            key,
            crate::application::viewer::presenter::ConnectingWindowsRunConfig {
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
    pub(super) takeover: Option<takeover::Approval>,
    pub(super) force_takeover: bool,
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
        client: hosted_client,
        takeover,
        force_takeover,
    } = window;
    let presence_stop = CancellationToken::new();
    let mut presence_task = None;
    let mut account_owner = None;
    let result = async {
        let client = if let Some(client)=hosted_client.clone(){client}else{Arc::new(AuthenticatedClient::from_saved_session()?)};
        account_owner = Some(Arc::clone(&client));
        let mut resolved = if let Some(request) = assist {
            cancellable(cancel, assist::resolve(client, &alias, options, request, Some(reporter))).await?
        } else {
            cancellable(cancel, resolve_connection_with_client(client, &alias, options, Some(reporter), target_id.as_deref(), takeover, force_takeover)).await?
        };
        // Standalone processes keep the host presence room (设备在线状态；
        // 被控权限开关已从界面移除，不开放被控). The account-ended token
        // still closes the viewer so a revoked session cannot keep watching.
        if owns_presence {
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
        loop {
            monitor.send_replace(None);
            target.send_replace(Some(windows::ViewerTarget {
                device_id: resolved.target_device_id.clone(), alias: resolved.summary.alias.clone(),
            }));
            let update_attempt = resolved.refresh_after_upgrade;
            let mut controller = match resolved.connect(Some(reporter), cancel, &mut retries).await {
                Ok(controller) => controller,
                Err(error) if !cancel.is_cancelled() && retry_session_failure(&error) && retries < 5 => {
                    retries += 1;
                    resolved.refresh_after_upgrade |= update_attempt;
                    reporter(ConnectionProgress::working(4, "重建观看会话", format!("{error:#}；正在重新加入房间（{retries}/5）")));
                    if update_attempt {
                        cancellable(cancel,async {tokio::time::sleep(crate::features::remote_upgrade::UPDATE_PROBE_INTERVAL).await;Ok(())}).await?;
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            // F94230 resets the full-session retry budget on peer connected.
            monitor.send_replace(Some(controller.performance_monitor()));
            // During an update, a connected peer is not enough: retain the
            // retry budget until media startup succeeds, otherwise repeated
            // disconnects before the first frame could reset it indefinitely.
            if !update_attempt { retries = 0; }
            let switcher = crate::application::viewer::device_switch::DeviceSwitcher::new(
                Arc::clone(&resolved.client), resolved.target_device_id.clone(), switch_sender.clone(), cancel.clone());
            let session_control = controller.stream_control_handle();
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
                present_video(&mut controller, &resolved.summary.alias, display, reporter, cancel, &viewer_sender, &switcher).await.map(Some)
            };
            let mut close=match startup {
                Ok(close)=>close,
                Err(error)=>{
                    let error=controller.close_after_startup_error(error).await;
                    if update_attempt && !cancel.is_cancelled() && retry_session_failure(&error) && retries<5 {
                        retries+=1;
                        resolved.refresh_after_upgrade=true;
                        display=update_reconnect_display(&resolved,reporter,cancel,&viewer_sender,update_started.context("更新等待状态已丢失")?).await?;
                        cancellable(cancel,async {tokio::time::sleep(crate::features::remote_upgrade::UPDATE_PROBE_INTERVAL).await;Ok(())}).await?;
                        continue;
                    }
                    return Err(error);
                }
            };
            // The viewing attachment owns this lease, independently of windows,
            // decoder replacement and shared file/port connections. Also keep it
            // across video/audio-only mode changes: display audio can disappear
            // when Windows idles the monitor. Each device owns its own request.
            let power_request = match crate::platform::power_request::Request::viewing() {
                Ok(request) => Some(request),
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"),
                        "viewer automatic display-off/sleep prevention unavailable");
                    None
                }
            };
            if audio_only {
                reporter(ConnectionProgress::ready("仅音频连接已建立"));
                viewer_sender.send(ViewerWindowEvent::Listening(crate::application::viewer::AudioView::new(session_control.clone(), None)))
                    .map_err(|_| anyhow!("音频窗口已关闭"))?;
            }
            if update_attempt {tracing::info!("media resumed after controlled update");update_started=None;}
            retries=0;
            if !cancel.is_cancelled() && let Some(assist) = &mut resolved.assist
                && let Err(error) = assist.remember_success(&resolved.client).await {
                tracing::warn!(%error, "connected assistance code was not saved");
                reporter(ConnectionProgress::ready(format!("已连接 · 验证码未保存：{error}")));
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
                        let (progress_tx, progress_rx) = std::sync::mpsc::channel();
                        let (display_tx, display_rx) = oneshot::channel();
                        *reporter = Arc::new(move |progress| { let _ = progress_tx.send(progress); });
                        reporter(ConnectionProgress::working(1, if wanted { "切换为仅音频" } else { "恢复画面" }, "正在切换媒体模式"));
                        viewer_sender.send(ViewerWindowEvent::Reconnect {
                            alias:resolved.summary.alias.clone(), window:None, progress:progress_rx, display:display_tx,
                        }).map_err(|_| anyhow!("媒体窗口已关闭"))?;
                        let mode_display = cancellable(cancel, async { display_rx.await.context("媒体窗口未完成切换") }).await?;
                        close = None;
                        let switched = if wanted {
                            cancellable(cancel, await_media_startup(controller.forwarder.session.end_waiter(), controller.start_listening())).await
                        } else {
                            let started = cancellable(cancel, session_control.set_video_enabled(true)).await;
                            match started {
                                Err(error) => Err(error),
                                Ok(()) => {
                                    present_video(&mut controller, &resolved.summary.alias, mode_display, reporter, cancel, &viewer_sender, &switcher).await.map(|ready| { close = Some(ready); })
                                }
                            }
                        };
                        if let Err(error) = switched {
                            if cancel.is_cancelled() { break Err(error); }
                            let _ = session_control.set_video_enabled(false).await;
                            audio_only = true;
                            session_control.request_audio_only(true);
                            viewer_sender.send(ViewerWindowEvent::Listening(crate::application::viewer::AudioView::new(session_control.clone(), Some(format!("媒体切换失败：{error:#}"))))).map_err(|_| anyhow!("媒体窗口已关闭"))?;
                        } else {
                            audio_only = wanted;
                            if wanted {
                                viewer_sender.send(ViewerWindowEvent::Listening(crate::application::viewer::AudioView::new(session_control.clone(), None)))
                                    .map_err(|_| anyhow!("音频窗口已关闭"))?;
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
                            Some(&request.device.device_id), request.takeover, false)).await;
                        let next = match next {
                            Ok(next) => next,
                            Err(error) => {
                                if let Some(required) = error.downcast_ref::<takeover::Required>() {
                                    switcher.require_takeover(required.0.clone(), request.window);
                                } else {
                                    switcher.failed(format!("无法切换：{error}"));
                                }
                                continue;
                            }
                        };
                        let (progress_tx, progress_rx) = std::sync::mpsc::channel();
                        let (display_tx, display_rx) = oneshot::channel();
                        *reporter = Arc::new(move |progress| { let _ = progress_tx.send(progress); });
                        reporter(ConnectionProgress::working(1, "切换设备", format!("正在连接 {}", next.summary.alias)));
                        if let Some(background)=next.background.clone(){reporter(ConnectionProgress::background(background));}
                        // The UI releases input and joins every old screen/decoder,
                        // retaining the window in which the user selected the device.
                        let replacement = async {
                            viewer_sender.send(ViewerWindowEvent::Reconnect {
                                alias: next.summary.alias.clone(), window: Some(request.window),
                                progress: progress_rx, display: display_tx,
                            }).map_err(|_| anyhow!("player window closed during device switch"))?;
                            display_rx.await.context("player did not acknowledge device switch")
                        };
                        let new_display = cancellable(cancel, replacement).await;
                        let new_display = new_display?;
                        next_connection = Some((next, new_display));
                        break Ok(());
                    }
                }
            };
            drop(power_request);
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
                resolved.takeover = None;
                retries = 0;
                display = update_reconnect_display(&resolved,reporter,cancel,&viewer_sender,started).await?;
                continue;
            }
            if let Some(upgrade) = &upgrade { upgrade.retire(); }
            match result {
                Err(error) if retry_session_failure(&error) => {
                    resolved.preferences = Some(session_control.preferences());
                    resolved.audio_preferences = Some(session_control.audio().settings());
                    retries += 1;
                    let (progress_tx, progress_rx) = std::sync::mpsc::channel();
                    let (display_tx, display_rx) = oneshot::channel();
                    *reporter = Arc::new(move |progress| { let _ = progress_tx.send(progress); });
                    reporter(ConnectionProgress::working(4, "重建观看会话", format!("{error:#}；正在重新加入房间（{retries}/5）")));
                    if let Some(background)=resolved.background.clone(){reporter(ConnectionProgress::background(background));}
                    viewer_sender.send(ViewerWindowEvent::Reconnect { alias: resolved.summary.alias.clone(), window: None, progress: progress_rx, display: display_tx })
                        .map_err(|_| anyhow!("player window closed during reconnect"))?;
                    display = cancellable(cancel, async { display_rx.await.context("player did not acknowledge room replacement") }).await?;
                }
                result => { if let Some(close) = close.as_ref() { close.close(); } return result; }
            }
        }
    }.await;
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

async fn update_reconnect_display(
    resolved: &ResolvedConnection,
    reporter: &mut ConnectionProgressReporter,
    cancel: &CancellationToken,
    sender: &std::sync::mpsc::Sender<ViewerWindowEvent>,
    update_started: std::time::Instant,
) -> Result<ViewerDisplayHandle> {
    let (progress_tx, progress_rx) = std::sync::mpsc::channel();
    let (display_tx, display_rx) = oneshot::channel();
    *reporter = Arc::new(move |progress| {
        let _ = progress_tx.send(progress);
    });
    reporter(ConnectionProgress::updating(update_started));
    if let Some(background) = resolved.background.clone() {
        reporter(ConnectionProgress::background(background));
    }
    sender
        .send(ViewerWindowEvent::Reconnect {
            alias: resolved.summary.alias.clone(),
            window: None,
            progress: progress_rx,
            display: display_tx,
        })
        .map_err(|_| anyhow!("更新等待窗口已关闭"))?;
    cancellable(cancel, async {
        display_rx.await.context("更新等待窗口已关闭")
    })
    .await
}

#[allow(clippy::too_many_arguments)]
async fn present_video(
    controller: &mut ControllerConnection,
    alias: &str,
    display: ViewerDisplayHandle,
    reporter: &ConnectionProgressReporter,
    cancel: &CancellationToken,
    sender: &std::sync::mpsc::Sender<ViewerWindowEvent>,
    switcher: &crate::application::viewer::device_switch::DeviceSwitcher,
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
                .context("等待视频首帧超时")?
        }),
    )
    .await?;
    let route = controller
        .peer
        .selected_route_details()
        .await
        .unwrap_or_else(|| "安全媒体通道已建立".into());
    reporter(ConnectionProgress::ready(format!(
        "{route} · {} · {}",
        playback.codec,
        controller.performance_monitor().snapshot().decoder
    )));
    tracing::info!(device=alias,codec=playback.codec,track=%playback.track_id,"viewer entered playback");
    viewer.set_device_switch(switcher.clone());
    let close = viewer.close_handle();
    sender
        .send(ViewerWindowEvent::Playing(Box::new(viewer)))
        .map_err(|_| anyhow!("画面窗口已关闭"))?;
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
