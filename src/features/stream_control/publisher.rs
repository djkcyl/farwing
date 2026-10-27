//! Sending-side business protocol; deliberately separate from viewer consumers.
use super::wire::{PbCaptureSettingRequest, SessionBody, microphone_state};
use super::*;
use crate::features::host::VideoConfig;
use crate::features::host::capture::Screen;
use anyhow::Context as _;

mod displays;
pub(crate) use displays::{receive_session, screen_states};

/// A `MicrophonePolicy` request of the controller.
pub(crate) struct MicrophonePolicy {
    pub enabled: bool,
    request_id: u32,
}
pub(crate) fn microphone_policy(bytes: &[u8]) -> Result<Option<MicrophonePolicy>> {
    let Some(received) = super::wire::decode(bytes)? else {
        return Ok(None);
    };
    let Some(SessionBody::MicrophonePolicy(policy)) = received.body else {
        return Ok(None);
    };
    anyhow::ensure!(received.request_id != 0, "麦克风请求缺少标识");
    Ok(Some(MicrophonePolicy {
        enabled: policy.policy & 1 != 0,
        request_id: received.request_id,
    }))
}
impl MicrophonePolicy {
    pub fn response(&self, error_code: i32) -> Vec<u8> {
        super::wire::response(
            self.request_id,
            SessionBody::MicrophoneResult(super::microphone::PolicyResponse { error_code }),
        )
    }
}

/// A `ControlTurn` request of the controller.
pub(crate) struct ControlTurn {
    pub take: bool,
    pub automatic: bool,
    pub ask: bool,
    request_id: u32,
}
pub(crate) fn control_turn(bytes: &[u8]) -> Result<Option<ControlTurn>> {
    let Some(received) = super::wire::decode(bytes)? else {
        return Ok(None);
    };
    let Some(SessionBody::ControlTurn(request)) = received.body else {
        return Ok(None);
    };
    anyhow::ensure!(received.request_id != 0, "控制权请求缺少标识");
    Ok(Some(ControlTurn {
        take: request.take,
        automatic: request.automatic,
        ask: request.ask,
        request_id: received.request_id,
    }))
}
impl ControlTurn {
    pub fn response(&self, held: bool, refusal: i32, holder: String) -> Vec<u8> {
        super::wire::response(
            self.request_id,
            SessionBody::ControlTurnResult(super::control_turn::TurnResponse {
                held,
                refusal,
                holder,
            }),
        )
    }
}

/// Host virtual microphone state (one of [`microphone_state`]).
pub(crate) fn microphone_event(state: i32) -> Vec<u8> {
    debug_assert!(matches!(
        state,
        microphone_state::ACTIVE | microphone_state::INACTIVE | microphone_state::RESTART
    ));
    super::wire::push(SessionBody::Microphone(super::wire::MicrophoneState { state }))
}

/// `HOST_EVENT` announcing a locally confirmed update of this host.
pub(crate) fn update_started() -> Vec<u8> {
    crate::protocol::host_event::encode(crate::protocol::host_event::kind::UPDATING, 0, "")
}

/// Session kinds of [`ConnectOptions::kind`], from the server-stated purpose.
pub(crate) mod kind {
    pub const DESKTOP: i32 = 1;
    pub const FILES: i32 = 5;
    pub const PORTS: i32 = 9;
}

