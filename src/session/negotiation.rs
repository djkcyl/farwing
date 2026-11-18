//! Verified controller handshake payloads.
//!
//! `ConnectOptions` is carried as a Socket.IO binary attachment. It must not be
//! converted to UTF-8 or base64: the official client sends the protobuf bytes
//! unchanged after a binary-event placeholder.

use anyhow::{Result, bail};
use serde_json::json;
use uuid::Uuid;

use crate::account::api::PROTOCOL_VERSION;
use crate::media::ConnectionMediaProfile;
use crate::protocol::capability::{CodecCapability, DeviceCapability};

// Streamer decoder implementation identifiers observed in the official client:
// Local implementations emit 32 (DXVA11) or 37 (software).  The adapter identifier is part of the
// capability contract and can affect the encoder's selected frame-rate/format;
// it must describe the local native adapter instead of being forced to 37.

pub(crate) struct ControlFrames {
    pub header: String,
    pub attachment: Vec<u8>,
    pub app_control_id: String,
}

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
    Terminal,
}

pub(crate) fn build_control_frames(
    controller_device_id: &str,
    ack_id: u64,
    decoder_support: &DeviceCapability,
    profile: ConnectionMediaProfile,
    connect_type: ControlConnectType,
    preferences: Option<crate::features::stream_control::StreamControlPreferences>,
    purpose: ControlPurpose,
) -> Result<ControlFrames> {
    build_control_frames_with_id(
        controller_device_id,
        ack_id,
        &Uuid::new_v4().to_string(),
        decoder_support,
        profile,
        connect_type,
        preferences,
        purpose,
    )
}

fn build_control_frames_with_id(
    controller_device_id: &str,
    ack_id: u64,
    app_control_id: &str,
    decoder_support: &DeviceCapability,
    profile: ConnectionMediaProfile,
    connect_type: ControlConnectType,
    preferences: Option<crate::features::stream_control::StreamControlPreferences>,
    purpose: ControlPurpose,
) -> Result<ControlFrames> {
    if controller_device_id.len() != 16
        || !controller_device_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        bail!("cannot build control request for an invalid device identifier");
    }

    let selected_capabilities = &decoder_support.video_codec_capability;
    if selected_capabilities.is_empty() {
        bail!("cannot build control request without a UU-compatible video decoder");
    }
    let streamer_data = serde_json::to_string(&json!({
        "control_id": app_control_id,
        "openuuyc_audio_only": purpose == ControlPurpose::Audio,
        "openuuyc_audio_control": 1,
        "openuuyc_annotation": 1,
        "openuuyc_display_geometry": 1,
        // 终端会话必须把 BINARY_DATA_CHANNEL 留给 TERM 帧；被控端看到
        // 这个扩展就会把同一条通道绑定到诊断下载，终端请求没人接。
        "openuuyc_diagnostics": if purpose == ControlPurpose::Terminal { 0 } else { 1 },
        "openuuyc_drag_drop": 1,
        "device_capability": decoder_support
    }))?;
    let event = json!([
        "control",
        {
            "app_control_id": app_control_id,
            "app_data": { "_placeholder": true, "num": 0 },
            "streamer_data": streamer_data
        }
    ]);

    Ok(ControlFrames {
        header: format!("451-{ack_id}{}", serde_json::to_string(&event)?),
        attachment: encode_connect_options(
            controller_device_id,
            selected_capabilities,
            profile,
            connect_type,
            preferences,
            purpose,
        )?,
        app_control_id: app_control_id.to_owned(),
    })
}

