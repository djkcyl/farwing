//! VirtualAudioDriverPolicyResponse errors shared by the host and viewer.
//! The wire values are mapped independently of assistance identity.
pub(crate) const COMPONENT_UNAVAILABLE: i32 = 0x0e00_3001;
pub(crate) const NOT_ALLOWED: i32 = 0x0e00_3006;
pub(crate) const INITIALIZATION_FAILED: i32 = 0x0e00_3012;

pub(crate) fn failure_message(code: i32) -> String {
    let text = &crate::i18n::t().features;
    match code {
        COMPONENT_UNAVAILABLE => text.remote_microphone_component_unavailable,
        NOT_ALLOWED => text.remote_microphone_not_allowed,
        INITIALIZATION_FAILED => text.remote_microphone_initialization_failed,
        _ => text.remote_microphone_failed,
    }
    .to_owned()
}
