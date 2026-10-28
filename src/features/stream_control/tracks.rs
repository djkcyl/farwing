//! Capture activation: `Capture` requests and their answers.
use super::requests::Waiting;
use super::wire::{CaptureControl, SessionBody};
use super::{StreamControlHandle, ensure_business_ready, feature_supported, lock};
use crate::i18n::t;
use anyhow::{Result, anyhow, bail};

/// How long the host may take to start a capture.
const CAPTURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

impl StreamControlHandle {
    pub(crate) async fn set_video_enabled(&self, enabled: bool) -> Result<()> {
        if !enabled {
            // Control goes back to the host too: another participant may
            // take it while this one only listens.
            let _ = self.suspend_mouse_control();
            self.set_viewing_enabled(false);
        } else {
            self.set_viewing_enabled(true);
        }
        {
            let state = lock(&self.shared);
            anyhow::ensure!(
                state.pb_connected && state.text_channel_open,
                "{}",
                t().features.stream_media_control_not_ready
            );
        }
        self.capture(-1, enabled).await?;
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
        {
            let state = lock(&self.shared);
            if active
                && screen_id != state.current_screen_id
                && !feature_supported(&state, crate::account::feature_ability::Feature::Host)
            {
                bail!("{}", t().features.stream_multi_screen_unavailable);
            }
            if screen_id < 0 || !state.screens.iter().any(|screen| screen.id == screen_id) {
                bail!("{}", t().features.stream_display_unavailable);
            }
        }
        self.capture(screen_id, active).await
    }
    /// Start or stop capturing `screen_id` (-1: every screen) and wait for
    /// the host's answer; its display list is in effect when this returns.
    async fn capture(&self, screen_id: i32, start: bool) -> Result<()> {
        let (answer, answered) = tokio::sync::oneshot::channel();
        {
            let mut state = lock(&self.shared);
            ensure_business_ready(&state)?;
            self.send_request(
                &mut state,
                SessionBody::Capture(CaptureControl {
                    screen_id,
                    start,
                    track: None,
                }),
                Waiting::Capture(answer),
                CAPTURE_TIMEOUT,
            )?;
        }
        answered
            .await
            .map_err(|_| anyhow!("{}", t().features.stream_viewing_connection_closed))?
            .map_err(anyhow::Error::msg)
    }
}
