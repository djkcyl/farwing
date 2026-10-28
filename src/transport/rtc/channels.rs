//! Farwing peer data channels: binding, frame-type routing and ordered senders.
use super::workers::{SessionWorkers, std_mutex_lock};
use crate::diagnostics::performance::PerformanceMonitor;
use crate::features::stream_control::{
    OutgoingControlMessage, StreamControlHandle, encode_pb_echo_request,
};
use crate::media::ConnectionMediaProfile;
use crate::protocol::peer::{self, FrameType, channel as label};
use crate::transport::fct::FctControl;
use bytes::Bytes;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_init::{RTCDataChannelInit, RTCDataChannelPriority};
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

pub const DATA_CHANNEL_LABELS: [&str; 5] = label::ALL;

#[derive(Clone)]
pub(super) struct DataChannels {
    pub(super) port_mapping: Arc<crate::features::port_mapping::Transport>,
    pub(super) _local_channels: Arc<Vec<Arc<RTCDataChannel>>>,
    pub(super) incoming_channels: Arc<std::sync::Mutex<Vec<Arc<RTCDataChannel>>>>,
    pub(super) performance: PerformanceMonitor,
    pub(super) stream_control: StreamControlHandle,
    pub(super) fct: FctControl,
    pub(super) workers: Arc<SessionWorkers>,
}

impl DataChannels {
    pub(super) fn new(
        local_channels: Vec<Arc<RTCDataChannel>>,
        performance: PerformanceMonitor,
        profile: ConnectionMediaProfile,
        fct: FctControl,
        connection: Weak<RTCPeerConnection>,
    ) -> Self {
        let workers = SessionWorkers::new();
        let (stream_control, control_messages, echo_responses) =
            StreamControlHandle::new(profile, performance.clone());
        let port_mapping = Arc::new(crate::features::port_mapping::Transport::default());
        for channel in &local_channels {
            if matches!(channel.label(), label::TUNNEL | label::DGRAM) {
                let channel = Arc::clone(channel);
                let mapping = Arc::clone(&port_mapping);
                workers.spawn(async move { mapping.bind(&channel).await });
            }
            stream_control.file_transfer().bind(channel);
            stream_control.drag_drop().bind(channel);
            stream_control.diagnostics().bind(channel);
            Self::install_handlers(
                channel,
                performance.clone(),
                stream_control.clone(),
                true,
                fct.clone(),
                &workers,
                Arc::clone(&port_mapping),
            );
        }
        let control_channel = local_channels
            .iter()
            .find(|channel| channel.label() == label::CONTROL)
            .expect("the Farwing data-channel set always includes the control channel")
            .clone();
        let text_channel = local_channels
            .iter()
            .find(|channel| channel.label() == label::SESSION)
            .expect("the Farwing data-channel set always includes the session channel")
            .clone();
        let bulk_channel = local_channels
            .iter()
            .find(|channel| channel.label() == label::BULK)
            .expect("the Farwing data-channel set always includes the bulk channel")
            .clone();
        let clipboard = clipboard_of(&stream_control);
        let clipboard_channel = text_channel.clone();
        workers.spawn(async move {
            clipboard.run_sender(clipboard_channel, bulk_channel).await;
        });
        workers.spawn(send_remote_input(
            control_channel.clone(),
            fct.clone(),
            stream_control.clone(),
        ));
        workers.spawn(send_control_messages(
            control_channel,
            text_channel,
            control_messages,
            echo_responses,
            stream_control.clone(),
            fct.clone(),
            connection,
        ));
        Self {
            _local_channels: Arc::new(local_channels),
            port_mapping,
            incoming_channels: Arc::new(std::sync::Mutex::new(Vec::new())),
            performance,
            stream_control,
            fct,
            workers,
        }
    }

