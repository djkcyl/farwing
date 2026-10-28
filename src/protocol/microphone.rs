//! Why the host refused the remote microphone (`PeerError` of SESSION
//! `MicrophonePolicy`): its code and reason, shared by the host and the
//! viewer.
use farwing_protocol::peer::error_code;

/// The host's virtual sound card is not there.
pub(crate) const COMPONENT_UNAVAILABLE: (&str, &str) =
    (error_code::UNAVAILABLE, "component_unavailable");
/// Not allowed in this session, or the host's user does not allow it.
pub(crate) const NOT_ALLOWED: (&str, &str) = (error_code::FORBIDDEN, "not_allowed");
/// The virtual sound card did not start.
pub(crate) const INITIALIZATION_FAILED: (&str, &str) =
    (error_code::INTERNAL, "initialization_failed");

/// A refusal in this device's language.
pub(crate) fn failure_message(reason: &str) -> String {
    let text = &crate::i18n::t().features;
    if reason == COMPONENT_UNAVAILABLE.1 {
        text.remote_microphone_component_unavailable
    } else if reason == NOT_ALLOWED.1 {
        text.remote_microphone_not_allowed
    } else if reason == INITIALIZATION_FAILED.1 {
        text.remote_microphone_initialization_failed
    } else {
        text.remote_microphone_failed
    }
    .to_owned()
}