/// Host view of the controller's connect options (`session.start`).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ConnectOptions {
    pub kind: i32,
    pub params: Option<CaptureParams>,
    pub decoders: Vec<DecoderCapability>,
    pub client_type: i32,
    /// The controller's device ID.
    pub device_id: String,
    pub connect_type: i32,
    features: std::collections::BTreeMap<String, u32>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Resolution {
    #[prost(int32, tag = "1")]
    pub width: i32,
    #[prost(int32, tag = "2")]
    pub height: i32,
}
impl ConnectOptions {
    pub fn from_wire(wire: &farwing_protocol::session::ConnectOptions) -> Self {
        let resolution = |r: &farwing_protocol::session::Resolution| Resolution {
            width: r.width,
            height: r.height,
        };
        Self {
            params: wire.capture.as_ref().map(|c| CaptureParams {
                fps: c.fps,
                quality: c.quality,
                cursor_capture: c.cursor_capture,
                local_resolution: c.local_resolution.as_ref().map(resolution),
                chroma: c.chroma,
                bitrate: c.bitrate,
                hdr: c.hdr,
                auto_quality: c.auto_quality,
                fps_count: c.fps_count,
                weak_network: c.weak_network,
                cinema: c.cinema,
                codec: c.codec,
            }),
            decoders: wire
                .decoders
                .iter()
                .map(|d| DecoderCapability {
                    fps: d.fps,
                    codec: d.codec,
                    width: d.width,
                    height: d.height,
                    chroma: d.chroma,
                })
                .collect(),
            // Set by the host from the server's `session.incoming`.
            kind: 0,
            client_type: crate::protocol::peer_platform::WINDOWS_CLIENT_TYPE,
            device_id: String::new(),
            connect_type: 0,
            // Every v2 controller is this build; its HELLO confirms the set.
            features: crate::protocol::peer::controller_features(),
        }
    }
    fn features(&self) -> crate::protocol::peer::PeerFeatures {
        crate::protocol::peer::PeerFeatures::new(self.features.clone())
    }
    pub fn file_capabilities(&self) -> crate::features::file_transfer::host::Capabilities {
        crate::features::file_transfer::host::Capabilities::from_feature(
            self.features().has(crate::protocol::peer::feature::FILES),
        )
    }
    pub fn clipboard_level(&self) -> i32 {
        clipboard_level(&self.features())
    }
    pub fn control_screen_reports(&self) -> bool {
        self.features().has(crate::protocol::peer::feature::DISPLAY_CONFIG)
    }
}

/// Clipboard support of a peer: 0 none, 1 text and images, 2 also files.
fn clipboard_level(features: &crate::protocol::peer::PeerFeatures) -> i32 {
    use crate::protocol::peer::feature;
    if features.has(feature::CLIPBOARD_FILES) {
        2
    } else if features.has(feature::CLIPBOARD) {
        1
    } else {
        0
    }
}
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct DecoderCapability {
    #[prost(int32, tag = "1")]
    pub fps: i32,
    #[prost(int32, tag = "2")]
    pub codec: i32,
    #[prost(int32, tag = "3")]
    pub width: i32,
    #[prost(int32, tag = "4")]
    pub height: i32,
    #[prost(int32, tag = "5")]
    pub chroma: i32,
}
impl DecoderCapability {
    pub fn chroma_sampling(&self) -> u8 {
        // Protobuf's unspecified value means 4:2:0.
        // Android currently leaves this field unset, including for HEVC.
        match self.chroma {
            2..=4 => self.chroma as u8,
            _ => 1,
        }
    }

    pub fn maximum_fps(&self) -> u32 {
        // Convert a numeric rate to the protocol's tiers;
        // this is distinct from CaptureParams' 1/2/3/4 frame-rate enum.
        match self.fps as u32 {
            117.. => 144,
            75..=116 => 90,
            45..=74 => 60,
            _ => 30,
        }
    }
}
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct CaptureParams {
    #[prost(int32, tag = "1")]
    pub fps: i32,
    #[prost(int32, tag = "2")]
    pub quality: i32,
    #[prost(bool, tag = "3")]
    pub cursor_capture: bool,
    #[prost(message, optional, tag = "5")]
    pub local_resolution: Option<Resolution>,
    #[prost(int32, tag = "7")]
    pub chroma: i32,
    #[prost(int32, tag = "8")]
    pub bitrate: i32,
    #[prost(bool, tag = "9")]
    pub hdr: bool,
    #[prost(int32, tag = "10")]
    pub auto_quality: i32,
    #[prost(int32, tag = "11")]
    pub fps_count: i32,
    #[prost(int32, tag = "12")]
    pub weak_network: i32,
    #[prost(bool, tag = "13")]
    pub cinema: bool,
    #[prost(int32, tag = "14")]
    pub codec: i32,
}

/// `HELLO` sent by the host when its control channel opens.
pub(crate) fn hello_request() -> Vec<u8> {
    crate::protocol::peer::hello_request(crate::protocol::peer::Role::Host)
}


