//! Publisher business-channel routing and observed source reporting.
use super::{ReportRoutes, ReportTarget};
use anyhow::Result;
use bytes::Bytes;
use std::sync::Arc;
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
    // Requests that wait (screens, capture restarts, the microphone, the
    // annotation overlay) are served off the channel's read loop, in order.
    let business = text.then(|| {
        let (sender, requests) = tokio::sync::mpsc::channel::<Bytes>(64);
        tokio::spawn(serve_business(
            requests,
            Business {
                channel: Arc::downgrade(&channel),
                screens: screens.clone(),
                cancel: cancel.clone(),
                report_target: report_target.clone(),
                microphone: microphone.clone(),
                audio: audio.clone(),
                files: files.clone(),
                annotation: annotation.clone(),
            },
        ));
        sender
    });
    let annotations = text.then(|| {
        let (sender, requests) = tokio::sync::mpsc::channel::<Bytes>(64);
        tokio::spawn(serve_annotation(
            requests,
            annotation.clone(),
            Arc::downgrade(&channel),
            cancel.clone(),
        ));
        sender
    });
    let weak = Arc::downgrade(&channel);
    channel.on_message(Box::new(move |message| {
        let weak = weak.clone();
        let control_ingress = control_ingress.clone();
        let cancel = cancel.clone();
        let report_target = report_target.clone();
        let input = input.clone();
        let files = files.clone();
        let clipboard = clipboard.clone();
        let diagnostics = diagnostics.clone();
        let business = business.clone();
        let annotations = annotations.clone();
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
            // One decode, one owner per frame type.
            use crate::protocol::peer::FrameType;
            let Ok((header, _)) = farwing_protocol::peer::decode(&message.data) else {
                return;
            };
            let kind = header.frame_type();
            match kind {
                Some(FrameType::Transfer | FrameType::Flow) => {
                    // Streams by kind: clipboard and drag data go to the
                    // clipboard backend, diagnostics to its sender, files below.
                    if file {
                        match clipboard.receive_bulk(&channel, &message.data) {
                            Ok(true) => return,
                            Ok(false) => {}
                            Err(error) => {
                                tracing::warn!(%error, "clipboard stream data rejected");
                                return;
                            }
                        }
                    }
                    if crate::diagnostics::remote::claims(&message.data) {
                        if let Err(error) = diagnostics.receive(&message.data) {
                            tracing::warn!(%error, "diagnostics message rejected");
                        }
                        return;
                    }
                    if let Err(error) = files.receive(&channel, &message.data).await {
                        tracing::warn!(%error, "host file message rejected");
                    }
                }
                Some(FrameType::Diagnostics) => {
                    if let Err(error) = diagnostics.receive(&message.data) {
                        tracing::warn!(%error, "diagnostics message rejected");
                    }
                }
                Some(FrameType::File) => {
                    if let Err(error) = files.receive(&channel, &message.data).await {
                        tracing::warn!(%error, "host file message rejected");
                    }
                }
                _ if !text => {}
                Some(FrameType::Drag) => {
                    if let Err(error) = clipboard.receive(clipboard_generation.unwrap(), &message.data) {
                        tracing::warn!(%error, "native drag packet rejected");
                    }
                }
                Some(FrameType::Annotation) => {
                    if let Some(annotations) = &annotations
                        && annotations.try_send(message.data).is_err()
                    {
                        tracing::warn!("annotation requests piled up; one dropped");
                    }
                }
                // Only the participant's current session channel.
                _ if !report_target
                    .borrow()
                    .text
                    .as_ref()
                    .is_some_and(|current| current.ptr_eq(&weak)) => {}
                Some(FrameType::Clipboard) => {
                    if let Err(error) = clipboard.receive(clipboard_generation.unwrap(), &message.data) {
                        tracing::warn!(%error, "rejected host clipboard message");
                    }
                }
                _ => {
                    if let Some(business) = &business
                        && business.send(message.data).await.is_err()
                    {
                        tracing::debug!("host business requests closed");
                    }
                }
            }
        })
    }));
}

/// What the session channel's business requests need.
struct Business {
    channel: std::sync::Weak<RTCDataChannel>,
    screens: Arc<tokio::sync::Mutex<super::screens::Screens>>,
    cancel: CancellationToken,
    report_target: ReportTarget,
    microphone: crate::features::host::microphone::Receiver,
    audio: crate::features::host::audio::Audio,
    files: crate::features::host::files::Receiver,
    annotation: crate::features::host::annotation::Receiver,
}