    pub(super) fn install_handlers(
        channel: &Arc<RTCDataChannel>,
        _performance: PerformanceMonitor,
        stream_control: StreamControlHandle,
        local_channel: bool,
        fct: FctControl,
        workers: &Arc<SessionWorkers>,
        port_mapping: Arc<crate::features::port_mapping::Transport>,
    ) {
        if !local_channel {
            fct.bind_stream(channel.id(), channel.label() == label::CONTROL);
        }
        let channel_label = channel.label().to_owned();
        let opened_channel = Arc::downgrade(channel);
        let open_stream_control = stream_control.clone();
        let open_kcp = fct.clone();
        let open_workers = Arc::downgrade(workers);
        let open_mapping = Arc::clone(&port_mapping);
        channel.on_open(Box::new(move || {
            open_mapping.wake();
            let label = channel_label.clone();
            let opened_channel = opened_channel.clone();
            let stream_control = open_stream_control.clone();
            let fct = open_kcp.clone();
            let _workers = open_workers.clone();
            Box::pin(async move {
                let Some(opened_channel) = opened_channel.upgrade() else {
                    return;
                };
                tracing::info!(%label, stream_id = opened_channel.id(), "peer data channel opened");
                if local_channel && matches!(label.as_str(), label::CONTROL | label::SESSION) {
                    stream_control.set_data_channel_open(&label, true);
                }
                if label == label::CONTROL {
                    fct.set_control_stream(opened_channel.id(), true);
                } else {
                    fct.bind_stream(opened_channel.id(), false);
                }
            })
        }));

        let label = channel.label().to_owned();
        let close_stream_control = stream_control.clone();
        let closed_channel = Arc::downgrade(channel);
        let close_mapping = Arc::clone(&port_mapping);
        channel.on_close(Box::new(move || {
            if local_channel && matches!(label.as_str(), label::SESSION | label::BULK) {
                close_stream_control.file_transfer().close();
                if let Some(channel) = closed_channel.upgrade() {
                    close_stream_control.drag_drop().close(&channel);
                }
            }
            if local_channel && label == label::BULK {
                close_mapping.close();
            }
            let label = label.clone();
            let stream_control = close_stream_control.clone();
            let closed_channel = closed_channel.clone();
            let fct = fct.clone();
            Box::pin(async move {
                tracing::info!(%label, "peer data channel closed");
                if label == label::CONTROL
                    && let Some(channel) = closed_channel.upgrade()
                {
                    fct.set_control_stream(channel.id(), false);
                }
                if local_channel && matches!(label.as_str(), label::CONTROL | label::SESSION) {
                    stream_control.set_data_channel_open(&label, false);
                }
            })
        }));

        let label = channel.label().to_owned();
        channel.on_error(Box::new(move |error| {
            let label = label.clone();
            Box::pin(async move {
                tracing::warn!(%label, %error, "peer data channel error");
            })
        }));

        let channel_label = channel.label().to_owned();
        let message_stream_control = stream_control;
        let drag_channel = Arc::downgrade(channel);
        channel.on_message(Box::new(move |message| {
            let drag_channel = drag_channel.clone();
            let mapping = Arc::clone(&port_mapping);
            let label = channel_label.clone();
            let stream_control = message_stream_control.clone();
            Box::pin(async move {
                route_message(
                    &label,
                    local_channel,
                    &message.data,
                    &stream_control,
                    &mapping,
                    drag_channel.upgrade(),
                )
                .await;
            })
        }));
    }

    pub(super) fn attach_remote_channel(&self, channel: Arc<RTCDataChannel>) {
        let label = channel.label().to_owned();
        tracing::info!(%label, "remote peer data channel received");
        Self::install_handlers(
            &channel,
            self.performance.clone(),
            self.stream_control.clone(),
            false,
            self.fct.clone(),
            &self.workers,
            Arc::clone(&self.port_mapping),
        );
        std_mutex_lock(&self.incoming_channels).push(channel);
    }
}

