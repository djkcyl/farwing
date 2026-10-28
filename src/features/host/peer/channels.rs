//! Publisher business-channel routing and observed source reporting.
use super::super::lock;
use super::{ReportRoutes, ReportTarget};
use anyhow::Result;
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::RTCDataChannel;

/// Port forwarding channels carry only `PORT`, `PORT_DGRAM` and `FLOW`.
async fn bind_tunnel(
    channel: Arc<RTCDataChannel>,
    ports: crate::features::port_mapping::host::Receiver,
    cancel: CancellationToken,
) {
    ports.bind(&channel).await;
    tracing::info!(
        channel = channel.label(),
        stream_id = channel.id(),
        "host port forwarding channel bound"
    );
    let closing = ports.clone();
    let close_channel = Arc::downgrade(&channel);
    channel.on_close(Box::new(move || {
        if let Some(channel) = close_channel.upgrade() {
            closing.close(&channel);
        }
        Box::pin(async {})
    }));
    let weak = Arc::downgrade(&channel);
    channel.on_message(Box::new(move |message| {
        let weak = weak.clone();
        let ports = ports.clone();
        let cancel = cancel.clone();
        Box::pin(async move {
            let Some(channel) = weak.upgrade().filter(|_| !cancel.is_cancelled()) else {
                return;
            };
            match ports.receive(&channel, &message.data).await {
                Ok(true) => {}
                Ok(false) => tracing::debug!(
                    bytes = message.data.len(),
                    "ignored frame on a port forwarding channel"
                ),
                Err(error) => tracing::warn!(%error, "host port message rejected"),
            }
        })
    }));
}

pub(super) async fn bind_channel(
    channel: Arc<RTCDataChannel>,
    screens: Arc<tokio::sync::Mutex<super::screens::Screens>>,
    cancel: CancellationToken,
    kcp: crate::transport::fct::FctControl,
    report_target: ReportTarget,
    input: crate::features::host::input::Receiver,
    microphone: crate::features::host::microphone::Receiver,
    audio: crate::features::host::audio::Audio,
    clipboard: crate::features::host::clipboard::Receiver,
    files: crate::features::host::files::Receiver,
    ports: crate::features::port_mapping::host::Receiver,
    annotation: crate::features::host::annotation::Receiver,
    control_ingress: crate::transport::fct::ControlReceiver,
    diagnostics: crate::diagnostics::remote::Host,
) {
    if cancel.is_cancelled() {
        return;
    }
    let control = channel.label() == crate::protocol::peer::channel::CONTROL;
    let text = channel.label() == crate::protocol::peer::channel::SESSION;
    let file = channel.label() == crate::protocol::peer::channel::BULK;
    kcp.bind_stream(channel.id(), control);
    if matches!(
        channel.label(),
        crate::protocol::peer::channel::TUNNEL | crate::protocol::peer::channel::DGRAM
    ) {
        bind_tunnel(channel, ports, cancel).await;
        return;
    }
    if !control && !text && !file {
        return;
    }
    if text {
        annotation.bind(&channel);
    }
    if text || file {
        files.bind(&channel);
        diagnostics.attach(&channel);
    }
    if file {
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
    let closing_annotation = annotation.clone();
    channel.on_close(Box::new(move || {
        if let Some(channel) = close_channel.upgrade() {
            closing_files.close(&channel);
            if text {
                closing_annotation.close(&channel);
            }
            if file {
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
                    if let Err(error) = channel.send(&Bytes::from(hello)).await {
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
            let bytes = crate::features::stream_control::publisher::hello_request();
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
        let annotation = annotation.clone();
        let clipboard = clipboard.clone();
        let diagnostics = diagnostics.clone();
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
            if file {
                // Bulk streams by kind: clipboard and drag data go to the
                // clipboard backend, diagnostics credit to its sender, file
                // streams below.
                match clipboard.receive_bulk(&channel, &message.data) {
                    Ok(true) => return,
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(%error, "clipboard stream data rejected");
                        return;
                    }
                }
            }
            if (text || file) && crate::diagnostics::remote::claims(&message.data) {
                if let Err(error) = diagnostics.receive(&message.data) {
                    tracing::warn!(%error, "diagnostics message rejected");
                }
                return;
            }
            if text && crate::protocol::drag_drop::is_packet(&message.data) {
                if let Err(error) = clipboard.receive(clipboard_generation.unwrap(), &message.data) {
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
                        let _ = channel.send(&Bytes::from(response)).await;
                        return;
                    }
                    Err(error) => {
                        tracing::warn!(%error,"invalid audio quality request");
                        return;
                    }
                    Ok(None) => {}
                }
                match crate::features::stream_control::publisher::control_turn(&message.data) {
                    Ok(Some(turn)) => {
                        let lease = screens.lock().await.authorization();
                        if turn.take
                            && turn.ask
                            && lease.requested()
                            && !lease.granted(farwing_protocol::caps::CONTROL)
                        {
                            // The host's user answers within a minute; the
                            // channel keeps serving meanwhile.
                            let weak = weak.clone();
                            let cancel = cancel.clone();
                            tokio::spawn(async move {
                                let (held, refusal, holder) = if lease.ask_control().await {
                                    control_turn(&lease, true, false)
                                } else {
                                    (
                                        false,
                                        crate::features::stream_control::control_turn::refusal::DECLINED,
                                        String::new(),
                                    )
                                };
                                if let Some(channel) = weak.upgrade()
                                    && !cancel.is_cancelled()
                                    && let Err(error) = channel
                                        .send(&Bytes::from(turn.response(held, refusal, holder)))
                                        .await
                                {
                                    tracing::debug!(%error, "control request response failed");
                                }
                            });
                            return;
                        }
                        let (held, refusal, holder) =
                            control_turn(&lease, turn.take, turn.automatic);
                        if let Some(channel) = weak.upgrade()
                            && !cancel.is_cancelled()
                            && let Err(error) = channel
                                .send(&Bytes::from(turn.response(held, refusal, holder)))
                                .await
                        {
                            tracing::debug!(%error, "control token response failed");
                        }
                        return;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(%error, "invalid control token request");
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
                                    channel.send(&Bytes::from(response)).await
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
            let responses = if crate::protocol::peer::is_type(
                &message.data,
                &[crate::protocol::peer::FrameType::Hello],
            ) {
                crate::features::stream_control::publisher::receive(&message.data)
            } else {
                crate::features::stream_control::publisher::receive_session(
                    &mut *screens.lock().await,
                    &message.data,
                )
                .await
            };
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
                            let result = channel.send(&Bytes::from(response)).await;
                            if let Err(error) = result {
                                tracing::warn!(%error,control,"host business response failed");
                            }
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "host business request failed");
                    // A request is always answered: the controller settles it
                    // (and words the failure in its own language).
                    if let Ok((header, _)) = farwing_protocol::peer::decode(&message.data)
                        && header.request_id != 0
                        && !header.is_response()
                        && header.frame_type() == Some(crate::protocol::peer::FrameType::Session)
                        && let Some(channel) = weak.upgrade()
                    {
                        let reply = crate::features::stream_control::wire::error_response(
                            header.request_id,
                            farwing_protocol::peer::error_code::INTERNAL,
                            "",
                        );
                        if let Err(error) = channel.send(&Bytes::from(reply)).await {
                            tracing::debug!(%error, "host business error reply failed");
                        }
                    }
                }
            }
        })
    }));
}