/// Serve AUDIO, SESSION and HELLO requests of one session channel in
/// order, off its read loop; anything else is answered `unsupported`.
async fn serve_business(mut requests: tokio::sync::mpsc::Receiver<Bytes>, business: Business) {
    while let Some(data) = requests.recv().await {
        if business.cancel.is_cancelled() {
            break;
        }
        let Some(channel) = business.channel.upgrade() else {
            break;
        };
        business.serve(&channel, &data).await;
    }
}

/// Annotation requests of one session channel, in order: each waits for
/// the overlay to draw it.
async fn serve_annotation(
    mut requests: tokio::sync::mpsc::Receiver<Bytes>,
    annotation: crate::features::host::annotation::Receiver,
    channel: std::sync::Weak<RTCDataChannel>,
    cancel: CancellationToken,
) {
    while let Some(data) = requests.recv().await {
        if cancel.is_cancelled() {
            break;
        }
        let Some(channel) = channel.upgrade() else {
            break;
        };
        if let Err(error) = annotation.receive(&channel, &data).await {
            tracing::warn!(%error, "annotation request rejected");
        }
    }
}

impl Business {
    async fn serve(&self, channel: &Arc<RTCDataChannel>, data: &[u8]) {
        let Business {
            channel: weak,
            screens,
            cancel,
            report_target,
            microphone,
            audio,
            files,
            annotation,
        } = self;
        {
            match audio.quality_request(data) {
                Ok(Some(response)) => {
                    let _ = crate::transport::outbox::send(channel, Bytes::from(response)).await;
                    return;
                }
                Err(error) => {
                    tracing::warn!(%error,"invalid audio quality request");
                    return;
                }
                Ok(None) => {}
            }
            match crate::features::stream_control::publisher::control_turn(data) {
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
                            let result = if lease.ask_control().await {
                                control_turn(&lease, true, false)
                            } else {
                                Err(crate::features::stream_control::control_turn::refusal::DECLINED)
                            };
                            if let Some(channel) = weak.upgrade()
                                && !cancel.is_cancelled()
                                && let Err(error) = crate::transport::outbox::send(&channel, Bytes::from(turn.response(result)))
                                    .await
                            {
                                tracing::debug!(%error, "control request response failed");
                            }
                        });
                        return;
                    }
                    let result = control_turn(&lease, turn.take, turn.automatic);
                    if let Some(channel) = weak.upgrade()
                        && !cancel.is_cancelled()
                        && let Err(error) = crate::transport::outbox::send(&channel, Bytes::from(turn.response(result)))
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
            match crate::features::stream_control::publisher::microphone_policy(data) {
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
                                    .map(drop)
                                    .map_err(crate::features::host::microphone::refusal),
                            );
                            if let Err(error) =
                                crate::transport::outbox::send(&channel, Bytes::from(response)).await
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
            bytes = data.len(),
            tag = data.first().copied(),
            "host business message received"
        );
        let responses = match farwing_protocol::peer::decode(data)
            .ok()
            .and_then(|(header, _)| header.frame_type())
        {
            Some(crate::protocol::peer::FrameType::Hello) => {
                crate::features::stream_control::publisher::receive(data)
            }
            Some(crate::protocol::peer::FrameType::Session) => {
                crate::features::stream_control::publisher::receive_session(
                    &mut *screens.lock().await,
                    data,
                )
                .await
            }
            _ => {
                if let Some(reply) = crate::protocol::peer::unsupported(data)
                    && let Err(error) = crate::transport::outbox::send(channel, Bytes::from(reply)).await
                {
                    tracing::debug!(%error, "host unsupported-request reply failed");
                }
                return;
            }
        };
        match responses {
            Ok(responses) => {
                if let Some(caps) = responses.files {
                    files.capabilities(caps);
                }
                if let Some(active) = responses.media_active {
                    audio.set_media_allowed(active);
                    if annotation.viewing(active) {
                        annotation.notify_reset().await;
                    }
                }
                report_target.send_if_modified(|routes| routes.received(&responses));
                if let Some(channel) = weak.upgrade() {
                    for response in responses.messages {
                        if cancel.is_cancelled() {
                            break;
                        }
                        let result = crate::transport::outbox::send(&channel, Bytes::from(response)).await;
                        if let Err(error) = result {
                            tracing::warn!(%error, "host business response failed");
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "host business request failed");
                // A request is always answered: the controller settles it
                // (and words the failure in its own language).
                if let Ok((header, _)) = farwing_protocol::peer::decode(data)
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
                    if let Err(error) = crate::transport::outbox::send(&channel, Bytes::from(reply)).await {
                        tracing::debug!(%error, "host business error reply failed");
                    }
                }
            }
        }
    }
}