#[derive(Default)]
pub(crate) struct Received {
    pub messages: Vec<Vec<u8>>,
    pub handshake_complete: bool,
    pub handshake_request: bool,
    pub control_screen_reports: Option<bool>,
    pub refresh_state: bool,
    pub clipboard: Option<i32>,
    pub files: Option<crate::features::file_transfer::host::Capabilities>,
    pub media_active: Option<bool>,
}
impl From<Vec<Vec<u8>>> for Received {
    fn from(messages: Vec<Vec<u8>>) -> Self {
        Self {
            messages,
            ..Default::default()
        }
    }
}
pub(crate) fn quality_report(
    format: crate::features::host::format::Format,
    quality: i32,
    probe: u32,
    source: (u32, u32),
    fps: u32,
    encoder: crate::features::host::format::Backend,
    capture: &str,
) -> Vec<u8> {
    let _ = (format, source, fps);
    super::wire::push(SessionBody::Quality(PbReportQosStats {
        encoder_type: encoder.qos_type().into(),
        capture_type: capture.into(),
        probe_bps: probe.into(),
        video_quality: quality,
    }))
}

pub(crate) fn config(params: Option<&CaptureParams>) -> VideoConfig {
    let Some(p) = params else {
        return VideoConfig::default();
    };
    VideoConfig {
        fps: fps(p.fps, p.fps_count),
        requested_fps: fps(p.fps, 0),
        fps_limit: if p.fps_count > 0 {
            p.fps_count as u32
        } else {
            144
        },
        bitrate: bitrate(p.quality, p.auto_quality, p.bitrate),
        quality: quality(p.quality),
        auto_quality: auto_quality(p.auto_quality),
        revision: 0,
        reported_quality: if p.quality == 5 {
            auto_quality(p.auto_quality)
        } else {
            quality(p.quality)
        },
        sending: true,
        capturing: true,
        cursor_capture: p.cursor_capture,
        weak_network: crate::features::host::parameters::WeakNetwork::from_wire(p.weak_network),
        cinema: p.cinema,
        // The session starts with the controller's codec rather than
        // replacing the host's choice right after.
        requested_codec: crate::features::host::format::Codec::from_wire(p.codec),
        ..Default::default()
    }
}
fn fps(level: i32, count: i32) -> u32 {
    let requested = match level {
        1 => 30,
        2 => 60,
        3 => 90,
        4 => 144,
        _ => 30,
    };
    if count > 0 {
        requested.min(count as u32)
    } else {
        requested
    }
}
fn quality(value: i32) -> i32 {
    // Unspecified and unknown quality use the general tier.
    match value {
        1..=6 => value,
        _ => 2,
    }
}
fn auto_quality(value: i32) -> i32 {
    // The automatic baseline is general, HD, or Blu-ray.
    match value {
        3 | 4 => value,
        _ => 2,
    }
}
fn bitrate(quality: i32, auto: i32, custom: i32) -> u32 {
    match if quality == 5 { auto } else { quality } {
        4 => 30_000_000,
        3 => 14_000_000,
        // Both CaptureParams and CaptureSetting carry bits/second on the wire.
        6 => custom.clamp(1_000_000, MAX_CUSTOM_BITRATE_MBPS as i32 * 1_000_000) as u32,
        _ => 8_000_000,
    }
}

/// Host side of the peer `HELLO` on a control or session channel.
pub(crate) fn receive(bytes: &[u8]) -> Result<Received> {
    let Some(received) = crate::protocol::peer::decode_hello(bytes)? else {
        return Ok(Received::default());
    };
    let hello = &received.hello;
    let features = received.features();
    tracing::debug!(
        response = received.response,
        peer_version = %hello.client_version,
        "host peer HELLO received"
    );
    Ok(Received {
        handshake_complete: received.response,
        handshake_request: !received.response,
        messages: if received.response {
            Vec::new()
        } else {
            vec![crate::protocol::peer::hello_response(
                crate::protocol::peer::Role::Host,
                received.request_id,
            )]
        },
        clipboard: Some(clipboard_level(&features)),
        files: Some(
            crate::features::file_transfer::host::Capabilities::from_feature(
                features.has(crate::protocol::peer::feature::FILES),
            ),
        ),
        control_screen_reports: Some(features.has(crate::protocol::peer::feature::DISPLAY_CONFIG)),
        ..Default::default()
    })
}

