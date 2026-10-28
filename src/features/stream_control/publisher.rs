//! Sending-side business protocol; deliberately separate from viewer consumers.
use super::wire::{SessionBody, microphone_state};
use super::*;
use crate::features::host::VideoConfig;
use crate::features::host::capture::Screen;

mod displays;
pub(crate) use displays::{display_list, receive_session, refuse};

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
        enabled: policy.enabled,
        request_id: received.request_id,
    }))
}
impl MicrophonePolicy {
    /// The answer: carried out, or the refusal's code and reason.
    pub fn response(&self, result: std::result::Result<(), (&str, &str)>) -> Vec<u8> {
        super::wire::answer(self.request_id, result)
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
    /// The answer: carried out (taken or given back), or the refusal's code
    /// and reason.
    pub fn response(&self, result: std::result::Result<(), (&str, &str)>) -> Vec<u8> {
        super::wire::answer(self.request_id, result)
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
    crate::protocol::host_event::encode(crate::protocol::host_event::kind::UPDATING, "")
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
    pub decoders: Vec<farwing_protocol::session::Decoder>,
    pub display: Option<farwing_protocol::session::LocalDisplay>,
    /// The stream the session starts with.
    pub stream: farwing_protocol::session::StreamSettings,
    /// The controller's device ID.
    pub device_id: String,
    pub connect_type: i32,
    features: std::collections::BTreeMap<String, u32>,
}
impl ConnectOptions {
    pub fn from_wire(wire: &farwing_protocol::session::ConnectOptions) -> Self {
        Self {
            decoders: wire.decoders.clone(),
            display: wire.display,
            stream: wire.stream.unwrap_or(farwing_protocol::session::StreamSettings {
                fps: 60,
                cursor: true,
                ..Default::default()
            }),
            // Set by the host from the server's `session.incoming`.
            kind: 0,
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

/// `HELLO` sent by the host when its control channel opens.
pub(crate) fn hello_request() -> Vec<u8> {
    crate::protocol::peer::hello_request(crate::protocol::peer::Role::Host)
}


#[derive(Default)]
pub(crate) struct Received {
    pub messages: Vec<Vec<u8>>,
    pub handshake_complete: bool,
    pub handshake_request: bool,
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
/// The host's quality number of `quality` (1–4 a tier, 5 automatic, 6
/// custom).
pub(crate) fn quality_number(quality: farwing_protocol::session::Quality) -> i32 {
    use farwing_protocol::session::Quality as Q;
    match quality {
        Q::Low => 1,
        Q::Standard => 2,
        Q::High => 3,
        Q::Ultra => 4,
        Q::Auto => 5,
        Q::Custom => 6,
    }
}

/// The frame rates a stream takes.
const FPS: std::ops::RangeInclusive<u32> = 1..=240;
/// The custom bitrates a stream takes (kbit/s).
const BITRATE_KBPS: std::ops::RangeInclusive<u32> = 1_000..=MAX_CUSTOM_BITRATE_MBPS * 1_000;

/// Whether the host takes `settings`.
pub(crate) fn valid(settings: &farwing_protocol::session::StreamSettings) -> bool {
    FPS.contains(&settings.fps)
        && (settings.quality != farwing_protocol::session::Quality::Custom
            || BITRATE_KBPS.contains(&settings.bitrate_kbps))
}

/// `config` asking for `settings`; `auto_start` is the tier automatic
/// quality starts from. The format is chosen separately ([`configure`]).
pub(crate) fn config_of(
    settings: &farwing_protocol::session::StreamSettings,
    auto_start: i32,
    mut config: VideoConfig,
) -> VideoConfig {
    use farwing_protocol::session::WeakNetwork;
    let fps = settings.fps.clamp(*FPS.start(), *FPS.end());
    config.fps = fps.min(config.maximum_fps.max(1));
    config.requested_fps = fps;
    config.fps_limit = fps;
    config.quality = quality_number(settings.quality);
    config.auto_quality = auto_start.clamp(1, 4);
    config.bitrate = if settings.quality == farwing_protocol::session::Quality::Custom {
        settings
            .bitrate_kbps
            .clamp(*BITRATE_KBPS.start(), *BITRATE_KBPS.end())
            * 1_000
    } else {
        0
    };
    config.cursor_capture = settings.cursor;
    config.weak_network = match settings.weak_network {
        WeakNetwork::Bitrate => crate::features::host::parameters::WeakNetwork::Bitrate,
        WeakNetwork::FrameRate => crate::features::host::parameters::WeakNetwork::FrameRate,
    };
    config.cinema = settings.cinema;
    config.requested_codec = settings.codec.map(crate::features::host::format::Codec::of);
    // The participant's own settings from the start (`session.start`).
    config.configured = true;
    config
}

/// Choose the format of `config` for `screen`: the requested codec, 4:4:4
/// and HDR where they are possible.
pub(crate) fn configure(
    settings: &farwing_protocol::session::StreamSettings,
    screen: &Screen,
    config: &mut VideoConfig,
    negotiated: &crate::features::host::format::Negotiated,
) -> Result<()> {
    let mut next = config_of(settings, config.auto_quality, *config);
    next.revision = next.revision.wrapping_add(1);
    negotiated.apply_requested(
        &mut next,
        if settings.chroma444 { 3 } else { 1 },
        settings.hdr,
        (screen.width, screen.height),
    )?;
    *config = next;
    tracing::info!(
        fps = config.fps,
        quality = config.quality,
        format = ?config.format,
        maximum = ?config.maximum,
        "host stream settings applied"
    );
    Ok(())
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
        ..Default::default()
    })
}

/// Whether this host accepts files on the clipboard.
pub(crate) fn clipboard_permission(enabled: bool) -> Vec<u8> {
    super::wire::push(SessionBody::ClipboardFiles(super::wire::ClipboardPermission {
        enabled,
    }))
}
