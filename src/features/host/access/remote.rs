//! The device centre's mirror of the background host (`resident`).
use super::*;

impl Handle {
    pub(crate) fn take_remote_action(&self) -> Option<RemoteAction> {
        let mut state = lock(&self.ownership);
        state
            .remote_action
            .take()
            .or_else(|| {
                state
                    .remote_disconnects
                    .pop_front()
                    .map(RemoteAction::Disconnect)
            })
            .or_else(|| {
                state
                    .remote_blocks
                    .pop_front()
                    .map(|(session, blocked)| RemoteAction::BlockControl(session, blocked))
            })
    }
    pub(crate) async fn apply_remote(
        &self,
        snapshot: crate::platform::windows::host_service::resident::Snapshot,
    ) {
        self.media.replace(snapshot.capabilities).await;
        let mut state = lock(&self.ownership);
        if !state.active {
            return;
        }
        if state.dirty {
            // Pending local preferences must not freeze the observed live session.
            let mut status = snapshot.status;
            status.saving = state.status.saving;
            status.settings_error = state.status.settings_error.clone();
            state.status = status;
            return;
        }
        state.apply_settings(snapshot.settings);
        state.status = snapshot.status;
        self.assistance.replace(snapshot.assistance);
    }
    pub(crate) fn remote_failed(&self, error: String) {
        let mut state = lock(&self.ownership);
        let observed = state.status.connection.clone();
        clear_session(&mut state);
        // Losing the IPC observer is not evidence that remote access ended.
        state.status.connection = observed
            .filter(|c| c.elapsed_seconds.is_some())
            .map(|mut c| {
                c.observation_lost = true;
                c
            });
        state.status.ready = false;
        state.status.error = Some(error);
        state.status.message = t().features.host_background_lost.into();
    }
}
