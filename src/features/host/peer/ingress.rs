//! One CONTROL business ingress shared by SCTP and mixed KCP.
//! Transport callbacks validate their binding, then use this same receiver.
//! Input remains synchronous; file, display and annotation keep their owners.
use super::{ReportTarget, channels, screens};
use crate::features::host::{VideoConfig, capture, lock};
use crate::transport::uu_kcp::{ControlReceiver, UuKcpControl};
use anyhow::Context;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(super) struct ControlIngress {
    pub screen: Option<capture::Screen>,
    pub config: Arc<Mutex<VideoConfig>>,
    pub negotiated: Arc<crate::features::host::format::Negotiated>,
    pub input: crate::features::host::input::Receiver,
    pub files: crate::features::host::files::Receiver,
    pub annotation: crate::features::host::annotation::Receiver,
    pub screens: Arc<tokio::sync::Mutex<screens::Screens>>,
    pub report_target: ReportTarget,
    pub kcp: UuKcpControl,
    pub cancel: CancellationToken,
}
impl ControlIngress {
    pub fn start(self) -> (ControlReceiver, tokio::task::JoinHandle<()>) {
        let Self {
            screen,
            config,
            negotiated,
            input,
            files,
            annotation,
            screens,
            report_target,
            kcp,
            cancel,
        } = self;
        enum ControlWork {
            Response { bytes: Vec<u8>, handshake: bool },
            SessionRequest(Vec<u8>),
        }
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<(u16, u64, ControlWork)>();
        struct DrawWork {
            stream: u16,
            input_generation: u64,
            annotation_generation: u64,
            text: std::sync::Weak<webrtc::data_channel::RTCDataChannel>,
            request: crate::features::stream_control::publisher::DrawRequest,
            at: std::time::Instant,
        }
        let (draw_tx, mut draw_rx) = mpsc::channel::<DrawWork>(64);
        let control_annotation = annotation.clone();
        let control_screen = screen.clone();
        let control_config = config.clone();
        let control_stop = cancel.clone();
        let control_negotiated = negotiated.clone();
        let control_target = report_target.clone();
        let control_input = input.clone();
        let control_files = files.clone();
        let control_receiver: crate::transport::uu_kcp::ControlReceiver = Arc::new(
            move |stream_id, bytes| {
                if control_stop.is_cancelled() {
                    return Ok(());
                }
                let Some(generation) = control_input.generation(stream_id) else {
                    return Ok(());
                };
                if control_input.receive(stream_id, bytes)? {
                    return Ok(());
                }
                match control_files.receive_control(bytes) {
                    Ok(true) => return Ok(()),
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(%error, "host CONTROL file request rejected");
                        return Ok(());
                    }
                }
                if bytes.len() <= 128 * 1024 {
                    if let Some(request) =
                        crate::features::stream_control::publisher::draw_request(bytes)?
                    {
                        let text = control_target
                            .borrow()
                            .text
                            .as_ref()
                            .and_then(std::sync::Weak::upgrade)
                            .context("批注回执通道尚未就绪")?;
                        if let Some(annotation_generation) = control_annotation.generation(&text) {
                            if let crate::features::stream_control::annotation::wire::PbDrawRequestKind::Toggle(toggle) = &request.command {
                                tracing::info!(enabled=toggle.enable, carrier="CONTROL", "host annotation request received");
                            }
                            draw_tx
                                .try_send(DrawWork {
                                    stream: stream_id,
                                    input_generation: generation,
                                    annotation_generation,
                                    text: Arc::downgrade(&text),
                                    request,
                                    at: std::time::Instant::now(),
                                })
                                .map_err(|_| anyhow::anyhow!("批注请求队列不可用"))?;
                        }
                        return Ok(());
                    }
                }
                if let Some(operation) =
                    crate::features::stream_control::publisher::session_request(bytes)?
                {
                    tracing::info!(operation, carrier = "CONTROL", "host session RPC received");
                    // KCP delivery is synchronous. Reuse the response worker to
                    // serialize display/settings work without blocking ACK/input
                    // delivery or spawning an unordered task for each request.
                    control_tx
                        .send((
                            stream_id,
                            generation,
                            ControlWork::SessionRequest(bytes.to_vec()),
                        ))
                        .context("host CONTROL session RPC queue closed")?;
                    return Ok(());
                }
                let responses = crate::features::stream_control::publisher::receive(
                    bytes,
                    true,
                    control_screen.as_ref(),
                    &mut lock(&control_config),
                    &control_negotiated,
                )?;
                if let Some(caps) = responses.files {
                    control_files.capabilities(caps);
                }
                control_target.send_if_modified(|routes| routes.received(&responses));
                for response in responses.messages {
                    control_tx
                        .send((
                            stream_id,
                            generation,
                            ControlWork::Response {
                                bytes: response,
                                handshake: responses.handshake_request,
                            },
                        ))
                        .context("host CONTROL response queue closed")?;
                }
                Ok(())
            },
        );
        let control_kcp = kcp.clone();
        let control_stop = cancel.clone();
        let response_input = input.clone();
        let control_screens = screens.clone();
        let control_target = report_target.clone();
        let draw_stop = cancel.clone();
        let draw_input = input.clone();
        // Separate bounded business work: starting the overlay must not delay
        // CONTROL heartbeats, display replies, or the synchronous input path.
        let draw_responses = async move {
            loop {
                let work =
                    tokio::select! { _=draw_stop.cancelled()=>break, work=draw_rx.recv()=>work };
                let Some(work) = work else {
                    break;
                };
                if work.at.elapsed() >= std::time::Duration::from_secs(2)
                    || draw_input.generation(work.stream) != Some(work.input_generation)
                {
                    continue;
                }
                let Some(text) = work.text.upgrade() else {
                    continue;
                };
                if let Err(error) = annotation
                    .receive_official(&text, work.request, work.annotation_generation)
                    .await
                {
                    tracing::warn!(%error, "host CONTROL annotation request failed");
                }
            }
        };
        let response_work = async move {
            loop {
                let item = tokio::select! { _=control_stop.cancelled()=>break, item=control_rx.recv()=>item };
                let Some((id, generation, work)) = item else {
                    break;
                };
                if response_input.generation(id) != Some(generation) {
                    continue;
                }
                let (messages, setting_response, handshake) = match work {
                    ControlWork::Response { bytes, handshake } => (vec![bytes], false, handshake),
                    ControlWork::SessionRequest(bytes) => {
                        let mut state = tokio::select! {
                            _ = control_stop.cancelled() => break,
                            state = control_screens.lock() => state,
                        };
                        if response_input.generation(id) != Some(generation) {
                            continue;
                        }
                        match crate::features::stream_control::publisher::receive_session(
                            &mut state, &bytes, false,
                        )
                        .await
                        {
                            Ok(responses) => {
                                control_target
                                    .send_if_modified(|routes| routes.received(&responses));
                                (responses.messages, true, false)
                            }
                            Err(error) => {
                                tracing::warn!(%error, "host CONTROL session RPC failed");
                                continue;
                            }
                        }
                    }
                };
                for bytes in messages {
                    if response_input.generation(id) != Some(generation)
                        || control_stop.is_cancelled()
                    {
                        break;
                    }
                    let result = if setting_response {
                        channels::send_setting_response(&control_target, bytes).await
                    } else {
                        // Mixed KCP can deliver the initial request between bind
                        // and on_open. Keep its reply until that exact channel
                        // opens; do not drop it or wait for the HID executor.
                        let mut routes = control_target.subscribe();
                        let channel = loop {
                            if response_input.generation(id) != Some(generation) {
                                break None;
                            }
                            let channel = routes
                                .borrow_and_update()
                                .control
                                .as_ref()
                                .and_then(std::sync::Weak::upgrade)
                                .filter(|channel| channel.id() == id);
                            if channel.is_some() {
                                break channel;
                            }
                            tokio::select! {
                                _ = control_stop.cancelled() => break None,
                                result = routes.changed() => if result.is_err() { break None; },
                            }
                        };
                        let Some(channel) = channel else {
                            continue;
                        };
                        // Reply via the negotiated carrier, never replay across carriers.
                        let result = control_kcp.send_control(&channel, bytes).await;
                        if result.is_ok() && handshake {
                            tracing::debug!(
                                stream_id = id,
                                "host CONTROL handshake response submitted"
                            );
                            control_target.send_if_modified(|routes| {
                                if response_input.generation(id) != Some(generation)
                                    || routes.handshake_answered
                                    || !routes.control.as_ref().is_some_and(|current| {
                                        current.ptr_eq(&Arc::downgrade(&channel))
                                    })
                                {
                                    return false;
                                }
                                routes.handshake_answered = true;
                                routes.revision = routes.revision.wrapping_add(1);
                                true
                            });
                        }
                        result
                    };
                    if let Err(error) = result {
                        tracing::warn!(%error,"host CONTROL response failed");
                    }
                }
            }
        };
        let control_responses = tokio::spawn(async move {
            tokio::join!(response_work, draw_responses);
        });
        (control_receiver, control_responses)
    }
}
