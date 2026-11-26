//! Reliable business-channel binding, routing and ordered senders.
use super::statistics::send_official_streamer_statistics;
use super::workers::{SessionWorkers, std_mutex_lock};
use crate::diagnostics::performance::PerformanceMonitor;
use crate::features::stream_control::{
    OutgoingControlMessage, PbMessageSource, StreamControlHandle, encode_pb_echo_request,
};
use crate::media::ConnectionMediaProfile;
use crate::transport::uu_kcp::UuKcpControl;
use bytes::Bytes;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_init::{RTCDataChannelInit, RTCDataChannelPriority};
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

pub const DATA_CHANNEL_LABELS: [&str; 5] = [
    "CONTROL_DATA_CHANNEL",
    "TEXT_DATA_CHANNEL",
    "STREAMER_DATA_CHANNEL",
    "FILE_DATA_CHANNEL",
    "BINARY_DATA_CHANNEL",
];

#[derive(Clone)]
pub(super) struct DataChannels {
    pub(super) port_mapping: Arc<crate::features::port_mapping::Transport>,
    pub(super) terminal: Arc<crate::features::terminal::client::Transport>,
    pub(super) _local_channels: Arc<Vec<Arc<RTCDataChannel>>>,
    pub(super) incoming_channels: Arc<std::sync::Mutex<Vec<Arc<RTCDataChannel>>>>,
    pub(super) performance: PerformanceMonitor,
    pub(super) streamer_sender_started: Arc<AtomicBool>,
    pub(super) stream_control: StreamControlHandle,
    pub(super) uu_kcp: UuKcpControl,
    pub(super) workers: Arc<SessionWorkers>,
}

impl DataChannels {
    pub(super) fn new(
        local_channels: Vec<Arc<RTCDataChannel>>,
        performance: PerformanceMonitor,
        profile: ConnectionMediaProfile,
        uu_kcp: UuKcpControl,
        connection: Weak<RTCPeerConnection>,
        terminal: bool,
    ) -> Self {
        let workers = SessionWorkers::new("RTC peer");
        let streamer_sender_started = Arc::new(AtomicBool::new(false));
        let (stream_control, control_messages, echo_responses) =
            StreamControlHandle::new(profile, performance.clone());
        let port_mapping = Arc::new(crate::features::port_mapping::Transport::default());
        let terminal_transport = Arc::new(crate::features::terminal::client::Transport::default());
        for channel in &local_channels {
            if channel.label() == crate::diagnostics::remote::CHANNEL {
                uu_kcp.bind_stream(channel.id(), false);
                if terminal {
                    terminal_transport.bind(channel);
                } else {
                    stream_control.diagnostics().bind(channel);
                }
                continue;
            }
            stream_control.file_transfer().bind(channel);
            stream_control.drag_drop().bind(channel);
            if channel.label() == "FILE_DATA_CHANNEL" {
                port_mapping.bind(channel);
                let binary = Arc::clone(channel);
                let mapping = Arc::clone(&port_mapping);
                let files = Arc::clone(stream_control.file_transfer());
                workers.spawn(async move {
                    binary
                        .set_buffered_amount_low_threshold(4 * 1024 * 1024)
                        .await;
                    binary
                        .on_buffered_amount_low(Box::new(move || {
                            mapping.wake();
                            files.wake();
                            Box::pin(async {})
                        }))
                        .await;
                });
            }
            Self::install_handlers(
                channel,
                performance.clone(),
                Arc::clone(&streamer_sender_started),
                stream_control.clone(),
                true,
                uu_kcp.clone(),
                &workers,
                Arc::clone(&port_mapping),
            );
        }
        let control_channel = local_channels
            .iter()
            .find(|channel| channel.label() == "CONTROL_DATA_CHANNEL")
            .expect("the official data-channel set always includes CONTROL_DATA_CHANNEL")
            .clone();
        let text_channel = local_channels
            .iter()
            .find(|channel| channel.label() == "TEXT_DATA_CHANNEL")
            .expect("the official data-channel set always includes TEXT_DATA_CHANNEL")
            .clone();
        let clipboard = stream_control.clipboard().clone();
        let clipboard_channel = text_channel.clone();
        workers.spawn(async move {
            clipboard.run_sender(clipboard_channel).await;
        });
        workers.spawn(send_remote_input(
            control_channel.clone(),
            uu_kcp.clone(),
            stream_control.clone(),
        ));
        workers.spawn(send_official_control_messages(
            control_channel,
            text_channel,
            control_messages,
            echo_responses,
            stream_control.clone(),
            uu_kcp.clone(),
            connection,
        ));
        Self {
            _local_channels: Arc::new(local_channels),
            port_mapping,
            terminal: terminal_transport,
            incoming_channels: Arc::new(std::sync::Mutex::new(Vec::new())),
            performance,
            streamer_sender_started,
            stream_control,
            uu_kcp,
            workers,
        }
    }