pub(super) async fn send_business(
    channel: &Arc<RTCDataChannel>,
    control: bool,
    kcp: &crate::transport::fct::FctControl,
    bytes: Vec<u8>,
) -> Result<()> {
    if control {
        kcp.send_control(channel, bytes).await?;
        return Ok(());
    }
    crate::transport::outbox::send(channel, Bytes::from(bytes)).await
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
    let mut changed = reports.changed.subscribe();
    let mut last_screen = None;
    let mut last_capture = None;
    let mut last_quality = None;
    let mut last_microphone = None;
    let mut last_audio_quality = None;
    let mut last_grants = None;
    let mut revision = None;
    // The process reads the displays once a second for every participant;
    // each one maintains its screens and tracks on every read.
    let mut inventory = super::screens::shared::subscribe();
    loop {
        let refresh = tokio::select! {
            _=cancel.cancelled()=>break,
            result=changed.changed()=>{if result.is_err(){break;} false},
            result=target.changed()=>{if result.is_err(){break;} false},
            result=microphone.changed()=>{if result.is_err(){break;} false},
            result=inventory.changed()=>{if result.is_err(){break;} true},
        };
        if refresh {
            let latest = inventory.borrow_and_update().clone();
            if let Ok(mut state) = screens.try_lock()
                && let Err(error) = state.maintain(latest.as_deref()).await
            {
                tracing::debug!(%error, "host screen inventory refresh failed");
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
        if let Some(hello) = audio.quality_state()
            && last_audio_quality.as_ref() != Some(&hello)
            && let Some(channel) = routes.text.as_ref().and_then(std::sync::Weak::upgrade)
        {
            let payload = Bytes::copy_from_slice(&hello);
            let sent = tokio::select! {
                _ = cancel.cancelled() => break,
                result = crate::transport::outbox::send(&channel, payload) => result,
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
        let (list, stream, media) = {
            let state = tokio::select! {
                _ = cancel.cancelled() => break,
                state = screens.lock() => state,
            };
            (state.display_list(), state.stream_state(), reports.media())
        };
        let current = list.active;
        // The revision alone changes with every publication; the list is sent
        // when what it shows changes.
        let shown = (list.displays.clone(), list.active);
        if last_screen.as_ref() != Some(&shown)
            && send(
                false,
                crate::features::stream_control::wire::push(
                    crate::features::stream_control::wire::SessionBody::Displays(list),
                ),
            )
            .await
        {
            tracing::debug!(current, "host display list published");
            last_screen = Some(shown);
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
        // Likewise the stream state, when what it says changes.
        let said = (stream.settings, stream.limits.clone(), stream.tracks.clone());
        if last_quality.as_ref() != Some(&said)
            && send(
                false,
                crate::features::stream_control::wire::push(
                    crate::features::stream_control::wire::SessionBody::Stream(stream),
                ),
            )
            .await
        {
            last_quality = Some(said);
        }
    }
}

/// Take or release the control token for this participant; a refused
/// take answers with its code and reason (the requester words it).
fn control_turn(
    lease: &crate::features::host::Lease,
    take: bool,
    automatic: bool,
) -> std::result::Result<(), (&'static str, &'static str)> {
    use crate::features::host::participants::{self, Refusal};
    use crate::features::stream_control::control_turn::refusal;
    let Some(key) = lease.participant() else {
        return Err(refusal::UNAVAILABLE);
    };
    if !take {
        participants::release_control(key);
        return Ok(());
    }
    if !lease.requested() || !lease.granted(farwing_protocol::caps::CONTROL) {
        return Err(refusal::NOT_ALLOWED);
    }
    match participants::take_control(key, automatic) {
        Ok(()) => Ok(()),
        Err(Refusal::NotAllowed) => Err(refusal::NOT_ALLOWED),
        Err(Refusal::HeldBy(_)) => Err(refusal::HELD),
        Err(Refusal::Shared) => Err(refusal::SHARED),
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
