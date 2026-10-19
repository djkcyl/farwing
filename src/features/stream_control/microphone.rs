use super::*;

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct PolicyRequest {
    #[prost(int32, tag = "1")]
    pub(super) policy: i32,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct PolicyResponse {
    #[prost(int32, tag = "1")]
    pub error_code: i32,
}

impl StreamControlHandle {
    pub(crate) fn microphone(&self) -> &crate::media::microphone::Microphone {
        &self.microphone
    }

    pub(crate) fn microphone_available(&self) -> bool {
        let state = lock(&self.shared);
        state.features.as_ref().is_some_and(|p| {
            p.is_windows() && p.supports(crate::account::feature_ability::Feature::Microphone)
        }) && self.microphone.configured()
    }

    pub(crate) fn set_microphone_enabled(&self, enabled: bool) -> Result<()> {
        let mut state = lock(&self.shared);
        if enabled {
            ensure_ready(&state)?;
            if !state.features.as_ref().is_some_and(|p| {
                p.is_windows() && p.supports(crate::account::feature_ability::Feature::Microphone)
            }) {
                bail!("对端未开放麦克风功能");
            }
            if !state.viewing_enabled {
                bail!("请先打开画面，再开启麦克风");
            }
            if !self.microphone.configured() {
                bail!("对端未协商可接收的Opus麦克风轨道");
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
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1);
        self.microphone.request(sequence, enabled);
        if !state.pb_connected || !state.text_channel_open {
            self.microphone.disconnect();
            if enabled {
                bail!("麦克风控制通道未就绪");
            }
            return Ok(());
        }
        let payload = super::wire::request(
            sequence,
            super::wire::SessionBody::MicrophonePolicy(PolicyRequest {
                policy: i32::from(enabled),
            }),
        );
        if self
            .outgoing
            .send(OutgoingControlMessage {
                annotation_generation: None,
                sequence,
                payload,
                protocol: StreamControlProtocol::CaptureSetting,
                completion: None,
            })
            .is_err()
        {
            self.microphone.send_failed(sequence, "会话已结束");
            bail!("会话已结束");
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
