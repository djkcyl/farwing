//! One CONTROL ingress shared by SCTP and mixed KCP: input, file control and
//! the peer `HELLO`. Transport callbacks validate their binding, then use this
//! same receiver. Input remains synchronous.
use super::ReportTarget;
use crate::transport::fct::{ControlReceiver, FctControl};
use anyhow::Context;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(super) struct ControlIngress {
    pub input: crate::features::host::input::Receiver,
    /// `VideoFeedback` goes to the video track it names.
    pub recovery: Arc<crate::features::host::recovery::Inbox>,
    /// `ForwardStatus` goes to multi-viewer forwarding.
    pub forwarding: Option<Arc<crate::features::host::forwarding::Registration>>,
    /// Receives this participant's `POINTER` frames.
    pub membership: Option<Arc<crate::features::host::participants::Membership>>,
    pub files: crate::features::host::files::Receiver,
    pub report_target: ReportTarget,
    pub kcp: FctControl,
    pub cancel: CancellationToken,
}
impl ControlIngress {
    pub fn start(self) -> (ControlReceiver, tokio::task::JoinHandle<()>) {
        let Self {
            input,
            recovery,
            forwarding,
            membership,
            files,
            report_target,
            kcp,
            cancel,
        } = self;
        struct ControlWork {
            bytes: Vec<u8>,
            handshake: bool,
        }
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<(u16, u64, ControlWork)>();
        let control_stop = cancel.clone();
        let control_target = report_target.clone();
        let control_input = input.clone();
        let control_files = files.clone();
        let control_receiver: crate::transport::fct::ControlReceiver = Arc::new(
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
                if let Some(body) = crate::protocol::peer::body_of(
                    crate::protocol::peer::FrameType::Pointer,
                    bytes,
                ) {
                    match (
                        &membership,
                        crate::protocol::participants::decode_pointer(body),
                    ) {
                        (Some(membership), Ok(pointer)) => membership.pointer(pointer),
                        (_, Err(error)) => tracing::debug!(%error, "invalid participant pointer"),
                        (None, Ok(_)) => {}
                    }
                    return Ok(());
                }
                if let Some(body) = crate::protocol::peer::body_of(
                    crate::protocol::peer::FrameType::Session,
                    bytes,
                ) {
                    use crate::features::stream_control::wire::{SessionBody, SessionMessage};
                    match <SessionMessage as prost::Message>::decode(body) {
                        Ok(SessionMessage { body: Some(SessionBody::VideoFeedback(feedback)) }) => {
                            recovery.deliver(&feedback);
                            return Ok(());
                        }
                        Ok(SessionMessage { body: Some(SessionBody::ForwardStatus(status)) }) => {
                            if let Some(forwarding) = &forwarding {
                                forwarding.status(&status);
                            }
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                // A participant may ask for its own cursor colour (again
                // later in the session to change it; 0 = the given colour).
                if let Some(membership) = &membership
                    && let Ok(Some(hello)) = crate::protocol::peer::decode_hello(bytes)
                    && let Some(color) = hello.hello.profile.map(|p| p.color)
                {
                    membership.set_color(color);
                }
                let responses = crate::features::stream_control::publisher::receive(bytes)?;
                if let Some(caps) = responses.files {
                    control_files.capabilities(caps);
                }
                control_target.send_if_modified(|routes| routes.received(&responses));
                for response in responses.messages {
                    control_tx
                        .send((
                            stream_id,
                            generation,
                            ControlWork {
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
        let control_target = report_target.clone();
        let response_work = async move {
            loop {
                let item = tokio::select! { _=control_stop.cancelled()=>break, item=control_rx.recv()=>item };
                let Some((id, generation, work)) = item else {
                    break;
                };
                if response_input.generation(id) != Some(generation) {
                    continue;
                }
                let ControlWork { bytes, handshake } = work;
                let messages = vec![bytes];
                for bytes in messages {
                    if response_input.generation(id) != Some(generation)
                        || control_stop.is_cancelled()
                    {
                        break;
                    }
                    let result = {
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
        let control_responses = tokio::spawn(response_work);
        (control_receiver, control_responses)
    }
}
