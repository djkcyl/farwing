//! Controller `session.start` payload: what this controller decodes, its
//! display, and the stream it starts with. Purpose, type and identity come
//! from the session ticket and are stated to the host by the server.

use anyhow::{Result, bail};
use farwing_protocol::api::Purpose;
use farwing_protocol::realtime::SessionStart;
use farwing_protocol::session::{ConnectOptions, Decoder, LocalDisplay};
use uuid::Uuid;

use crate::media::ConnectionMediaProfile;


#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlPurpose {
    Viewing,
    Audio,
    PortMapping,
    FileTransfer,
    Diagnostics,
    /// The host's cameras only.
    Camera,
}

impl ControlPurpose {
    pub(crate) fn wire(self) -> Purpose {
        match self {
            Self::Viewing => Purpose::Desktop,
            Self::Audio => Purpose::Audio,
            Self::PortMapping => Purpose::Ports,
            Self::FileTransfer => Purpose::Files,
            Self::Diagnostics => Purpose::Diagnostics,
            Self::Camera => Purpose::Camera,
        }
    }
}

pub(crate) fn build_session_start(
    controller_device_id: &str,
    decoders: &[Decoder],
    profile: ConnectionMediaProfile,
    preferences: Option<crate::features::stream_control::StreamControlPreferences>,
    purpose: ControlPurpose,
) -> Result<SessionStart> {
    if !farwing_protocol::ids::is_device_id(controller_device_id) {
        bail!("cannot build a session request for an invalid device identifier");
    }
    if decoders.is_empty() {
        bail!("cannot build a session request without a usable video decoder");
    }
    Ok(SessionStart {
        attempt_id: Uuid::new_v4().to_string(),
        options: connect_options(decoders, profile, preferences),
        purpose: Some(purpose.wire()),
        visit: String::new(),
    })
}

fn connect_options(
    decoders: &[Decoder],
    profile: ConnectionMediaProfile,
    preferences: Option<crate::features::stream_control::StreamControlPreferences>,
) -> ConnectOptions {
    // The confirmed choices from the start: the host does not start at
    // defaults and switch once video runs.
    let settings = preferences.map_or_else(
        || {
            crate::features::stream_control::StreamControlPreferences::from_saved(
                crate::features::stream_control::default_settings(profile),
                profile,
            )
        },
        |p| p,
    );
    let device = settings.device.unwrap_or_default();
    let mut stream = crate::features::stream_control::Wanted {
        settings: settings.settings,
        display: profile.local_display,
        codec: profile.codec.preferred(),
        // The host draws the cursor into the picture unless this viewer
        // draws it (controlling with the local or smart pointer); the
        // policy follows the session from there.
        cursor: !device.control_enabled
            || device.mouse_mode == crate::features::remote_input::MouseMode::Remote,
        weak_network: device.weak_network,
        cinema: device.cinema,
    }
    .stream();
    // HDR needs a 10-bit decoder and an HDR display here.
    stream.hdr &= decoders.iter().any(|d| d.bit10) && crate::platform::display_supports_hdr();
    ConnectOptions {
        decoders: decoders.to_vec(),
        display: Some(LocalDisplay {
            width: profile.local_display.width,
            height: profile.local_display.height,
            scale: 0,
        }),
        stream: Some(stream),
        e2ee_only: crate::transport::nat::settings::current().e2ee_only,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::stream_control::{StreamControlPreferences, StreamQuality, default_settings};
    use crate::media::{ConnectionMediaOptions, LocalDisplayInfo};
    use farwing_protocol::session::Quality;

    #[test]
    fn new_connection_asks_for_profile_quality_or_the_saved_device_override() {
        let profile = ConnectionMediaOptions {
            quality: StreamQuality::Clear,
            ..Default::default()
        }
        .resolve(LocalDisplayInfo::FALLBACK)
        .unwrap();
        let request = connect_options(&[], profile, None);
        assert_eq!(request.stream.unwrap().quality, Quality::Standard);

        let mut saved = default_settings(profile);
        saved.quality = StreamQuality::Original;
        let preferences = StreamControlPreferences::from_saved(saved, profile);
        let request = connect_options(&[], profile, Some(preferences));
        assert_eq!(request.stream.unwrap().quality, Quality::Ultra);
    }
}
