//! The control token (`docs/protocol/v2-design.md` §4.2). Only one
//! participant controls the host at a time: turning keyboard and mouse
//! control on takes the token, turning it off gives it back. While another
//! participant holds it, control stays off here.
use super::*;

/// `ControlTurn` (C→H): take (`true`) or release (`false`) the token.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct TurnRequest {
    #[prost(bool, tag = "1")]
    pub take: bool,
    /// The take restores the previous control state rather than answering a
    /// user action: the host grants it only to the holder or to a
    /// participant alone in the session.
    #[prost(bool, tag = "2")]
    pub automatic: bool,
}
/// `ControlTurnResult` (H→C).
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct TurnResponse {
    /// Whether the requester holds the token now.
    #[prost(bool, tag = "1")]
    pub held: bool,
    /// Why a take was refused, for the user.
    #[prost(string, tag = "2")]
    pub message: String,
}

/// Why a take is refused while `holder` (a nickname) controls.
pub(crate) fn held_by(holder: &str) -> String {
    let holder = holder.trim();
    let holder = if holder.is_empty() { "另一位参与者" } else { holder };
    format!("{holder} 正在控制")
}

impl StreamControlHandle {
    /// Ask the host for the token or give it back. Without a session the
    /// host has nothing to arbitrate (a new session starts without a holder).
    pub(super) fn request_turn_locked(
        &self,
        state: &mut StreamControlState,
        take: bool,
        automatic: bool,
    ) {
        if !state.pb_connected || !state.text_channel_open {
            return;
        }
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1);
        state.turn_request = take.then_some((sequence, automatic));
        let payload = super::wire::request(
            sequence,
            super::wire::SessionBody::ControlTurn(TurnRequest { take, automatic }),
        );
        tracing::info!(take, automatic, sequence, "control token requested");
        let _ = self.outgoing.send(OutgoingControlMessage {
            annotation_generation: None,
            sequence,
            payload,
            protocol: StreamControlProtocol::CaptureSetting,
            completion: None,
        });
    }

    /// The host answered a take: a refusal turns control off again. An
    /// automatic take (restoring control) is refused without a notice: the
    /// user did not ask for control and turns it on by hand.
    pub(super) fn turn_response_locked(
        &self,
        state: &mut StreamControlState,
        sequence: i64,
        response: TurnResponse,
    ) {
        let Some((pending, automatic)) = state.turn_request else {
            return;
        };
        if pending != sequence {
            return;
        }
        state.turn_request = None;
        if response.held {
            return;
        }
        tracing::info!(automatic, message = %response.message, "control token refused");
        let notice = (!automatic).then(|| {
            if response.message.is_empty() {
                "被控端未允许本机控制".to_owned()
            } else {
                response.message
            }
        });
        self.leave_control_locked(state, notice);
    }

    /// A take that could not be sent: this participant does not control
    /// (no notice for an automatic take, as for a refusal).
    pub(super) fn turn_send_failed(&self, state: &mut StreamControlState, sequence: i64) -> bool {
        let Some((pending, automatic)) = state.turn_request else {
            return false;
        };
        if pending != sequence {
            return false;
        }
        state.turn_request = None;
        let notice = (!automatic).then(|| "未能向被控端请求键鼠控制权".to_owned());
        self.leave_control_locked(state, notice);
        true
    }

    /// `PARTICIPANTS` moved the token away from this participant while it
    /// was controlling (the host owner blocked it, the grant was withdrawn).
    pub(super) fn turn_lost_locked(&self, state: &mut StreamControlState) {
        if state.turn_request.is_some() {
            // Our take is still on its way; the list predates it.
            return;
        }
        self.leave_control_locked(state, Some("被控端已收回键鼠控制权".to_owned()));
    }

    fn leave_control_locked(&self, state: &mut StreamControlState, notice: Option<String>) {
        if state.mouse.mode() == MouseMode::View {
            return;
        }
        tracing::info!(?notice, "keyboard and mouse control turned off by the host");
        state.mouse.disable();
        state.restore_input_pending = false;
        state.cursor_sync_needed = true;
        if let Some(notice) = notice {
            state.remote_notice = Some((Instant::now(), notice));
        }
        self.refresh_mouse_policy(state);
        self.mouse.repaint();
    }

    /// Cinema playout follows the preference, and is always on while this
    /// participant watches the shared viewer stream.
    /// Cinema playout: chosen for this device, or a shared viewing stream.
    pub(crate) fn cinema_playout(&self) -> bool {
        let state = lock(&self.shared);
        state.device_preferences.cinema || self.participants.shared_view()
    }

    pub(super) fn apply_playout_locked(&self, state: &StreamControlState) {
        self.audio
            .set_cinema(state.device_preferences.cinema || self.participants.shared_view());
    }
}