    pub(super) fn install_handlers(
        channel: &Arc<RTCDataChannel>,
        performance: PerformanceMonitor,
        streamer_sender_started: Arc<AtomicBool>,
        stream_control: StreamControlHandle,
        local_channel: bool,
        uu_kcp: UuKcpControl,
        workers: &Arc<SessionWorkers>,
        port_mapping: Arc<crate::features::port_mapping::Transport>,
    ) {
        if !local_channel {
            uu_kcp.bind_stream(channel.id(), channel.label() == "CONTROL_DATA_CHANNEL");
        }
        let label = channel.label().to_owned();
        let stats_channel = Arc::downgrade(channel);
        let stats_performance = performance.clone();
        let open_stream_control = stream_control.clone();
        let open_kcp = uu_kcp.clone();
        let open_workers = Arc::downgrade(workers);
        let open_mapping = Arc::clone(&port_mapping);
        channel.on_open(Box::new(move || {
            open_mapping.wake();
            let label = label.clone();
            let stats_channel = stats_channel.clone();
            let stats_performance = stats_performance.clone();
            let streamer_sender_started = Arc::clone(&streamer_sender_started);
            let stream_control = open_stream_control.clone();
            let uu_kcp = open_kcp.clone();
            Box::pin(async move {
                let Some(stats_channel) = stats_channel.upgrade() else {
                    return;
                };
                tracing::info!(%label, stream_id = stats_channel.id(), "UU data channel opened");
                if local_channel
                    && matches!(label.as_str(), "CONTROL_DATA_CHANNEL" | "TEXT_DATA_CHANNEL")
                {
                    stream_control.set_data_channel_open(&label, true);
                }
                if label == "CONTROL_DATA_CHANNEL" {
                    uu_kcp.set_control_stream(stats_channel.id(), true);
                } else {
                    uu_kcp.bind_stream(stats_channel.id(), false);
                }
                if label == "STREAMER_DATA_CHANNEL"
                    && streamer_sender_started
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    && let Some(workers) = open_workers.upgrade()
                {
                    workers.spawn(send_official_streamer_statistics(
                        stats_channel,
                        stats_performance,
                    ));
                }
            })
        }));

        let label = channel.label().to_owned();
        let close_stream_control = stream_control.clone();
        let closed_channel = Arc::downgrade(channel);
        let close_mapping = Arc::clone(&port_mapping);
        channel.on_close(Box::new(move || {
            if local_channel && matches!(label.as_str(), "TEXT_DATA_CHANNEL" | "FILE_DATA_CHANNEL")
            {
                close_stream_control.file_transfer().close();
                if let Some(channel) = closed_channel.upgrade() {
                    close_stream_control.drag_drop().close(&channel);
                }
            }
            if local_channel && label == "FILE_DATA_CHANNEL" {
                close_mapping.close();
            }
            let label = label.clone();
            let stream_control = close_stream_control.clone();
            let closed_channel = closed_channel.clone();
            let uu_kcp = uu_kcp.clone();
            Box::pin(async move {
                tracing::info!(%label, "UU data channel closed");
                if label == "CONTROL_DATA_CHANNEL"
                    && let Some(channel) = closed_channel.upgrade()
                {
                    uu_kcp.set_control_stream(channel.id(), false);
                }
                if local_channel
                    && matches!(label.as_str(), "CONTROL_DATA_CHANNEL" | "TEXT_DATA_CHANNEL")
                {
                    stream_control.set_data_channel_open(&label, false);
                }
            })
        }));

        let label = channel.label().to_owned();
        channel.on_error(Box::new(move |error| {
            let label = label.clone();
            Box::pin(async move {
                tracing::warn!(%label, %error, "UU data channel error");
            })
        }));

        let label = channel.label().to_owned();
        let message_stream_control = stream_control;
        let drag_channel = Arc::downgrade(channel);
        channel.on_message(Box::new(move |message| {
            let drag_channel=drag_channel.clone();
            let mapping=Arc::clone(&port_mapping);
            let label = label.clone();
            let stream_control = message_stream_control.clone();
            Box::pin(async move {
                if matches!(label.as_str(), "TEXT_DATA_CHANNEL" | "FILE_DATA_CHANNEL") && crate::protocol::drag_drop::is_packet(&message.data) {
                    if local_channel && let Some(channel)=drag_channel.upgrade() {
                        if let Err(error) = stream_control.drag_drop().receive(&channel, &message.data, label == "FILE_DATA_CHANNEL") { tracing::warn!(%error,"invalid native drag message"); }
                    }
                } else if label == "TEXT_DATA_CHANNEL" && crate::protocol::annotation::is_packet(&message.data) {
                    if let Err(error)=stream_control.handle_native_annotation(&message.data) {tracing::warn!(%error,"invalid native annotation message");}
                } else if matches!(label.as_str(), "TEXT_DATA_CHANNEL" | "FILE_DATA_CHANNEL") && !message.data.starts_with(b"{")
                    && stream_control.file_transfer().receive(&message.data).await.unwrap_or_else(|error| {
                        tracing::warn!(%error,"invalid file transfer message"); false
                    }) {
                    // Only solicited transfer RPCs reach their registered task.
                } else if label=="FILE_DATA_CHANNEL" {
                    if let Err(error)=mapping.receive(&message.data) {tracing::warn!(%error,"invalid port mapping message");}
                } else if label == "TEXT_DATA_CHANNEL" && !message.data.starts_with(b"{")
                    && stream_control.clipboard().receive(&message.data).unwrap_or_else(|error| {
                        stream_control.clipboard().protocol_error(error.to_string());
                        tracing::warn!(%error,"invalid clipboard message"); true
                    }) {
                    // Clipboard owns only its RPC fields, independently of video requests.
                } else if label == "STREAMER_DATA_CHANNEL" {
                    // Peer media_inbounds describe the peer's receive direction.
                    // This client has no UU video sender. Do not use
                    // those reports as measurements of our local video pipeline.
                    tracing::trace!(bytes = message.data.len(), "peer receiver statistics not used for incoming video");
                } else if matches!(
                    label.as_str(),
                    "CONTROL_DATA_CHANNEL" | "TEXT_DATA_CHANNEL"
                ) && let Err(error) = stream_control.handle_protocol_message(&message.data,
                    if label == "CONTROL_DATA_CHANNEL" { PbMessageSource::Control } else { PbMessageSource::Text })
                {
                    tracing::warn!(
                        %label,
                        %error,
                        bytes = message.data.len(),
                        is_string = message.is_string,
                        "invalid UU protobuf domain message"
                    );
                }
                tracing::trace!(%label, bytes = message.data.len(), "UU data channel message received");
            })
        }));
    }

