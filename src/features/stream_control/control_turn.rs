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
    /// Without the `control` grant, ask the host's user for control in
    /// this session only.
    #[prost(bool, tag = "3")]
    pub ask: bool,
}
/// Why the host refused a take (`PeerError` code and reason).
pub(crate) mod refusal {
    use farwing_protocol::peer::error_code;
    /// This participant cannot control in this session.
    pub const UNAVAILABLE: (&str, &str) = (error_code::UNAVAILABLE, "unavailable");
    /// It was not granted control.
    pub const NOT_ALLOWED: (&str, &str) = (error_code::FORBIDDEN, "not_allowed");
    /// Another participant controls.
    pub const HELD: (&str, &str) = (error_code::BUSY, "held");
    /// Shared view: control is not taken automatically.
    pub const SHARED: (&str, &str) = (error_code::BUSY, "shared");
    /// The host's user declined a request for control (or nobody answered).
    pub const DECLINED: (&str, &str) = (error_code::FORBIDDEN, "declined");
}

/// Why a take is refused while `holder` (a nickname) controls.
pub(crate) fn held_by(holder: &str) -> String {
    let holder = holder.trim();
    let holder = if holder.is_empty() {
        t().features.stream_another_participant
    } else {
        holder
    };
    (t().features.stream_held_by)(holder)
}

impl StreamControlHandle {
    /// Ask the host for the token or give it back. Without a session the
    /// host has nothing to arbitrate (a new session starts without a holder).
    pub(super) fn request_turn_locked(
        &self,
        state: &mut StreamControlState,
        take: bool,
        automatic: bool,
        ask: bool,
    ) {
        if !state.hello_answered || !state.text_channel_open {
            return;
        }
        // Asking the host's user waits for an answer from a person.
        let timeout = std::time::Duration::from_secs(if ask { 75 } else { 15 });
        match self.send_request(
            state,
            super::wire::SessionBody::ControlTurn(TurnRequest { take, automatic, ask }),
            super::requests::Waiting::Turn { take },
            timeout,
        ) {
            Ok(sequence) => {
                if take {
                    state.turn_request = Some((sequence, automatic, ask));
                }
                tracing::info!(take, automatic, ask, sequence, "control token requested");
            }
            Err(error) => tracing::debug!(%error, "control token request not sent"),
        }
    }

    /// The host answered a take: a refusal turns control off again. An
    /// automatic take (restoring control) is refused without a notice: the
    /// user did not ask for control and turns it on by hand.
    pub(super) fn turn_settled(
        &self,
        state: &mut StreamControlState,
        sequence: i64,
        result: std::result::Result<(), super::requests::Refused>,
    ) {
        let Some((pending, automatic, ask)) = state.turn_request else {
            return;
        };
        if pending != sequence {
            return;
        }
        state.turn_request = None;
        let Err(refused) = result else {
            return;
        };
        tracing::info!(automatic, ask, reason = %refused.reason, "control token refused");
        let notice = (!automatic).then(|| refused.text.clone());
        // Without the grant, the notice offers to ask the host's user.
        let offer = !automatic && !ask && refused.reason == refusal::NOT_ALLOWED.1;
        self.leave_control_locked(state, notice, offer);
    }

    /// A refused take in this device's language.
    pub(super) fn turn_refusal_text(&self, reason: &str) -> String {
        let text = &t().features;
        match reason {
            r if r == refusal::UNAVAILABLE.1 => text.peer_control_not_available.to_owned(),
            r if r == refusal::NOT_ALLOWED.1 => text.peer_no_control_permission.to_owned(),
            r if r == refusal::HELD.1 => held_by(
                &self
                    .participants
                    .controlled_by_other()
                    .map(|m| m.nickname)
                    .unwrap_or_default(),
            ),
            r if r == refusal::SHARED.1 => text.peer_control_not_automatic_shared.to_owned(),
            r if r == refusal::DECLINED.1 => text.peer_control_declined.to_owned(),
            _ => text.stream_control_refused.to_owned(),
        }
    }

    /// `PARTICIPANTS` moved the token away from this participant while it
    /// was controlling (the host owner blocked it, the grant was withdrawn).
    pub(super) fn turn_lost_locked(&self, state: &mut StreamControlState) {
        if state.turn_request.is_some() {
            // Our take is still on its way; the list predates it.
            return;
        }
        self.leave_control_locked(
            state,
            Some(t().features.stream_control_withdrawn.to_owned()),
            false,
        );
    }

    fn leave_control_locked(&self, state: &mut StreamControlState, notice: Option<String>, offer: bool) {
        if state.mouse.mode() == MouseMode::View {
            return;
        }
        tracing::info!(?notice, "keyboard and mouse control turned off by the host");
        state.mouse.disable();
        state.restore_input_pending = false;
        state.cursor_sync_needed = true;
        if let Some(notice) = notice {
            state.notify_remote_with(notice, offer);
        }
        self.refresh_mouse_policy(state);
        self.mouse.repaint();
    }

    /// Cinema playout follows the preference, and is always on while this
    /// participant watches the shared viewer stream.
    /// Cinema playout: chosen for this device, or a shared viewing stream.
    pub(crate) fn cinema_playout(&self) -> bool {
        let state = lock(&self.shared);
        self.playout(&state)
    }

    pub(super) fn apply_playout_locked(&self, state: &StreamControlState) {
        self.audio.set_cinema(self.playout(state));
    }

    /// `connect --dev-cinema` decides alone (measurements).
    fn playout(&self, state: &StreamControlState) -> bool {
        super::dev_cinema()
            .unwrap_or_else(|| state.device_preferences.cinema || self.participants.shared_view())
    }
}
