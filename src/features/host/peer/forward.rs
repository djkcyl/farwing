//! Tells this peer's controller where its picture comes from (the
//! forwarder or the host directly), on the session channel.
use super::{ReportRoutes, channels::send_business};
use crate::features::host::forwarding::Registration;
use crate::features::stream_control::wire::{self, SessionBody};
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(super) async fn run(
    registration: Arc<Registration>,
    mut route: watch::Receiver<ReportRoutes>,
    cancel: CancellationToken,
    kcp: crate::transport::fct::FctControl,
) {
    let mut instructions = registration.instructions();
    let mut revision = None;
    let mut sent = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            r = route.changed() => if r.is_err() { return },
            r = instructions.changed() => if r.is_err() { return },
        }
        let routes = route.borrow_and_update().clone();
        if !routes.handshake_complete || !routes.handshake_answered {
            continue;
        }
        if revision != Some(routes.revision) {
            revision = Some(routes.revision);
            sent = None;
        }
        let Some(current) = instructions.borrow_and_update().clone() else {
            continue;
        };
        if sent.as_ref() == Some(&current) {
            continue;
        }
        let Some(channel) = routes.text.as_ref().and_then(std::sync::Weak::upgrade) else {
            continue;
        };
        let frame = wire::push(SessionBody::Forward(current.clone()));
        let result = tokio::select! {
            _ = cancel.cancelled() => return,
            r = send_business(&channel, false, &kcp, frame) => r,
        };
        match result {
            Ok(_) => {
                tracing::info!(room = %current.room, generation = current.generation,
                    tracks = current.tracks.len(), "forwarding instruction sent");
                sent = Some(current);
            }
            Err(error) => tracing::debug!(%error, "forwarding instruction not sent"),
        }
    }
}