    pub(super) fn attach_remote_channel(&self, channel: Arc<RTCDataChannel>) {
        let label = channel.label().to_owned();
        tracing::info!(%label, "remote UU data channel received");
        Self::install_handlers(
            &channel,
            self.performance.clone(),
            Arc::clone(&self.streamer_sender_started),
            self.stream_control.clone(),
            false,
            self.uu_kcp.clone(),
            &self.workers,
            Arc::clone(&self.port_mapping),
        );
        std_mutex_lock(&self.incoming_channels).push(channel);
    }
}

pub(super) async fn send_official_control_messages(
    control_channel: Arc<RTCDataChannel>,
    text_channel: Arc<RTCDataChannel>,
    mut messages: mpsc::UnboundedReceiver<OutgoingControlMessage>,
    mut echo_responses: mpsc::UnboundedReceiver<Vec<u8>>,
    stream_control: StreamControlHandle,
    uu_kcp: UuKcpControl,
    connection: Weak<RTCPeerConnection>,
) {
    const PB_CONNECT_INTERVAL: Duration = Duration::from_millis(500);
    const PB_CONNECT_MAX_ATTEMPTS: u32 = 30;
    let notifications = stream_control.protocol_notifications();
    let mut generation = None;
    let mut annotation_tick = tokio::time::interval(Duration::from_millis(16));
    annotation_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut attempts = 0_u32;
    let mut timer: Option<tokio::time::Interval> = None;
    loop {
        let changed = notifications.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let status = stream_control.handshake_status();
        if generation != Some(status.generation) {
            generation = Some(status.generation);
            attempts = 0;
            timer = status.open.then(|| {
                // D4A070 dispatches once immediately, then every 500 ms.
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
            _ = async {
                if let Some(timer) = timer.as_mut() { timer.tick().await; }
                else { std::future::pending::<()>().await; }
            } => {
                // The original tick checks the active peer state before
                // consuming an attempt. A disconnected transport is not an
                // unanswered ECHO request.
                match connection.upgrade().map(|peer| peer.connection_state()) {
                    Some(RTCPeerConnectionState::Connected) => {}
                    None | Some(RTCPeerConnectionState::Closed) => break,
                    _ => continue,
                }
                if attempts >= PB_CONNECT_MAX_ATTEMPTS {
                    timer = None;
                    stream_control.mark_pb_handshake_timeout();
                    tracing::warn!(attempts, "protobuf handshake timed out; no feature version was negotiated");
                    continue;
                }
                attempts += 1;
                match uu_kcp.send_control(&control_channel, encode_pb_echo_request()).await {
                    Ok(bytes) => tracing::debug!(attempt = attempts, bytes, "sent official protobuf ECHO_REQUEST"),
                    Err(error) => tracing::warn!(attempt = attempts, %error, "protobuf ECHO_REQUEST send failed"),
                }
            }
            message = messages.recv() => {
                let Some(mut message) = message else { break; };
                if message.completion.as_ref().is_some_and(|done| done.is_closed()) { continue; }
                if let Some(generation) = message.annotation_generation {
                    if !stream_control.annotation_message_current(message.sequence, generation) { continue; }
                }
                let is_annotation = message.annotation_generation.is_some();
                let payload = Bytes::from(message.payload);
                let is_screen_request = message.completion.is_some();
                let sent = text_channel.send_text_bytes(&payload);
                let result = if let Some(done) = &mut message.completion {
                    // Capacity reservation precedes atomic SCTP packetization.
                    // Withdrawal cannot retract an already admitted message.
                    tokio::select! { biased; _=done.closed()=>continue, result=sent=>result }
                } else { sent.await };
                match result {
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
                match uu_kcp.send_control(&control_channel, response).await {
                    Ok(bytes) => tracing::debug!(bytes, "sent official protobuf ECHO_RESPONSE"),
                    Err(error) => tracing::warn!(%error, "protobuf ECHO_RESPONSE send failed"),
                }
            }
        }
    }
}

pub(super) async fn send_remote_input(
    channel: Arc<RTCDataChannel>,
    kcp: UuKcpControl,
    stream_control: StreamControlHandle,
) {
    use crate::transport::uu_kcp::INPUT_SEND_BUDGET_BYTES;
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
            _ = mouse.epoch_cancelled(epoch) => Err(anyhow::anyhow!("鼠标连接代次已变更")),
            result = tokio::time::timeout(event.event.send_timeout(),
                async {
                    let release=event.event.release();
                    if !release {
                        if kcp.is_negotiated() {
                            kcp.wait_input_capacity().await?;
                        } else {
                            channel.wait_input_capacity(INPUT_SEND_BUDGET_BYTES).await?;
                        }
                    }
                    anyhow::ensure!(mouse.refresh_pending(&mut event), "input no longer current before submission");
                    let state=mouse.clone();let guarded=event.clone();
                    if kcp.is_negotiated(){kcp.send_input(channel.id(),event.event.encode(),Arc::new(move||state.is_current(&guarded)),release).await}
                    else {kcp.send_control(&channel,event.event.encode()).await}
                }) => result
                .map_err(|_| { timed_out = true; match kcp.send_failure_reason() {
                    Some(reason) => anyhow::anyhow!("键鼠输入发送超时：{reason}"),
                    None => anyhow::anyhow!("键鼠输入发送超时"),
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
                InputEvent::CapsLock { .. } => "keyboard_lock_state",
                InputEvent::Button { .. } | InputEvent::AssistButton { .. } => "button",
                InputEvent::Relative { .. }
                | InputEvent::Absolute { .. }
                | InputEvent::Correction { .. } => "movement",
                InputEvent::Wheel { .. } => "wheel",
                InputEvent::Touch { .. } => "touch",
                InputEvent::Heartbeat => "keyboard_heartbeat",
                InputEvent::Gamepad(_) => "gamepad",
            };
            tracing::warn!(target: "openuuyc::transport::rtc::input", %error, input_kind=kind,
                carrier=if kcp.is_negotiated(){"mixed_kcp"}else{"sctp"},
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
            tracing::debug!(target: "openuuyc::transport::rtc::input",
                down = matches!(event.event, crate::features::remote_input::InputEvent::Key { down: true, .. }),
                "keyboard event submitted to CONTROL transport");
        }
        mouse.complete(&event, result);
    }
}

async fn recover_remote_input(
    channel: &RTCDataChannel,
    kcp: &UuKcpControl,
    mouse: &crate::features::remote_input::RemoteInput,
    recovery: crate::features::remote_input::InputRecovery,
) {
    use crate::features::remote_input::InputEvent;
    let epoch = recovery.epoch;
    let recover = async {
        // One release pass followed by a harmless, reliably acknowledged probe.
        // Never resubmit the failed press/motion or switch carriers on failure.
        for event in recovery.releases.into_iter().chain([InputEvent::Heartbeat]) {
            anyhow::ensure!(mouse.recovery_current(epoch), "input recovery cancelled");
            if kcp.is_negotiated() {
                let state = mouse.clone();
                kcp.send_input(
                    channel.id(),
                    event.encode(),
                    Arc::new(move || state.recovery_current(epoch)),
                    true,
                )
                .await?;
            } else {
                kcp.send_control(channel, event.encode()).await?;
            }
        }
        if kcp.is_negotiated() {
            kcp.drain().await?;
        } else {
            // This SCTP implementation releases buffered bytes on SACK. Sending
            // the probe alone only means local enqueue, not a healthy carrier.
            loop {
                anyhow::ensure!(
                    channel.ready_state()
                        == webrtc::data_channel::data_channel_state::RTCDataChannelState::Open,
                    "CONTROL channel closed during input recovery"
                );
                if channel.buffered_amount().await == 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        Ok(())
    };
    tokio::select! {
        biased;
        _ = mouse.epoch_cancelled(epoch) => {},
        result = recover => mouse.finish_transport_recovery(epoch, result),
    }
}

pub(super) fn official_data_channel_init(label: &str) -> RTCDataChannelInit {
    let priority = match label {
        "CONTROL_DATA_CHANNEL" | "FILE_DATA_CHANNEL" => RTCDataChannelPriority::High,
        "TEXT_DATA_CHANNEL" | "STREAMER_DATA_CHANNEL" => RTCDataChannelPriority::Medium,
        "BINARY_DATA_CHANNEL" => RTCDataChannelPriority::Low,
        _ => RTCDataChannelPriority::Low,
    };
    RTCDataChannelInit {
        ordered: Some(true),
        priority: Some(priority),
        ..Default::default()
    }
}