/// Route one received frame by its type. Channels only select delivery
/// quality; the frame header decides the owner.
async fn route_message(
    label: &str,
    local_channel: bool,
    data: &Bytes,
    stream_control: &StreamControlHandle,
    mapping: &Arc<crate::features::port_mapping::Transport>,
    channel: Option<Arc<RTCDataChannel>>,
) {
    let header = match peer::decode(data) {
        Ok((header, _)) => header,
        Err(error) => {
            tracing::warn!(%label, %error, bytes = data.len(), "discarded data that is not a Farwing peer frame");
            return;
        }
    };
    let result = match header.frame_type() {
        Some(FrameType::Drag) => match (local_channel, channel) {
            (true, Some(channel)) => stream_control
                .drag_drop()
                .receive(&channel, data, false)
                .map_err(|error| ("native drag", error)),
            _ => Ok(()),
        },
        Some(FrameType::Port) => mapping
            .receive(data)
            .map(drop)
            .map_err(|error| ("port mapping", error)),
        Some(FrameType::Flow) if label == label::TUNNEL => mapping
            .receive(data)
            .map(drop)
            .map_err(|error| ("port mapping", error)),
        // Bulk streams belong to the feature named by their kind.
        Some(FrameType::File | FrameType::Transfer | FrameType::Flow) => {
            use crate::transport::transfer::kind;
            match crate::transport::transfer::kind_of(data) {
                Some(kind::FILE) | None => stream_control
                    .file_transfer()
                    .receive(data)
                    .await
                    .map(|_| ())
                    .map_err(|error| ("file transfer", error)),
                Some(kind::CLIPBOARD) => receive_clipboard(stream_control, data),
                Some(kind::DRAG) => match (local_channel, channel) {
                    (true, Some(channel)) => stream_control
                        .drag_drop()
                        .receive(&channel, data, true)
                        .map_err(|error| ("native drag", error)),
                    _ => Ok(()),
                },
                Some(kind::DIAGNOSTICS) => stream_control
                    .diagnostics()
                    .receive(data)
                    .map(drop)
                    .map_err(|error| ("diagnostics", error)),
                Some(other) => {
                    tracing::debug!(kind = other, "ignored bulk stream without a controller owner");
                    Ok(())
                }
            }
        }
        Some(FrameType::Diagnostics) => stream_control
            .diagnostics()
            .receive(data)
            .map(drop)
            .map_err(|error| ("diagnostics", error)),
        Some(FrameType::PortDatagram) => mapping
            .receive_datagram(data)
            .map(drop)
            .map_err(|error| ("port mapping", error)),
        Some(FrameType::Clipboard) => receive_clipboard(stream_control, data),
        Some(
            FrameType::Hello
            | FrameType::Session
            | FrameType::Annotation
            | FrameType::Audio
            | FrameType::Cursor
            | FrameType::HostEvent
            | FrameType::Permissions
            | FrameType::Participants,
        ) => stream_control
            .handle_protocol_message(data)
            .map_err(|error| ("session", error)),
        Some(other) => {
            tracing::debug!(%label, kind = other.name(), "ignored peer frame without a controller consumer");
            Ok(())
        }
        None => {
            tracing::debug!(%label, kind = header.kind, "ignored unknown peer frame type");
            Ok(())
        }
    };
    if let Err((owner, error)) = result {
        tracing::warn!(%label, owner, %error, bytes = data.len(), "invalid peer frame");
    }
    tracing::trace!(%label, bytes = data.len(), "peer frame received");
}

/// The clipboard exchange of this connection: the viewer's, or the one
/// `dev clipboard` installed.
fn clipboard_of(stream_control: &StreamControlHandle) -> crate::features::clipboard::Clipboard {
    crate::features::clipboard::dev_exchange().unwrap_or_else(|| stream_control.clipboard().clone())
}

fn receive_clipboard(
    stream_control: &StreamControlHandle,
    data: &Bytes,
) -> Result<(), (&'static str, anyhow::Error)> {
    let clipboard = clipboard_of(stream_control);
    match clipboard.receive(data) {
        Ok(_) => Ok(()),
        Err(error) => {
            clipboard.protocol_error(error.to_string());
            Err(("clipboard", error))
        }
    }
}

