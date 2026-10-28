use super::*;

/// Turn the remote microphone on or off → `Done`, or a `PeerError` with
/// a reason of [`crate::protocol::microphone`].
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct PolicyRequest {
    #[prost(bool, tag = "1")]
    pub(crate) enabled: bool,
}

impl StreamControlHandle {
    pub(crate) fn microphone(&self) -> &crate::media::microphone::Microphone {
        &self.microphone
    }

    pub(crate) fn microphone_available(&self) -> bool {
        let state = lock(&self.shared);
        state.features.as_ref().is_some_and(|p| {
            p.supports(crate::account::feature_ability::Feature::Host)
        }) && self.microphone.configured()
    }

    pub(crate) fn set_microphone_enabled(&self, enabled: bool) -> Result<()> {
        let mut state = lock(&self.shared);
        if enabled {
            ensure_ready(&state)?;
            if !state.features.as_ref().is_some_and(|p| {
                p.supports(crate::account::feature_ability::Feature::Host)
            }) {
                bail!("{}", t().features.stream_microphone_unavailable);
            }
            if !state.viewing_enabled {
                bail!("{}", t().features.stream_viewing_off);
            }
            if !self.microphone.configured() {
                bail!("{}", t().features.stream_microphone_not_accepted);
            }
            self.microphone.start()?;
        }
        self.request_microphone_locked(&mut state, enabled)
    }

    fn request_microphone_locked(
        &self,
        state: &mut StreamControlState,
        enabled: bool,
    ) -> Result<()> {
        if !state.pb_connected || !state.text_channel_open {
            self.microphone.disconnect();
            if enabled {
                bail!("{}", t().features.stream_microphone_channel_not_ready);
            }
            return Ok(());
        }
        // The microphone waits for this request's answer.
        self.microphone.request(state.next_sequence, enabled);
        if let Err(error) = self.send_request(
            state,
            super::wire::SessionBody::MicrophonePolicy(PolicyRequest { enabled }),
            super::requests::Waiting::Microphone,
            std::time::Duration::from_secs(10),
        ) {
            self.microphone
                .answered(state.next_sequence - 1, Err(error.to_string()));
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn disable_microphone_locked(&self, state: &mut StreamControlState) {
        if self.microphone.snapshot().enabled
            || (self.microphone.needs_cleanup()
                && state.mouse_transport_connected
                && state.pb_connected
                && state.text_channel_open)
        {
            if let Err(e) = self.request_microphone_locked(state, false) {
                tracing::debug!(%e,"microphone revoked locally");
            }
        }
    }
}