pub(super) async fn send_business(
    channel: &RTCDataChannel,
    control: bool,
    kcp: &crate::transport::fct::FctControl,
    bytes: Vec<u8>,
) -> Result<usize> {
    if control {
        return kcp.send_control(channel, bytes).await;
    }
    Ok(channel.send(&Bytes::from(bytes)).await?)
}

pub(super) async fn publish_state(
    screens: Arc<tokio::sync::Mutex<super::screens::Screens>>,
    reports: Arc<super::screens::Reports>,
    mut target: tokio::sync::watch::Receiver<ReportRoutes>,
    cancel: CancellationToken,
    kcp: crate::transport::fct::FctControl,
    initial: Option<crate::features::host::capture::Screen>,
    audio: super::super::audio::Audio,
    mut microphone: tokio::sync::watch::Receiver<super::super::microphone::Status>,
    lease: crate::features::host::Lease,
) {
    use crate::features::stream_control::publisher;
    use std::sync::atomic::Ordering;
    let mut changed = reports.changed.subscribe();
    let mut last_screen = None;
    let mut last_capture = None;
    let mut last_quality = None;
    let mut last_microphone = None;
    let mut last_audio_quality = None;
    let mut last_grants = None;
    let mut revision = None;
    // The session's preparation has just read the display inventory: the
    // first periodic read (tens of milliseconds) would only hold back the
    // first screen list, and with it the first video frame.
    let mut timer = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(1),
        Duration::from_secs(1),
    );
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let refresh = tokio::select! {
            _=cancel.cancelled()=>break,
            result=changed.changed()=>{if result.is_err(){break;} false},
            result=target.changed()=>{if result.is_err(){break;} false},
            result=microphone.changed()=>{if result.is_err(){break;} false},
            _=timer.tick()=>true,
        };
        let inventory_revision = if refresh {
            screens
                .try_lock()
                .ok()
                .map(|state| state.inventory_revision())
        } else {
            None
        };
        if let Some(revision) = inventory_revision {
            // Driver enumeration must not hold the screen lock or occupy a
            // network runtime worker. At most one periodic read is in flight.
            let result = super::screens::Inventory::collect(&cancel).await;
            match result {
                Ok(None) => break,
                Ok(Some(inventory)) => {
                    if let Ok(mut state) = screens.try_lock() {
                        if let Err(error) = state.maintain(revision, inventory).await {
                            tracing::debug!(%error,"host screen inventory refresh failed");
                        }
                    }
                }
                Err(error) => tracing::debug!(%error,"host screen inventory read failed"),
            }
        }
        let routes = target.borrow_and_update().clone();
        // State is published once the peer answered our HELLO, not merely when
        // the channel opens: the controller must have initialized its session.
        if !routes.handshake_complete || !routes.handshake_answered {
            reports.media_ready.set(false);
            continue;
        }
        if revision != Some(routes.revision) {
            revision = Some(routes.revision);
            last_screen = None;
            last_capture = None;
            last_quality = None;
            last_microphone = None;
            last_audio_quality = None;
            last_grants = None;
        }
        if let Some(hello) = audio.quality_hello()
            && last_audio_quality.as_ref() != Some(&hello)
            && let Some(channel) = routes.text.as_ref().and_then(std::sync::Weak::upgrade)
        {
            let payload = Bytes::copy_from_slice(&hello);
            let sent = tokio::select! {
                _ = cancel.cancelled() => break,
                result = channel.send(&payload) => result,
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
            async move {
                let Some(channel) = channel else {
                    return false;
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
        // Capabilities in effect for this participant.
        let grants = lease.grants();
        if last_grants.as_ref() != Some(&grants)
            && send(
                false,
                crate::protocol::host_event::encode_permissions(grants.clone()),
            )
            .await
        {
            last_grants = Some(grants);
        }
        let mic = microphone.borrow_and_update().clone();
        let state = microphone_event(last_microphone.as_ref(), &mic);
        if state.is_none() || send(false, publisher::microphone_event(state.unwrap())).await {
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
        let screen = publisher::screen_states(&catalog, &mapping, current);
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
        if selected.is_some() || initial.is_some() {
            last_capture = Some(capture);
        }
        // The controller's HELLO exchange must precede its screen list and
        // first video frame.
        if last_screen.is_some() && last_capture.is_some() {
            reports.media_ready.set(true);
        }
        if let Some((_, state)) = selected
            && let Some(encoder) = state.encoder
            && state.quality > 0
        {
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
            if last_quality.as_ref() != Some(&key)
                && send(
                    false,
                    publisher::quality_report(state.quality, encoder, &state.capture),
                )
                .await
            {
                last_quality = Some(key);
            }
        }
    }
}

/// Take or release the control token for this participant: whether it
/// holds the token now, and why not (a refusal code and the holder; the
/// requester words it in its own language).
fn control_turn(
    lease: &crate::features::host::Lease,
    take: bool,
    automatic: bool,
) -> (bool, i32, String) {
    use crate::features::host::participants::{self, Refusal};
    use crate::features::stream_control::control_turn::refusal;
    let Some(key) = lease.participant() else {
        return (false, refusal::UNAVAILABLE, String::new());
    };
    if !take {
        participants::release_control(key);
        return (false, refusal::NONE, String::new());
    }
    if !lease.requested() || !lease.granted(farwing_protocol::caps::CONTROL) {
        return (false, refusal::NOT_ALLOWED, String::new());
    }
    match participants::take_control(key, automatic) {
        Ok(()) => (true, refusal::NONE, String::new()),
        Err(Refusal::NotAllowed) => (false, refusal::NOT_ALLOWED, String::new()),
        Err(Refusal::HeldBy(holder)) => (false, refusal::HELD, holder),
        Err(Refusal::Shared) => (false, refusal::SHARED, String::new()),
    }
}

fn microphone_event(
    previous: Option<&super::super::microphone::Status>,
    next: &super::super::microphone::Status,
) -> Option<i32> {
    use crate::features::stream_control::wire::microphone_state;
    if previous.is_some_and(|previous| {
        previous.policy_generation == next.policy_generation
            && previous.enabled == next.enabled
            && previous.active == next.active
            && previous.error == next.error
    }) {
        return None;
    }
    if next.enabled && next.error.is_some() {
        Some(microphone_state::RESTART)
    } else if next.enabled {
        Some(if next.active {
            microphone_state::ACTIVE
        } else {
            microphone_state::INACTIVE
        })
    } else if previous.is_some_and(|previous| previous.enabled) {
        Some(microphone_state::INACTIVE)
    } else {
        None
    }
}
