//! Resolve display capture to the exact video track advertised by the host.
use crate::features::stream_control::{RemoteScreen, StreamControlHandle};
use crate::i18n::t;
use crate::transport::rtc::{NativePeer, VideoTrackSource};
use anyhow::{Context, Result, anyhow};
use std::sync::Arc;
use std::time::Duration;

pub(super) async fn start_capture(
    control: &StreamControlHandle,
    screen: &RemoteScreen,
) -> Result<()> {
    if screen.display.is_camera() {
        control.open_camera(screen.id).await
    } else {
        control.set_screen_capture(screen.id, true).await
    }
}

pub(super) async fn wait_for_track(
    control: &StreamControlHandle,
    peer: &NativePeer,
    screen_id: i32,
    after: Option<u64>,
) -> Result<(RemoteScreen, Arc<VideoTrackSource>)> {
    let tracks = peer.video_tracks();
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let snapshot = control.snapshot();
            let screen = snapshot
                .screens
                .into_iter()
                .find(|screen| screen.id == screen_id)
                .context(t().viewer.display_disconnected)?;
            if after.is_none_or(|generation| snapshot.screens_generation > generation)
                && screen.video_track_index >= 0
                && let Some(track) = tracks.get(screen.video_track_index)
            {
                return Ok((screen, track));
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .map_err(|_| anyhow!(t().viewer.display_no_video))?
}
