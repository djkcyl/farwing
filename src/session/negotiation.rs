//! Controller `session.start` payload: what this controller decodes, its
//! display, and the stream it starts with. Purpose, type and identity come
//! from the session ticket and are stated to the host by the server.

use anyhow::{Result, bail};
use farwing_protocol::api::Purpose;
use farwing_protocol::realtime::SessionStart;
use farwing_protocol::session::{ConnectOptions, Decoder, LocalDisplay};
use uuid::Uuid;

use crate::media::ConnectionMediaProfile;

#[derive(Clone, Copy, Debug)]
#[repr(u64)]
pub(crate) enum ControlConnectType {
    Normal = 1,
    Assistance = 2,
}


#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlPurpose {
    Viewing,
    Audio,
    PortMapping,
    FileTransfer,
}

impl ControlPurpose {
    pub(crate) fn wire(self) -> Purpose {
        match self {
            Self::Viewing => Purpose::Desktop,
            Self::Audio => Purpose::Audio,
            Self::PortMapping => Purpose::Ports,
            Self::FileTransfer => Purpose::Files,
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
        custom_bitrate_limit: settings
            .custom_bitrate_limit
            .unwrap_or(crate::features::stream_control::MAX_CUSTOM_BITRATE_MBPS),
        codec: profile.codec.preferred(),
        // The viewing policy follows once the session runs.
        cursor: false,
        weak_network: device.weak_network,
        cinema: device.cinema,
    }
    .stream();
    // HDR needs a 10-bit decoder and an HDR display here.
    stream.hdr &= decoders.iter().any(|d| d.bit10) && crate::platform::display_hdr::any_hdr();
    ConnectOptions {
        decoders: decoders.to_vec(),
        display: Some(LocalDisplay {
            width: profile.local_display.width,
            height: profile.local_display.height,
            scale: 0,
        }),
        stream: Some(stream),
    }
}