/// Apply the media part of a `CaptureSetting` to one capture slot.
/// Display changes are carried out (and validated) by the caller; the
/// setting must describe the screen's current mode. Returns the per-screen
/// results (`docs/protocol/v2-design.md` §6).
pub(crate) fn apply_capture_setting(
    setting: &PbCaptureSettingRequest,
    screen: &Screen,
    config: &mut VideoConfig,
    negotiated: &crate::features::host::format::Negotiated,
) -> Vec<PbError> {
    let mut errors = Vec::new();
    if setting.max_scale_width != 0
        && setting.max_scale_height != 0
        && (setting.max_scale_width < 2 || setting.max_scale_height < 2)
    {
        errors.push(PbError {
            error_code: -1,
            error_message: super::wire::setting_reason::INVALID_SCALE_LIMIT.into(),
            ..Default::default()
        });
        return errors;
    }
    let mut next = *config;
    // Both axes must be present; an incomplete pair uses normal decoder
    // negotiation rather than retaining a stale limit.
    next.requested_maximum = (setting.max_scale_width > 0 && setting.max_scale_height > 0)
        .then_some((
            setting.max_scale_width as u32,
            setting.max_scale_height as u32,
        ));
    next.fps = fps(setting.fps, setting.fps_count);
    next.requested_fps = fps(setting.fps, 0);
    next.fps_limit = if setting.fps_count > 0 {
        setting.fps_count as u32
    } else {
        144
    };
    next.cursor_capture = setting.cursor_capture;
    next.weak_network =
        crate::features::host::parameters::WeakNetwork::from_wire(setting.weak_network);
    next.cinema = setting.cinema;
    next.bitrate = bitrate(
        setting.frame_quality,
        setting.auto_frame_quality,
        setting.max_custom_bitrate,
    );
    next.quality = quality(setting.frame_quality);
    next.auto_quality = auto_quality(setting.auto_frame_quality);
    next.revision = next.revision.wrapping_add(1);
    next.configured = true;
    // The codec the controller asks for (it changes when its user picks
    // another); the host's choice where that one is not available here.
    let codec = crate::features::host::format::Codec::from_wire(setting.codec_type);
    next.requested_codec = codec;
    let selected = if setting.codec_type == 0 || codec.is_some() {
        negotiated.apply_requested(
            &mut next,
            if setting.chroma_format == 3 { 3 } else { 1 },
            setting.enable_hdr,
            (screen.width, screen.height),
        )
    } else {
        Err(anyhow!("{}", t().features.stream_unsupported_codec))
    };
    match selected {
        Ok(()) => {
            // -6 reports the true-colour result, including a successful
            // enable or disable; it is not a failure.
            let color_result = if setting.chroma_format == 3 {
                if next.format.chroma != 3 {
                    Some(if negotiated.encoder_true_color() { 2 } else { 1 })
                } else if config.format.chroma != 3 {
                    Some(0)
                } else {
                    None
                }
            } else if config.format.chroma == 3 {
                Some(5)
            } else {
                None
            };
            if let Some(code) = color_result {
                errors.push(PbError {
                    error_code: -6,
                    error_detail: serde_json::json!({"error_code":code}).to_string(),
                    ..Default::default()
                });
            }
            // The requested frame rate exceeds what this screen refreshes at.
            if setting.fps_count > 0 && setting.fps_count as u32 > screen.fps {
                errors.push(PbError {
                    error_code: CAPTURE_RESULT_FPS_ADJUSTED,
                    ..Default::default()
                });
            }
            *config = next;
        }
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "requested format not applied");
            errors.push(PbError {
                error_code: -1,
                error_message: super::wire::setting_reason::UNSUPPORTED_CODEC.into(),
                ..Default::default()
            })
        }
    }
    tracing::info!(
        fps = config.fps,
        bitrate = config.bitrate,
        quality = config.quality,
        maximum = ?config.maximum,
        "host capture setting applied"
    );
    errors
}

/// Whether this host accepts files on the clipboard.
pub(crate) fn clipboard_permission(enabled: bool) -> Vec<u8> {
    super::wire::push(SessionBody::ClipboardFiles(super::wire::ClipboardPermission {
        enabled,
    }))
}
