//! Capture activation and negotiated video-track registration.
use super::wire::{CaptureControl, PbSendVideoTrackRequest, SessionBody};
use super::{
    OutgoingControlMessage, StreamControlHandle, StreamControlState, ensure_business_ready,
    feature_supported, lock, protocol,
};
use crate::i18n::t;
use anyhow::{Result, anyhow, bail};

impl StreamControlHandle {
    pub(crate) async fn set_video_enabled(&self, enabled: bool) -> Result<()> {
        if !enabled {
            self.mouse.disable();
            self.set_viewing_enabled(false);
        }
        if enabled {
            self.set_viewing_enabled(true);
            self.ensure_video_tracks_registered().await?;
        }
        let (complete, done) = tokio::sync::oneshot::channel();
        {
            let mut state = lock(&self.shared);
            anyhow::ensure!(
                state.pb_connected && state.text_channel_open,
                "{}",
                t().features.stream_media_control_not_ready
            );
            let sequence = state.next_sequence;
            state.next_sequence += 1;
            self.outgoing
                .send(OutgoingControlMessage {
                    annotation_generation: None,
                    sequence,
                    payload: super::wire::push(SessionBody::Capture(CaptureControl {
                        screen_id: -1,
                        start: enabled,
                    })),
                    protocol: protocol(&state),
                    completion: Some(complete),
                })
                .map_err(|_| anyhow!("{}", t().features.stream_media_connection_closed))?;
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), done)
            .await
            .map_err(|_| anyhow!("{}", t().features.stream_video_switch_timeout))?
            .map_err(|_| anyhow!("{}", t().features.stream_media_connection_closed))?
            .map_err(anyhow::Error::msg)?;
        if enabled {
            self.set_viewing_enabled(true);
        }
        Ok(())
    }
    pub(crate) async fn wait_media_control(&self) -> Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                {
                    let state = lock(&self.shared);
                    if state.pb_connected && state.text_channel_open {
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .map_err(|_| anyhow!("{}", t().features.stream_media_control_handshake_timeout))
    }
    /// Ordinary desktop capture only. Negative IDs include all-screen actions
    /// and are never accepted from a single monitor/window operation.
    pub async fn set_screen_capture(&self, screen_id: i32, active: bool) -> Result<()> {
        if active {
            let state = lock(&self.shared);
            if screen_id != state.current_screen_id
                && !feature_supported(
                    &state,
                    crate::account::feature_ability::Feature::MultiScreen,
                )
            {
                bail!("{}", t().features.stream_multi_screen_unavailable);
            }
        }
        if screen_id < 0
            || !self
                .snapshot()
                .screens
                .iter()
                .any(|screen| screen.id == screen_id)
        {
            bail!("{}", t().features.stream_display_unavailable);
        }
        if active {
            self.ensure_video_tracks_registered().await?;
        }
        let (complete, done) = tokio::sync::oneshot::channel();
        {
            let mut state = lock(&self.shared);
            ensure_business_ready(&state)?;
            if screen_id < 0 || !state.screens.iter().any(|screen| screen.id == screen_id) {
                bail!("{}", t().features.stream_display_unavailable);
            }
            let sequence = state.next_sequence;
            state.next_sequence += 1;
            let payload = super::wire::push(SessionBody::Capture(CaptureControl {
                screen_id,
                start: active,
            }));
            self.outgoing
                .send(OutgoingControlMessage {
                    annotation_generation: None,
                    sequence,
                    payload,
                    protocol: protocol(&state),
                    completion: Some(complete),
                })
                .map_err(|_| anyhow!("{}", t().features.stream_viewing_connection_closed))?;
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), done)
            .await
            .map_err(|_| anyhow!("{}", t().features.stream_capture_request_timeout))?
            .map_err(|_| anyhow!("{}", t().features.stream_viewing_connection_closed))?
            .map_err(anyhow::Error::msg)
    }

    pub(crate) fn set_available_video_tracks(&self, mut tracks: Vec<i32>) {
        tracks.retain(|index| *index >= 0);
        tracks.sort_unstable();
        tracks.dedup();
        let mut state = lock(&self.shared);
        if state.available_video_tracks != tracks {
            tracing::info!(
                ?tracks,
                "negotiated remote video tracks available for capture registration"
            );
            state.available_video_tracks = tracks;
            state.track_registration_error = None;
        }
        self.maybe_register_video_tracks(&mut state);
    }

    pub(super) async fn ensure_video_tracks_registered(&self) -> Result<()> {
        {
            let mut state = lock(&self.shared);
            anyhow::ensure!(
                state.pb_connected && state.text_channel_open,
                "{}",
                t().features.stream_media_control_not_ready
            );
            if state.available_video_tracks.is_empty() {
                bail!("{}", t().features.stream_no_tracks_offered);
            }
            state.track_registration_error = None;
            self.maybe_register_video_tracks(&mut state);
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                {
                    let state = lock(&self.shared);
                    if let Some(error) = &state.track_registration_error {
                        bail!("{error}");
                    }
                    if state.available_video_tracks == state.registered_video_tracks {
                        return Ok(());
                    }
                    if !state.pb_connected {
                        bail!("{}", t().features.stream_viewing_connection_lost);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| {
            let mut state = lock(&self.shared);
            state.track_registration = None;
            state.track_registration_error =
                Some(t().features.stream_track_registration_unconfirmed.into());
            anyhow!("{}", t().features.stream_track_registration_unconfirmed)
        })?
    }

    pub(super) fn maybe_register_video_tracks(&self, state: &mut StreamControlState) {
        if !state.viewing_enabled
            || !state.pb_connected
            || !state.text_channel_open
            || state.available_video_tracks.is_empty()
            || state.available_video_tracks == state.registered_video_tracks
            || state.track_registration.is_some()
            || state.track_registration_error.is_some()
        {
            return;
        }
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1);
        let tracks = state.available_video_tracks.clone();
        let payload = super::wire::request(
            sequence,
            SessionBody::VideoTracks(PbSendVideoTrackRequest {
                video_track_index: tracks.clone(),
            }),
        );
        if self
            .outgoing
            .send(OutgoingControlMessage {
                annotation_generation: None,
                sequence,
                payload,
                protocol: protocol(state),
                completion: None,
            })
            .is_ok()
        {
            state.track_registration = Some((sequence, tracks));
        } else {
            state.track_registration_error =
                Some(t().features.stream_track_registration_unsent.into());
        }
    }
}