pub(super) async fn send_control_messages(
    control_channel: Arc<RTCDataChannel>,
    text_channel: Arc<RTCDataChannel>,
    mut messages: mpsc::UnboundedReceiver<OutgoingControlMessage>,
    mut echo_responses: mpsc::UnboundedReceiver<Vec<u8>>,
    stream_control: StreamControlHandle,
    fct: FctControl,
    connection: Weak<RTCPeerConnection>,
) {
    const PB_CONNECT_INTERVAL: Duration = Duration::from_millis(500);
    const PB_CONNECT_MAX_ATTEMPTS: u32 = 30;
    let notifications = stream_control.protocol_notifications();
    let mut generation = None;
    let mut annotation_tick = tokio::time::interval(Duration::from_millis(16));
    annotation_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut stats_tick = tokio::time::interval(Duration::from_secs(1));
    stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut attempts = 0_u32;
    let mut timer: Option<tokio::time::Interval> = None;
    let mut profile = crate::features::stream_control::ProfileWatch::new();
    loop {
        let changed = notifications.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let status = stream_control.handshake_status();
        if generation != Some(status.generation) {
            generation = Some(status.generation);
            attempts = 0;
            timer = status.open.then(|| {
                // HELLO is sent once immediately, then every 500 ms.
                let mut timer = tokio::time::interval(PB_CONNECT_INTERVAL);
                timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                timer
            });
        }
        if !status.open || status.connected {
            timer = None;
        }
        if control_channel.ready_state() == RTCDataChannelState::Closed
            && text_channel.ready_state() == RTCDataChannelState::Closed
        {
            break;
        }
        tokio::select! {
            _ = &mut changed => continue,
            _ = annotation_tick.tick() => stream_control.annotation_tick(),
            _ = stats_tick.tick() => {
                if let Some(frame) = stream_control.receiver_stats_frame()
                    && let Err(error) = text_channel.send(&Bytes::from(frame)).await
                {
                    tracing::debug!(%error, "receiver statistics send failed");
                }
                // A cursor colour changed in settings applies at once.
                if status.connected
                    && let Some(hello) = profile.changed()
                {
                    match fct.send_control(&control_channel, hello).await {
                        Ok(_) => tracing::info!("cursor colour change sent"),
                        Err(error) => tracing::warn!(%error, "cursor colour change send failed"),
                    }
                }
            }
            _ = async {
                if let Some(timer) = timer.as_mut() { timer.tick().await; }
                else { std::future::pending::<()>().await; }
            } => {
                // The original tick checks the active peer state before
                // consuming an attempt. A disconnected transport is not an
                // unanswered HELLO request.
                match connection.upgrade().map(|peer| peer.connection_state()) {
                    Some(RTCPeerConnectionState::Connected) => {}
                    None | Some(RTCPeerConnectionState::Closed) => break,
                    _ => continue,
                }
                if attempts >= PB_CONNECT_MAX_ATTEMPTS {
                    timer = None;
                    stream_control.mark_pb_handshake_timeout();
                    tracing::warn!(attempts, "peer HELLO timed out; no feature level was negotiated");
                    continue;
                }
                attempts += 1;
                match fct.send_control(&control_channel, encode_pb_echo_request()).await {
                    Ok(bytes) => tracing::debug!(attempt = attempts, bytes, "sent peer HELLO"),
                    Err(error) => tracing::warn!(attempt = attempts, %error, "peer HELLO send failed"),
                }
            }
            message = messages.recv() => {
                let Some(message) = message else { break; };
                if let Some(generation) = message.annotation_generation {
                    if !stream_control.annotation_message_current(message.sequence, generation) { continue; }
                }
                let is_annotation = message.annotation_generation.is_some();
                let payload = Bytes::from(message.payload);
                let is_screen_request = message.completion.is_some();
                match text_channel.send(&payload).await {
                    Ok(bytes) => {
                        if let Some(done) = message.completion { let _ = done.send(Ok(())); }
                        if is_annotation { tracing::debug!(sequence = message.sequence, bytes, "annotation request sent"); }
                        else { tracing::info!(sequence = message.sequence, bytes, is_screen_request, protocol = message.protocol.label(), "viewing control request sent"); }
                    }
                    Err(error) => {
                        let description = error.to_string();
                        if let Some(done) = message.completion { let _ = done.send(Err(description.clone())); }
                        if is_annotation { stream_control.annotation_send_failed(message.sequence, &description); }
                        else if !is_screen_request { stream_control.mark_send_failed(message.sequence, &description); }
                        tracing::warn!(sequence = message.sequence, %error, "runtime viewer request send failed");
                    }
                }
            }
            response = echo_responses.recv() => {
                let Some(response) = response else { break; };
                match fct.send_control(&control_channel, response).await {
                    Ok(bytes) => tracing::debug!(bytes, "sent peer HELLO response"),
                    Err(error) => tracing::warn!(%error, "peer HELLO response send failed"),
                }
            }
        }
    }
}

