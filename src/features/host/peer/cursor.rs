//! The capture owner samples the actual desktop; this task only publishes changes.
use super::{ReportRoutes, screens::Reports};
use crate::features::{
    host::lock,
    remote_cursor::{self, CursorImage},
};
use base64::Engine;
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(super) async fn run(
    reports: Arc<Reports>,
    mut route: watch::Receiver<ReportRoutes>,
    cancel: CancellationToken,
    lease: crate::features::host::Lease,
    kcp: crate::transport::fct::FctControl,
) {
    let mut timer = tokio::time::interval(Duration::from_millis(33));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut revision = None;
    let mut sent = None;
    loop {
        tokio::select! {_=cancel.cancelled()=>return,_=timer.tick()=>{},r=route.changed()=>{if r.is_err(){return}}}
        if !lease.requested() {
            return;
        }
        let routes = route.borrow_and_update().clone();
        let Some(channel) = routes.control.as_ref().and_then(std::sync::Weak::upgrade) else {
            continue;
        };
        if revision != Some(routes.revision) {
            revision = Some(routes.revision);
            sent = None;
        }
        let media = reports.media();
        let current = reports.current.load(Ordering::Acquire);
        let pointer = media
            .iter()
            .filter(|(_, m)| m.capturing && m.visible)
            .max_by_key(|(_, m)| m.screen.id == current)
            .and_then(|(index, _)| lock(&reports.pointers[*index]).clone());
        let screen = pointer.as_ref().and_then(|p| {
            lock(&reports.catalog)
                .iter()
                .find(|info| {
                    let s = &info.screen;
                    media
                        .iter()
                        .any(|(_, m)| m.capturing && m.visible && m.screen.id == s.id)
                        && i64::from(p.x) >= i64::from(s.left)
                        && i64::from(p.x) < i64::from(s.left) + i64::from(s.width)
                        && i64::from(p.y) >= i64::from(s.top)
                        && i64::from(p.y) < i64::from(s.top) + i64::from(s.height)
                })
                .map(|info| info.screen.clone())
        });
        let visible = pointer
            .as_ref()
            .is_some_and(|p| p.showing && p.image.is_some())
            && screen.is_some();
        let image = if visible {
            pointer.as_ref().and_then(|p| p.image.clone())
        } else {
            None
        };
        let key = (
            image.clone(),
            screen.as_ref().map_or(-1, |s| s.id),
            screen
                .as_ref()
                .map(|s| (s.dpi_scale, s.width, s.height, s.left, s.top)),
        );
        if sent.as_ref() == Some(&key) {
            continue;
        }
        let image = match image {
            Some(image) => match base64::engine::general_purpose::STANDARD.decode(&image.png) {
                Ok(png) => Some(CursorImage {
                    png,
                    width: image.width,
                    height: image.height,
                    hotspot: image.hotspot,
                    system_type: image.kind,
                }),
                Err(error) => {
                    tracing::warn!(%error,"invalid sampled cursor image");
                    continue;
                }
            },
            None => None,
        };
        let position = match (pointer.as_ref(), screen.as_ref()) {
            (Some(p), Some(s)) => [
                (f64::from(p.x) - f64::from(s.left)) / f64::from(s.width),
                (f64::from(p.y) - f64::from(s.top)) / f64::from(s.height),
            ],
            _ => [0.0, 0.0],
        };
        let bytes = remote_cursor::encode_shape(image.as_ref(), key.1, position);
        if bytes.len() > 512 * 1024 {
            tracing::warn!(bytes = bytes.len(), "cursor exceeds signal message limit");
            continue;
        }
        let result =
            tokio::select! {_=cancel.cancelled()=>return,r=kcp.send_control(&channel,bytes)=>r};
        match result {
            Ok(_) => {
                tracing::debug!(screen=key.1,visible,size=?image.as_ref().map(|i|(i.width,i.height)),"host cursor CONTROL state published");
                sent = Some(key);
            }
            Err(error) => tracing::debug!(%error,"host cursor CONTROL publication failed"),
        }
    }
}
