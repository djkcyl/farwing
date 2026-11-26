//! Linux uses the shared track mapping and one picture per visible viewport.
#[path = "screen_tracks.rs"]
mod screen_tracks;

use super::VideoPicture;
use crate::features::stream_control::{RemoteScreen, StreamControlHandle};
use crate::i18n::t;
use crate::media::ConnectionMediaProfile;
use crate::transport::rtc::NativePeer;
use anyhow::{Context, Result, bail, ensure};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

type VisibleScreens = HashMap<i32, CancellationToken>;
const IDLE_CAPTURE: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(crate) struct ScreenStreams {
    peer: Weak<NativePeer>,
    control: StreamControlHandle,
    profile: ConnectionMediaProfile,
    visible: watch::Sender<VisibleScreens>,
    capture: Arc<Mutex<HashSet<i32>>>,
}

impl ScreenStreams {
    pub(crate) fn new(peer: &Arc<NativePeer>, profile: ConnectionMediaProfile) -> Self {
        let control = peer.stream_control_handle();
        let (visible, receiver) = watch::channel(VisibleScreens::new());
        let capture = Arc::new(Mutex::new(HashSet::new()));
        peer.spawn_viewing_task(stop_idle(control.clone(), receiver, Arc::clone(&capture)));
        Self {
            peer: Arc::downgrade(peer),
            control,
            profile,
            visible,
            capture,
        }
    }

    pub(crate) fn control(&self) -> &StreamControlHandle {
        &self.control
    }

    pub(crate) fn screen(&self, screen_id: i32) -> Option<RemoteScreen> {
        self.control
            .snapshot()
            .screens
            .into_iter()
            .find(|screen| screen.id == screen_id)
    }

    pub(crate) async fn wait_primary(&self) -> Result<RemoteScreen> {
        tokio::time::timeout(Duration::from_secs(10), async {
            self.control.wait_media_control().await?;
            let changed = self.control.protocol_notifications();
            loop {
                let notified = changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if let Some(screen) = self
                    .control
                    .snapshot()
                    .screens
                    .into_iter()
                    .filter(|screen| screen.id >= 0 && !screen.display.is_camera())
                    .min_by_key(|screen| !screen.primary)
                {
                    return Ok(screen);
                }
                notified.await;
            }
        })
        .await
        .context(t().session.error_no_display_list)?
    }

    /// Include pending opens and still-presented pictures. Removing an ID also
    /// cancels its pending work, even if that same ID is opened again later.
    pub(crate) fn set_visible(&self, screens: Vec<i32>) {
        self.visible.send_if_modified(|visible| {
            let old_len = visible.len();
            visible.retain(|id, token| {
                let keep = screens.contains(id);
                if !keep {
                    token.cancel();
                }
                keep
            });
            let retained = visible.len();
            for id in screens.into_iter().filter(|id| *id >= 0) {
                visible.entry(id).or_default();
            }
            retained != old_len || visible.len() != retained
        });
    }

    pub(crate) fn focus(&self, track_index: i32) {
        if track_index >= 0
            && let Some(peer) = self.peer.upgrade()
            && self.control.snapshot().screens.iter().any(|screen| {
                screen.video_track_index == track_index
                    && self.visible.borrow().contains_key(&screen.id)
            })
            && peer.video_tracks().get(track_index).is_some()
        {
            peer.select_viewed_video_track(track_index);
        }
    }

    pub(crate) async fn open(
        &self,
        ctx: egui::Context,
        viewport: egui::ViewportId,
        screen_id: i32,
    ) -> Result<VideoPicture> {
        let visible = self
            .visible
            .borrow()
            .get(&screen_id)
            .cloned()
            .context(t().viewer.screen_load_cancelled)?;
        tokio::select! {
            biased;
            _ = visible.cancelled() => bail!("{}", t().viewer.screen_load_cancelled),
            result = self.open_visible(ctx, viewport, screen_id) => result,
        }
    }

    async fn open_visible(
        &self,
        ctx: egui::Context,
        viewport: egui::ViewportId,
        screen_id: i32,
    ) -> Result<VideoPicture> {
        let peer = self
            .peer
            .upgrade()
            .context(t().viewer.viewer_connection_closed)?;
        let after = {
            // A concurrent stop must settle before inspecting the mapping. Its
            // answer applies the new catalog before the RPC returns.
            let mut uncertain_stops = self.capture.lock().await;
            let snapshot = self.control.snapshot();
            let screen = snapshot
                .screens
                .iter()
                .find(|screen| screen.id == screen_id)
                .context(t().viewer.display_disconnected)?;
            if screen.video_track_index < 0 || uncertain_stops.contains(&screen_id) {
                screen_tracks::start_capture(&self.control, screen).await?;
                uncertain_stops.remove(&screen_id);
                Some(snapshot.screens_generation)
            } else {
                None
            }
        };
        let (screen, track) =
            screen_tracks::wait_for_track(&self.control, &peer, screen_id, after).await?;
        let picture = VideoPicture::start(ctx, viewport, self.profile, &track, &screen)?;
        track.add_sink(picture.sink()).await;
        self.check_binding(&screen)?;
        track.start();
        peer.request_keyframe(track.metadata.ssrc).await?;
        self.check_binding(&screen)?;
        Ok(picture)
    }

    fn check_binding(&self, screen: &RemoteScreen) -> Result<()> {
        ensure!(
            self.screen(screen.id).is_some_and(|current| {
                current.binding() == screen.binding()
                    && current.video_track_index == screen.video_track_index
            }),
            "{}",
            t().viewer.display_mapping_changed
        );
        Ok(())
    }
}

async fn stop_idle(
    control: StreamControlHandle,
    mut visible: watch::Receiver<VisibleScreens>,
    capture: Arc<Mutex<HashSet<i32>>>,
) {
    let mut inactive = HashMap::<i32, Instant>::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            changed = visible.changed() => if changed.is_err() { break },
            _ = tick.tick() => {},
        }
        let active = control
            .snapshot()
            .screens
            .into_iter()
            .filter(|screen| screen.id >= 0 && screen.video_track_index >= 0)
            .collect::<Vec<_>>();
        inactive.retain(|id, _| {
            !visible.borrow().contains_key(id) && active.iter().any(|screen| screen.id == *id)
        });
        for screen in active {
            if visible.borrow().contains_key(&screen.id)
                || inactive
                    .entry(screen.id)
                    .or_insert_with(Instant::now)
                    .elapsed()
                    < IDLE_CAPTURE
            {
                continue;
            }
            let mut uncertain_stops = capture.lock().await;
            if visible.borrow().contains_key(&screen.id)
                || !control
                    .snapshot()
                    .screens
                    .iter()
                    .any(|current| current.id == screen.id && current.video_track_index >= 0)
            {
                continue;
            }
            // Close just this capture; the other viewports keep playing.
            match control.set_screen_capture(screen.id, false).await {
                Ok(()) => {
                    uncertain_stops.remove(&screen.id);
                }
                Err(error) => {
                    // A lost answer can leave an old mapping locally even if
                    // the host stopped. The next open must explicitly start.
                    uncertain_stops.insert(screen.id);
                    tracing::warn!(screen_id = screen.id, %error, "stop inactive screen capture failed");
                }
            }
            inactive.remove(&screen.id);
        }
    }
}
