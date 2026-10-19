//! Controller `session.start` payload: media and display preferences and the
//! decoder capability. Purpose, type and identity come from the session ticket
//! and are stated to the host by the server.



use anyhow::{Result, bail};
use farwing_protocol::api::Purpose;
use farwing_protocol::realtime::SessionStart;
use farwing_protocol::session::{
    CaptureParams, ConnectOptions, ConnectType, DecoderCapability, Resolution, VirtualMode,
};
use uuid::Uuid;

use crate::media::ConnectionMediaProfile;
use crate::protocol::capability::{CodecCapability, DeviceCapability};

#[derive(Clone, Copy, Debug)]
#[repr(u64)]
pub(crate) enum ControlConnectType {
    Normal = 1,
    Assistance = 2,
}

impl ControlConnectType {
    #[allow(dead_code)]
    pub(crate) fn wire(self) -> ConnectType {
        match self {
            Self::Normal => ConnectType::Account,
            Self::Assistance => ConnectType::Assist,
        }
    }
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
    decoder_support: &DeviceCapability,
    profile: ConnectionMediaProfile,
    connect_type: ControlConnectType,
    preferences: Option<crate::features::stream_control::StreamControlPreferences>,
    purpose: ControlPurpose,
) -> Result<SessionStart> {
    if !farwing_protocol::ids::is_device_id(controller_device_id) {
        bail!("cannot build a session request for an invalid device identifier");
    }
    let selected_capabilities = &decoder_support.video_codec_capability;
    if selected_capabilities.is_empty() {
        bail!("cannot build a session request without a usable video decoder");
    }
    let _ = connect_type;
    Ok(SessionStart {
        attempt_id: Uuid::new_v4().to_string(),
        options: connect_options(selected_capabilities, profile, preferences, purpose)?,
        capability: serde_json::to_value(decoder_support)?,
        purpose: Some(purpose.wire()),
    })
}

fn connect_options(
    selected_capabilities: &[CodecCapability],
    profile: ConnectionMediaProfile,
    preferences: Option<crate::features::stream_control::StreamControlPreferences>,
    purpose: ControlPurpose,
) -> Result<ConnectOptions> {
    // Preserve the confirmed quality in the initial request and session rebuild.
    // Do not request Auto first and correct it only after receiving video.
    let (quality, auto_quality, bitrate) = preferences
        .map(|p| p.initial_capture_quality())
        .transpose()?
        .unwrap_or((5, 2, 0));
    let capture = CaptureParams {
        fps: match profile.stream_fps {
            30 => 1,
            60 => 2,
            90 => 3,
            _ => 4,
        },
        quality: quality as i32,
        // The initial request leaves cursor capture off. Once the peer protocol
        // and screen state are ready, stream control sends the viewing policy.
        cursor_capture: false,
        // Follow the remote resolution.
        resolution_type: 3,
        // The controller's physical display size, not a resize request. The
        // host falls back to 1920x1080 for display-layout initialization when
        // it is absent, so it is always included.
        local_resolution: Some(Resolution {
            width: profile.local_display.width as i32,
            height: profile.local_display.height as i32,
        }),
        chosen_resolution: None,
        chroma: if preferences.is_some_and(|p| p.settings.true_color) {
            3
        } else {
            1
        },
        bitrate: bitrate as i32,
        hdr: preferences.is_some_and(|p| p.settings.hdr)
            && selected_capabilities
                .iter()
                .any(|cap| cap.bit_depth >= 10 && cap.width > 0 && cap.height > 0),
        auto_quality: auto_quality as i32,
        fps_count: profile.local_display.refresh_hz.min(profile.stream_fps) as i32,
        // Per-device choices follow in the first CaptureSetting.
        weak_network: 0,
        cinema: false,
        codec: 0,
    };
    let mut decoders = Vec::new();
    let mut profiles = Vec::new();
    for capability in selected_capabilities {
        let key = (capability.video_codec, capability.chroma_sampling);
        if !profiles.contains(&key) {
            profiles.push(key);
        }
    }
    for (codec, chroma) in profiles {
        let (width, height) = selected_capabilities
            .iter()
            .filter(|cap| cap.video_codec == codec && cap.chroma_sampling == chroma)
            .map(|cap| (cap.width, cap.height))
            .max()
            .unwrap_or_default();
        decoders.push(DecoderCapability {
            // Receiver-side FPS capability derived from the local display tier.
            // The requested stream FPS remains in the capture parameters.
            fps: profile.decoder_fps_cap as i32,
            codec,
            width,
            height,
            chroma: i32::from(chroma),
        });
    }
    let virtual_modes = if purpose == ControlPurpose::Viewing {
        crate::media::local_display_dimensions()
            .into_iter()
            .map(|(width, height)| VirtualMode {
                width: width as i32,
                height: height as i32,
                vsync: 60,
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(ConnectOptions {
        screen_id: -1,
        capture: Some(capture),
        decoders,
        force_virtual: false,
        virtual_modes,
        virtual_initial: None,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    })
}