pub(super) async fn send_remote_input(
    channel: Arc<RTCDataChannel>,
    kcp: FctControl,
    stream_control: StreamControlHandle,
) {
    let mouse = stream_control.mouse().clone();
    let mut heartbeat = tokio::time::interval(Duration::from_millis(15));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut keyboard_submission_seen = false;
    loop {
        let work = tokio::select! {
            event = mouse.next() => event,
            _ = heartbeat.tick() => { mouse.heartbeat(); continue; }
        };
        let mut event = match work {
            crate::features::remote_input::InputWork::Event(event) => event,
            crate::features::remote_input::InputWork::Recovery(recovery) => {
                stream_control.reconcile_mouse_failure();
                recover_remote_input(&channel, &kcp, &mouse, recovery).await;
                continue;
            }
        };
        if !mouse.is_current(&event) {
            mouse.discard(&event);
            continue;
        }
        let mut timed_out = false;
        let epoch = event.epoch;
        let result = tokio::select! {
            biased;
            _ = mouse.epoch_cancelled(epoch) => Err(anyhow::anyhow!("{}", crate::i18n::t().session.mouse_channel_replaced)),
            result = tokio::time::timeout(event.event.send_timeout(),
                async {
                    let release=event.event.releases();
                    anyhow::ensure!(kcp.is_negotiated(), "control transport not negotiated");
                    if !release {
                        kcp.wait_input_capacity().await?;
                    }
                    anyhow::ensure!(mouse.refresh_pending(&mut event), "input no longer current before submission");
                    let state = mouse.clone();
                    let guarded = event.clone();
                    kcp.send_input(channel.id(), event.event.encode(), Arc::new(move || state.is_current(&guarded)), release).await
                }) => result
                .map_err(|_| { timed_out = true; match kcp.send_failure_reason() {
                    Some(reason) => anyhow::anyhow!("{}", (crate::i18n::t().session.input_send_timeout_reason)(&reason.to_string())),
                    None => anyhow::anyhow!("{}", crate::i18n::t().session.input_send_timeout),
                } })
                .and_then(|result| result.map(|_| ())),
        };
        if let Err(error) = &result
            && let Some(recovery) = mouse.pause_transport(&event)
        {
            tracing::warn!(
                %error,
                timed_out,
                worker_stage = kcp.worker_stage(),
                "remote input send interrupted; suspending until reliable release acknowledgement"
            );
            stream_control.reconcile_mouse_failure();
            recover_remote_input(&channel, &kcp, &mouse, recovery).await;
            continue;
        }
        if let Err(error) = &result
            && mouse.is_current(&event)
        {
            use crate::features::remote_input::InputEvent;
            let kind = match event.event {
                InputEvent::Key { .. } => "keyboard",
                InputEvent::Button { .. } | InputEvent::AssistButton { .. } => "button",
                InputEvent::Relative { .. }
                | InputEvent::Absolute { .. }
                | InputEvent::Correction { .. } => "movement",
                InputEvent::Wheel { .. } => "wheel",
                InputEvent::Heartbeat => "keyboard_heartbeat",
                InputEvent::Pointer { .. } => "touch_pen",
            };
            tracing::warn!(target: "farwing::transport::rtc::input", %error, input_kind=kind,
                carrier=if kcp.is_negotiated(){"fct"}else{"sctp"},
                worker_stage=kcp.worker_stage(), "remote input transport failed");
        }
        if !keyboard_submission_seen
            && result.is_ok()
            && matches!(
                event.event,
                crate::features::remote_input::InputEvent::Key { .. }
            )
        {
            keyboard_submission_seen = true;
            tracing::debug!(target: "farwing::transport::rtc::input",
                down = matches!(event.event, crate::features::remote_input::InputEvent::Key { down: true, .. }),
                "keyboard event submitted to the control transport");
        }
        mouse.complete(&event, result);
    }
}

async fn recover_remote_input(
    channel: &RTCDataChannel,
    kcp: &FctControl,
    mouse: &crate::features::remote_input::RemoteInput,
    recovery: crate::features::remote_input::InputRecovery,
) {
    use crate::features::remote_input::InputEvent;
    let epoch = recovery.epoch;
    let recover = async {
        // One release pass followed by a harmless, reliably acknowledged probe.
        // Never resubmit the failed press/motion or switch carriers on failure.
        anyhow::ensure!(kcp.is_negotiated(), "control transport not negotiated");
        for event in recovery.releases.into_iter().chain([InputEvent::Heartbeat]) {
            anyhow::ensure!(mouse.recovery_current(epoch), "input recovery cancelled");
            let state = mouse.clone();
            kcp.send_input(
                channel.id(),
                event.encode(),
                Arc::new(move || state.recovery_current(epoch)),
                true,
            )
            .await?;
        }
        kcp.drain().await?;
        Ok(())
    };
    tokio::select! {
        biased;
        _ = mouse.epoch_cancelled(epoch) => {},
        result = recover => mouse.finish_transport_recovery(epoch, result),
    }
}

pub(super) fn data_channel_init(label: &str) -> RTCDataChannelInit {
    let priority = match label {
        label::CONTROL | label::BULK => RTCDataChannelPriority::High,
        label::SESSION => RTCDataChannelPriority::Medium,
        _ => RTCDataChannelPriority::Low,
    };
    if label == label::DGRAM {
        // UDP forwarding: a late datagram is useless; never retransmit.
        return RTCDataChannelInit {
            ordered: Some(false),
            max_retransmits: Some(0),
            priority: Some(priority),
            ..Default::default()
        };
    }
    RTCDataChannelInit {
        ordered: Some(true),
        priority: Some(priority),
        ..Default::default()
    }
}