fn encode_connect_options(
    controller_device_id: &str,
    selected_capabilities: &[CodecCapability],
    profile: ConnectionMediaProfile,
    connect_type: ControlConnectType,
    preferences: Option<crate::features::stream_control::StreamControlPreferences>,
    purpose: ControlPurpose,
) -> Result<Vec<u8>> {
    let mut capture = Vec::new();
    let fps_level = match profile.stream_fps {
        30 => 1,
        60 => 2,
        90 => 3,
        _ => 4,
    };
    push_varint_field(&mut capture, 1, fps_level);
    // Preserve the confirmed quality in the initial request and room rebuild.
    // Do not request Auto first and correct it only after receiving video.
    let (quality, auto_quality, bitrate) = preferences
        .map(|p| p.initial_capture_quality())
        .transpose()?
        .unwrap_or((5, 2, 0));
    push_varint_field(&mut capture, 2, quality);
    // The initial handshake leaves cursor_capture (tag 3) false. Once PB
    // and screen state are ready, StreamControl sends the viewing policy
    // with cursor capture enabled. See docs/official-440-controller-route.md.
    push_varint_field(&mut capture, 4, 3); // follow remote resolution
    // This is the controller's physical display size, not a request to resize
    // the controlled display. GameViewerServer::DisplayLayout::tryOpen falls
    // back to 1920x1080 when this field is absent and feeds that fallback into
    // display-layout initialization. The official controller always includes
    // the real local size, including when FOLLOW_REMOTE is selected.
    let mut local_resolution = Vec::new();
    push_varint_field(
        &mut local_resolution,
        1,
        u64::from(profile.local_display.width),
    );
    push_varint_field(
        &mut local_resolution,
        2,
        u64::from(profile.local_display.height),
    );
    push_bytes_field(&mut capture, 5, &local_resolution);
    push_varint_field(
        &mut capture,
        7,
        if preferences.is_some_and(|p| p.settings.true_color) {
            3
        } else {
            1
        },
    );
    if bitrate != 0 {
        push_varint_field(&mut capture, 8, bitrate);
    }
    if preferences.is_some_and(|p| p.settings.hdr)
        && selected_capabilities
            .iter()
            .any(|cap| cap.bit_depth >= 10 && cap.width > 0 && cap.height > 0)
    {
        push_varint_field(&mut capture, 9, 1);
    }
    push_varint_field(&mut capture, 10, auto_quality);
    push_varint_field(
        &mut capture,
        11,
        u64::from(profile.local_display.refresh_hz.min(profile.stream_fps)),
    );

    let mut options = Vec::new();
    push_varint_field(
        &mut options,
        1,
        match purpose {
            ControlPurpose::Viewing | ControlPurpose::Audio => 1,
            ControlPurpose::PortMapping => 9,
            ControlPurpose::FileTransfer => 5,
            ControlPurpose::Terminal => 8,
        },
    );
    push_signed_int32_field(&mut options, 2, -1);
    push_bytes_field(&mut options, 3, &capture);
    let mut decoder_profiles = Vec::new();
    for capability in selected_capabilities {
        let key = (capability.video_codec, capability.chroma_sampling);
        if !decoder_profiles.contains(&key) {
            decoder_profiles.push(key);
        }
    }
    for (codec, chroma) in decoder_profiles {
        let mut decoder = Vec::new();
        // This is the receiver-side FPS capability derived from the local
        // display tier. The requested stream FPS remains in CaptureParams.
        push_varint_field(&mut decoder, 1, u64::from(profile.decoder_fps_cap));
        push_varint_field(&mut decoder, 2, codec as u64);
        let (width, height) = selected_capabilities
            .iter()
            .filter(|cap| cap.video_codec == codec && cap.chroma_sampling == chroma)
            .map(|cap| (cap.width, cap.height))
            .max()
            .unwrap_or_default();
        push_varint_field(&mut decoder, 3, width as u64);
        push_varint_field(&mut decoder, 4, height as u64);
        push_varint_field(&mut decoder, 5, u64::from(chroma));
        push_bytes_field(&mut options, 4, &decoder);
    }
    if purpose == ControlPurpose::Viewing {
        for (width, height) in crate::media::local_display_dimensions() {
            let mut mode = Vec::new();
            push_varint_field(&mut mode, 1, u64::from(width));
            push_varint_field(&mut mode, 2, u64::from(height));
            push_varint_field(&mut mode, 3, 60); // VirtualDisplayMode.VSync
            push_bytes_field(&mut options, 6, &mode);
        }
    }
    push_varint_field(&mut options, 8, 3); // desktop controller ABI value
    // ConnectOptions.device_id identifies the controller. The controlled host
    // uses it as the key for per-controller display-layout state. Sending the
    // target's own ID here selects the wrong state and can reapply a stale
    // physical monitor mode during session initialization.
    push_bytes_field(&mut options, 9, controller_device_id.as_bytes());
    // The actual connection source determines the host session semantics;
    // Assistance also gates host-side display changes.
    push_varint_field(&mut options, 10, connect_type as u64);
    // Keep initial ConnectOptions and both ECHO directions identical. Feature
    // levels are not harmless placeholders: the host reads private_screen
    // during display initialization, before the later ECHO replacement.
    let feature_flag = crate::features::stream_control::encode_read_only_feature_flags();
    push_bytes_field(&mut options, 11, &feature_flag);
    push_bytes_field(&mut options, 12, PROTOCOL_VERSION.as_bytes());
    Ok(options)
}

fn push_signed_int32_field(output: &mut Vec<u8>, number: u32, value: i32) {
    push_varint_field(output, number, value as i64 as u64);
}

fn push_varint_field(output: &mut Vec<u8>, number: u32, value: u64) {
    push_varint(output, u64::from(number) << 3);
    push_varint(output, value);
}

fn push_bytes_field(output: &mut Vec<u8>, number: u32, value: &[u8]) {
    push_varint(output, (u64::from(number) << 3) | 2);
    push_varint(output, value.len() as u64);
    output.extend_from_slice(value);
}

fn push_varint(output: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        output.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}
