//! Publisher business-channel routing and observed source reporting.
use super::super::lock;
use super::screens::{Inventory, ModeList};
use super::{ReportRoutes, ReportTarget};
use anyhow::Result;
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::RTCDataChannel;

pub(super) async fn bind_channel(
    channel: Arc<RTCDataChannel>,
    screens: Arc<tokio::sync::Mutex<super::screens::Screens>>,
    cancel: CancellationToken,
    kcp: crate::transport::uu_kcp::UuKcpControl,
    report_target: ReportTarget,
    input: crate::features::host::input::Receiver,
    microphone: crate::features::host::microphone::Receiver,
    audio: crate::features::host::audio::Audio,
    clipboard: crate::features::host::clipboard::Receiver,
    files: crate::features::host::files::Receiver,
    ports: crate::features::port_mapping::host::Receiver,
    annotation: crate::features::host::annotation::Receiver,
    control_ingress: crate::transport::uu_kcp::ControlReceiver,
) {
    if cancel.is_cancelled() {
        return;
    }
    let control = channel.label() == "CONTROL_DATA_CHANNEL";
    let text = channel.label() == "TEXT_DATA_CHANNEL";
    let file = channel.label() == "FILE_DATA_CHANNEL";
    kcp.bind_stream(channel.id(), control);
    if !control && !text && !file {
        return;
    }
    if text {
        annotation.bind(&channel);
    }
    if text || file {
        files.bind(&channel);
    }
    if file {
        ports.bind(&channel).await;
        clipboard.bind_file(&channel);
    }
    let clipboard_generation = if text {
        Some(clipboard.bind(channel.id()))
    } else {
        None
    };
    let input_generation = if control {
        report_target.send_modify(|routes| {
            routes.control = None;
            routes.handshake_complete = false;
            routes.handshake_answered = false;
            routes.revision = routes.revision.wrapping_add(1);
        });
        Some(input.bind(channel.id()))
    } else {
        None
    };
    tracing::info!(
        channel = channel.label(),
        stream_id = channel.id(),
        "host business channel bound"
    );
    let weak = Arc::downgrade(&channel);
    let opening = cancel.clone();
    let opening_kcp = kcp.clone();
    let close_kcp = kcp.clone();
    let close_channel = Arc::downgrade(&channel);
    let closing_target = report_target.clone();
    let closing_input = input.clone();
    let closing_microphone = microphone.clone();
    let closing_files = files.clone();
    let closing_clipboard = clipboard.clone();
    let closing_ports = ports.clone();
    let closing_annotation = annotation.clone();
    channel.on_close(Box::new(move || {
        if let Some(channel) = close_channel.upgrade() {
            closing_files.close(&channel);
            if text {
                closing_annotation.close(&channel);
            }
            if file {
                closing_ports.close(&channel);
                closing_clipboard.close_file(&channel);
            }
        }
        if control {
            if let Some(channel) = close_channel.upgrade() {
                if closing_input.close(channel.id(), input_generation.unwrap()) {
                    close_kcp.set_control_stream(channel.id(), false);
                }
            }
        }
        closing_target.send_if_modified(|routes| {
            let current = if control {
                &mut routes.control
            } else {
                &mut routes.text
            };
            if current
                .as_ref()
                .is_some_and(|target| target.ptr_eq(&close_channel))
            {
                if !control {
                    closing_microphone.transport_lost();
                }
                *current = None;
                if control {
                    routes.handshake_complete = false;
                    routes.handshake_answered = false;
                }
                routes.revision = routes.revision.wrapping_add(1);
                true
            } else {
                false
            }
        });
        Box::pin(async {})
    }));
    let opening_target = report_target.clone();
    let opening_annotation = annotation.clone();
    let opening_input = input.clone();
    channel.on_open(Box::new(move || {
        Box::pin(async move {
            let Some(channel) = weak.upgrade() else {
                return;
            };
            if opening.is_cancelled() {
                return;
            }
            if control && opening_input.generation(channel.id()) != input_generation {
                return;
            }
            if file {
                return;
            }
            opening_target.send_modify(|routes| {
                if control {
                    routes.control = Some(Arc::downgrade(&channel));
                } else {
                    routes.text = Some(Arc::downgrade(&channel));
                }
                routes.revision = routes.revision.wrapping_add(1);
            });
            // KCP can beat DCEP/on_open. Release its queued first message only
            // after this channel's handshake state and response route exist.
            if control {
                opening_kcp.set_control_stream(channel.id(), true);
            }
            if text {
                if let Some(hello) = opening_annotation.hello(&channel) {
                    if let Err(error) = channel.send_text_bytes(&Bytes::from(hello)).await {
                        tracing::debug!(%error,"annotation capability send failed");
                    }
                }
                return;
            }
            if file {
                return;
            }
            // PB readiness belongs to this channel, not the input executor.
            // Input continues to enforce its own binding/desktop readiness.
            tracing::info!(channel = channel.label(), "host business channel opened");
            let bytes = crate::features::stream_control::publisher::echo(1, 0, true);
            let result = send_business(&channel, control, &opening_kcp, bytes).await;
            if let Err(error) = result {
                tracing::warn!(%error,control,"host opening business message failed");
            }
        })
    }));
    let weak = Arc::downgrade(&channel);
    channel.on_message(Box::new(move |message| {
        let weak = weak.clone();
        let control_ingress = control_ingress.clone();
        let screens = screens.clone();
        let cancel = cancel.clone();
        let report_target = report_target.clone();
        let input = input.clone();
        let microphone = microphone.clone();
        let audio = audio.clone();
        let files = files.clone();
        let ports = ports.clone();
        let annotation = annotation.clone();
        let clipboard = clipboard.clone();
        Box::pin(async move {
            if cancel.is_cancelled() {
                return;
            }
            let Some(channel) = weak.upgrade() else {
                return;
            };
            if control {
                if input.generation(channel.id()) == input_generation {
                    if let Err(error) = control_ingress(channel.id(), &message.data) {
                        tracing::warn!(%error, "host CONTROL message rejected");
                    }
                }
                return;
            }
            if (text || file) && crate::protocol::drag_drop::is_packet(&message.data) {
                let result = if file {
                    clipboard.receive_file(&channel, &message.data)
                } else {
                    clipboard
                        .receive(clipboard_generation.unwrap(), &message.data)
                        .map(|_| ())
                };
                if let Err(error) = result {
                    tracing::warn!(%error,"native drag packet rejected");
                }
                return;
            }
            if text && crate::protocol::annotation::is_packet(&message.data) {
                if let Some(channel) = weak.upgrade() {
                    if let Err(error) = annotation.receive(&channel, &message.data).await {
                        tracing::warn!(%error,"native annotation packet rejected");
                    }
                }
                return;
            }
            if text || file {
                if let Some(channel) = weak.upgrade() {
                    if file {
                        match ports.receive(&channel, &message.data).await {
                            Ok(true) => return,
                            Ok(false) => {}
                            Err(error) => {
                                tracing::warn!(%error, "host port message rejected");
                                return;
                            }
                        }
                    }
                    match files.receive(&channel, &message.data).await {
                        Ok(true) => return,
                        Ok(false) => {}
                        Err(error) => {
                            tracing::warn!(%error, "host file message rejected");
                            return;
                        }
                    }
                }
                if file {
                    return;
                }
            }

            if cancel.is_cancelled() {
                return;
            }
            let Some(channel) = weak.upgrade() else {
                return;
            };
            if text
                && !report_target
                    .borrow()
                    .text
                    .as_ref()
                    .is_some_and(|current| current.ptr_eq(&weak))
            {
                return;
            }
            if text {
                match annotation.receive(&channel, &message.data).await {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(%error, "host annotation request rejected");
                        return;
                    }
                }
                match clipboard.receive(clipboard_generation.unwrap(), &message.data) {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(%error, "rejected host clipboard message");
                        return;
                    }
                }
                match audio.quality_request(&message.data) {
                    Ok(Some(response)) => {
                        let _ = channel.send_text_bytes(&Bytes::from(response)).await;
                        return;
                    }
                    Err(error) => {
                        tracing::warn!(%error,"invalid audio quality request");
                        return;
                    }
                    Ok(None) => {}
                }
                match input.receive_action(&message.data) {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(%error,"rejected malformed host input action");
                        return;
                    }
                }
                match crate::features::stream_control::publisher::microphone_policy(&message.data) {
                    Ok(Some(policy)) => {
                        let result = microphone.policy(policy.enabled).await;
                        if let Err(error) = &result {
                            tracing::warn!(%error,"host microphone request failed");
                        }
                        if let Some(channel) = weak.upgrade() {
                            if !cancel.is_cancelled() {
                                let response = policy.response(
                                    result
                                        .as_ref()
                                        .err()
                                        .map_or(0, crate::features::host::microphone::error_code),
                                );
                                if let Err(error) =
                                    channel.send_text_bytes(&Bytes::from(response)).await
                                {
                                    microphone.transport_lost();
                                    tracing::debug!(%error,"host microphone response failed");
                                }
                            }
                        }
                        return;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(%error,"invalid microphone envelope");
                        return;
                    }
                }
            }
            tracing::debug!(
                control,
                bytes = message.data.len(),
                tag = message.data.first().copied(),
                "host business message received"
            );
            let responses = crate::features::stream_control::publisher::receive_session(
                &mut *screens.lock().await,
                &message.data,
                false,
            )
            .await;
            match responses {
                Ok(responses) => {
                    if let Some(caps) = responses.files {
                        files.capabilities(caps);
                    }
                    if let Some(active) = responses.media_active {
                        audio.set_media_allowed(active);
                        annotation.viewing(active);
                    }
                    report_target.send_if_modified(|routes| routes.received(&responses));
                    if let Some(channel) = weak.upgrade() {
                        for response in responses.messages {
                            if cancel.is_cancelled() {
                                break;
                            }
                            let result = channel.send_text_bytes(&Bytes::from(response)).await;
                            if let Err(error) = result {
                                tracing::warn!(%error,control,"host business response failed");
                            }
                        }
                    }
                }
                Err(error) => tracing::warn!(%error,"rejected malformed host business message"),
            }
        })
    }));
}

