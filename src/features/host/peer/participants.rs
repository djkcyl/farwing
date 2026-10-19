//! Publishes the session's participants to this peer: `PARTICIPANTS` on the
//! session channel when the list or control token changes, and the other
//! participants' pointers in `CURSOR` on the control channel.
use super::{ReportRoutes, channels::send_business};
use crate::features::host::participants::{self, Membership};
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Pointer updates are sent at most this often.
const POINTER_INTERVAL: Duration = Duration::from_millis(30);

pub(super) async fn run(
    membership: Arc<Membership>,
    mut route: watch::Receiver<ReportRoutes>,
    cancel: CancellationToken,
    kcp: crate::transport::fct::FctControl,
) {
    let (mut members, mut pointers) = participants::subscribe();
    let mut timer = tokio::time::interval(POINTER_INTERVAL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut revision = None;
    let mut sent_list = None::<Vec<u8>>;
    let mut sent_pointers = None::<u64>;
    // Events that happened before this peer joined are not its business.
    let mut sent_event =
        participants::host_event_after(0).map_or(0, |(sequence, _, _)| sequence);
    let mut pointer_pending = false;
    let nobody = crate::features::remote_cursor::encode_pointers(Vec::new());
    let mut sent_nobody = false;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            r = route.changed() => if r.is_err() { return },
            r = members.changed() => if r.is_err() { return },
            r = pointers.changed() => {
                if r.is_err() { return }
                // Coalesce bursts of pointer frames into the next tick.
                pointer_pending = true;
                continue;
            },
            _ = timer.tick() => {},
        }
        let routes = route.borrow_and_update().clone();
        if !routes.handshake_complete || !routes.handshake_answered {
            continue;
        }
        if revision != Some(routes.revision) {
            revision = Some(routes.revision);
            sent_list = None;
            sent_pointers = None;
            sent_nobody = false;
        }
        members.borrow_and_update();
        // A capability change may have cost the holder its token.
        participants::validate_holder();
        if let Some((sequence, _, except)) = participants::host_event_after(sent_event)
            && except == Some(membership.key())
        {
            // This participant caused the event.
            sent_event = sequence;
        }
        if let Some((sequence, frame, _)) = participants::host_event_after(sent_event)
            && let Some(channel) = routes.text.as_ref().and_then(std::sync::Weak::upgrade)
        {
            let result = tokio::select! {
                _ = cancel.cancelled() => return,
                r = send_business(&channel, false, &kcp, frame) => r,
            };
            match result {
                Ok(_) => sent_event = sequence,
                Err(error) => tracing::debug!(%error, "host event publication failed"),
            }
        }
        let list = membership.participants_frame();
        if sent_list.as_ref() != Some(&list)
            && let Some(channel) = routes.text.as_ref().and_then(std::sync::Weak::upgrade)
        {
            let result = tokio::select! {
                _ = cancel.cancelled() => return,
                r = send_business(&channel, false, &kcp, list.clone()) => r,
            };
            match result {
                Ok(_) => sent_list = Some(list),
                Err(error) => tracing::debug!(%error, "host participants publication failed"),
            }
        }
        let current = *pointers.borrow_and_update();
        if !pointer_pending && sent_pointers == Some(current) {
            continue;
        }
        let Some(channel) = routes.control.as_ref().and_then(std::sync::Weak::upgrade) else {
            continue;
        };
        let frame = membership.pointers_frame();
        // Alone in the session: one empty update clears stale pointers.
        let empty = frame == nobody;
        if empty && sent_nobody {
            sent_pointers = Some(current);
            pointer_pending = false;
            continue;
        }
        let result = tokio::select! {
            _ = cancel.cancelled() => return,
            r = send_business(&channel, true, &kcp, frame) => r,
        };
        match result {
            Ok(_) => {
                sent_pointers = Some(current);
                pointer_pending = false;
                sent_nobody = empty;
            }
            Err(error) => tracing::debug!(%error, "host participant pointers publication failed"),
        }
    }
}