/// S 46D0F0 -> 471950 -> ControlledConnectionContext[10] -> wrapper[13]:
/// CaptureSetting RPC replies use TEXT even when iOS submits them on CONTROL.
pub(super) async fn send_setting_response(target: &ReportTarget, bytes: Vec<u8>) -> Result<usize> {
    let channel = target
        .borrow()
        .text
        .as_ref()
        .and_then(std::sync::Weak::upgrade)
        .ok_or_else(|| anyhow::anyhow!("画面设置回执通道尚未就绪"))?;
    // Do not cancel a reliable SCTP message midway through fragment enqueue.
    Ok(channel.send_text_bytes(&Bytes::from(bytes)).await?)
}

pub(super) async fn send_business(
    channel: &RTCDataChannel,
    control: bool,
    kcp: &crate::transport::uu_kcp::UuKcpControl,
    bytes: Vec<u8>,
) -> Result<usize> {
    if control {
        return kcp.send_control(channel, bytes).await;
    }
    Ok(channel.send_text_bytes(&Bytes::from(bytes)).await?)
}

pub(super) async fn publish_state(
    screens: Arc<tokio::sync::Mutex<super::screens::Screens>>,
    reports: Arc<super::screens::Reports>,
    transports: Vec<crate::features::host::transport::Transport>,
    mut target: tokio::sync::watch::Receiver<ReportRoutes>,
    cancel: CancellationToken,
    kcp: crate::transport::uu_kcp::UuKcpControl,
    initial: Option<crate::features::host::capture::Screen>,
    mut mouse_policy: tokio::sync::watch::Receiver<
        crate::features::host::input::config::MousePolicy,
    >,
    audio: super::super::audio::Audio,
    mut microphone: tokio::sync::watch::Receiver<super::super::microphone::Status>,
    display_geometry: bool,
) {
    use crate::features::stream_control::publisher;
    use std::sync::atomic::Ordering;
    let mut changed = reports.changed.subscribe();
    let mut last_screen = None;
    let mut last_capture = None;
    let mut last_visible = None;
    let mut last_quality = None;
    let mut last_probe = None::<u32>;
    let mut last_locked = None;
    let mut last_mouse_policy = None;
    let mut last_microphone = None;
    let mut last_virtual_speaker = None;
    let mut last_audio_quality = None;
    let mut revision = None;
    let mut secure_revision = None;
    let mut inventory_error = None::<String>;
    let mut inventory_reads = tokio::task::JoinSet::new();
    let sequence = &reports.sequence;
    let mut timer = tokio::time::interval(Duration::from_secs(1));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let entitlement_delay = screens
            .try_lock()
            .ok()
            .map_or(Duration::from_secs(1), |s| s.entitlement_delay());
        let outcome = tokio::select! {
            _=cancel.cancelled()=>break,
            result=changed.changed()=>{if result.is_err(){break;} None},
            result=target.changed()=>{if result.is_err(){break;} None},
            result=mouse_policy.changed()=>{if result.is_err(){break;} None},
            result=microphone.changed()=>{if result.is_err(){break;} None},
            _=timer.tick()=>{
                // Enumeration must not delay publication of a new capture source.
                if inventory_reads.is_empty() && let Ok(state) = screens.try_lock() {
                    let revision = state.inventory_revision();
                    inventory_reads.spawn_blocking(move || (revision, Inventory::read(ModeList::Enumerate)));
                }
                None
            },
            _=tokio::time::sleep(entitlement_delay)=>None,
            Some(result)=inventory_reads.join_next(), if !inventory_reads.is_empty()=>match result {
                Ok((revision, Ok(inventory))) => {
                    if let Ok(mut state) = screens.try_lock() {
                        Some(state.maintain(revision, inventory).await)
                    } else {
                        None
                    }
                }
                Ok((_, Err(error))) => Some(Err(error)),
                Err(error) => Some(Err(error.into())),
            },
        };
        if let Ok(mut state) = screens.try_lock() {
            state.refresh_entitlement_limits();
        }
        match outcome {
            Some(Ok(())) if inventory_error.take().is_some() => {
                tracing::info!("host screen inventory refresh recovered");
            }
            Some(Err(error)) => {
                let message = format!("{error:#}");
                if inventory_error.as_ref() != Some(&message) {
                    tracing::warn!(error = %message, "host screen inventory refresh failed");
                    inventory_error = Some(message);
                }
            }
            _ => {}
        }
        let routes = target.borrow_and_update().clone();
        // S547AF0: initial state is published after the peer answers our PB
        // handshake, not merely when DCEP opens. Otherwise the controller can
        // consume a catalog before its connection-success initialization.
        if !routes.handshake_complete || !routes.handshake_answered {
            reports.media_ready.store(false, Ordering::Release);
            continue;
        }
        if revision != Some(routes.revision) {
            revision = Some(routes.revision);
            last_screen = None;
            last_capture = None;
            last_visible = None;
            last_quality = None;
            last_probe = None;
            last_locked = None;
            last_mouse_policy = None;
            last_microphone = None;
            last_virtual_speaker = None;
            last_audio_quality = None;
        }
        if secure_revision != Some(routes.secure_revision) {
            secure_revision = Some(routes.secure_revision);
            last_locked = None;
        }
        if let Some(hello) = audio.quality_hello()
            && last_audio_quality.as_ref() != Some(&hello)
            && let Some(channel) = routes.text.as_ref().and_then(std::sync::Weak::upgrade)
        {
            let payload = Bytes::copy_from_slice(&hello);
            let sent = tokio::select! {
                _ = cancel.cancelled() => break,
                result = channel.send_text_bytes(&payload) => result,
            };
            if sent.is_ok() {
                last_audio_quality = Some(hello);
            }
        }
        let send = |control: bool, bytes: Vec<u8>| {
            let channel = if control {
                &routes.control
            } else {
                &routes.text
            }
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
            let cancel = &cancel;
            let kcp = &kcp;
            let sequence = &sequence;
            async move {
                let Some(channel) = channel else {
                    return false;
                };
                let bytes = match publisher::stamp_report(
                    &bytes,
                    sequence.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                ) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        tracing::warn!(%error, "invalid host report");
                        return false;
                    }
                };
                let result = tokio::select! {
                    _=cancel.cancelled()=>return false,
                    result=send_business(&channel, control, kcp, bytes)=>result,
                };
                if let Err(error) = result {
                    tracing::debug!(%error, control, "host publication channel unavailable");
                    false
                } else {
                    true
                }
            }
        };
        let mic = microphone.borrow_and_update().clone();
        if mic.enabled && mic.error.is_none() {
            if last_virtual_speaker != Some(mic.speaker_active)
                && send(
                    false,
                    publisher::audio_device_event(if mic.speaker_active { 25 } else { 26 }),
                )
                .await
            {
                last_virtual_speaker = Some(mic.speaker_active);
            }
        } else {
            last_virtual_speaker = None;
        }
        let action = microphone_event(last_microphone.as_ref(), &mic);
        if action.is_none() || send(false, publisher::audio_device_event(action.unwrap())).await {
            last_microphone = Some(mic);
        }
        // Native layout changes and capture restart form one screen transition.
        // Do not publish a half-restored catalog while that owner is awaiting I/O.
        let (media, catalog, requested) = {
            let _state = tokio::select! {
                _ = cancel.cancelled() => break,
                state = screens.lock() => state,
            };
            (
                reports.media(),
                lock(&reports.catalog).clone(),
                reports.current.load(Ordering::Acquire),
            )
        };
        // A capture can restart from topology alone. Publish supported modes
        // only once enumerated, so viewers never infer a bogus conversion.
        if catalog.iter().any(|screen| !screen.modes_enumerated) {
            continue;
        }
        let current = if catalog.iter().any(|s| s.screen.id == requested) {
            requested
        } else {
            catalog.first().map_or(-1, |s| s.screen.id)
        };
        let mapping = media
            .iter()
            .filter(|(_, state)| state.capturing)
            .map(|(index, state)| (state.screen.id, *index))
            .collect();
        let policy = mouse_policy.borrow_and_update().clone();
        if last_mouse_policy.as_ref() != Some(&policy) {
            if let Some(message) = policy.message() {
                if send(true, message).await {
                    last_mouse_policy = Some(policy);
                }
            } else {
                last_mouse_policy = Some(policy);
            }
        }
        let screen = publisher::screen_states(&catalog, &mapping, current, display_geometry);
        if last_screen.as_ref() != Some(&screen)
            && send(routes.control_screens, screen.clone()).await
        {
            tracing::debug!(current, ?mapping, "host screen catalog published");
            last_screen = Some(screen);
        }
        let selected = media
            .iter()
            .find(|(_, s)| s.capturing && s.screen.id == current)
            .or_else(|| media.iter().find(|(_, s)| s.capturing));
        let capture = selected.map(|(_, s)| s.screen.id);
        if last_capture != Some(capture)
            && let Some(source) = selected.map(|(_, s)| &s.screen).or(initial.as_ref())
            && send(false, publisher::capture_change(source, selected.is_some())).await
        {
            last_capture = Some(capture);
        }
        // The controller's own ECHO response must precede its source catalog
        // and first video frame. Answering our ECHO alone does not complete
        // its reconnect/UI initialization.
        if last_screen.is_some() && last_capture.is_some() {
            reports.media_ready.store(true, Ordering::Release);
        }
        let visible = media.iter().any(|(_, s)| s.capturing && s.visible);
        let permission = (visible, audio.allowed());
        if last_visible != Some(permission)
            && send(false, publisher::permissions(visible, permission.1)).await
        {
            last_visible = Some(permission);
        }
        if routes.control.is_some()
            && let Some(locked) = crate::features::host::capture::session_locked()
            && last_locked != Some(locked)
            && send(true, publisher::secure_desktop(locked)).await
        {
            last_locked = Some(locked);
        }
        if let Some((index, state)) = selected
            && let Some(encoder) = state.encoder
            && state.quality > 0
        {
            let probe = transports[*index].automatic_rates().probe;
            let key = (
                state.screen.id,
                state.quality,
                state.fps,
                state.format,
                state.budget_size,
                state.encoder,
                state.capture.clone(),
                state.screen.width,
                state.screen.height,
            );
            if (last_quality.as_ref() != Some(&key)
                || last_probe.is_none_or(|old| old.abs_diff(probe) >= 102_400))
                && send(
                    false,
                    publisher::quality_report(
                        state.format,
                        state.quality,
                        probe,
                        state.budget_size,
                        state.fps,
                        encoder,
                        &state.capture,
                    ),
                )
                .await
            {
                last_quality = Some(key);
                last_probe = Some(probe);
            }
        }
    }
}

fn microphone_event(
    previous: Option<&super::super::microphone::Status>,
    next: &super::super::microphone::Status,
) -> Option<i32> {
    if previous.is_some_and(|previous| {
        previous.policy_generation == next.policy_generation
            && previous.enabled == next.enabled
            && previous.active == next.active
            && previous.error == next.error
    }) {
        return None;
    }
    if next.enabled && next.error.is_some() {
        Some(27)
    } else if next.enabled {
        Some(if next.active { 23 } else { 24 })
    } else if previous.is_some_and(|previous| previous.enabled) {
        Some(24)
    } else {
        None
    }
}
