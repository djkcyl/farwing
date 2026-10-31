//! Farwing Protocol v2.
//!
//! Shared wire definitions for the protocol layers described in
//! `docs/protocol/farwing-protocol.md` and `docs/protocol/v2-design.md`:
//!
//! * [`api`]: the HTTPS control-plane API (`/api/v2/...`).
//! * [`realtime`]: the WSS realtime plane (`/api/v2/ws`): presence, session
//!   coordination, SDP/ICE relay and server pushes.
//! * [`peer`] (feature `peer`): the client-to-client data-channel frame layer.
//! * [`crypto`] (feature `crypto`): device keys, request signatures, DTLS
//!   identity binding and the assistance PAKE.
//!
//! Farwing Client and Farwing Server both depend on this crate, so producers
//! and consumers cannot drift apart.

pub mod api;
pub mod direct;
pub mod error;
pub mod ids;
pub mod realtime;
pub mod session;

#[cfg(feature = "crypto")]
pub mod crypto;
#[cfg(feature = "peer")]
pub mod peer;

pub use error::{ErrorBody, ErrorCode, ErrorEnvelope};

use serde::{Deserialize, Serialize};

/// Protocol name advertised by `GET /api/v2/server`.
pub const PROTOCOL_NAME: &str = "farwing";
/// Current protocol major version.
pub const PROTOCOL_VERSION: u32 = 3;
/// Oldest major version this build can speak; no other version interoperates.
pub const MIN_PROTOCOL_VERSION: u32 = 3;
/// URL prefix of every HTTPS endpoint.
pub const API_PREFIX: &str = "/api/v2";
/// Path of the realtime WebSocket endpoint.
pub const WS_PATH: &str = "/api/v2/ws";

/// HTTP header names (lower-case, as transmitted by HTTP/2 and accepted by HTTP/1).
pub mod headers {
    pub const PROTOCOL: &str = "x-farwing-protocol";
    pub const REQUEST_ID: &str = "x-farwing-request-id";
    /// Device identity of a signed request.
    pub const DEVICE_ID: &str = "x-farwing-device-id";
    /// Unix milliseconds covered by the request signature.
    pub const TIMESTAMP: &str = "x-farwing-timestamp";
    /// base64url Ed25519 signature over [`crate::crypto::request_message`].
    pub const SIGNATURE: &str = "x-farwing-signature";
    pub const TICKET: &str = "x-farwing-ticket";
    pub const RESUME: &str = "x-farwing-resume";
}

/// Resource limits shared by both sides. Servers may advertise lower values.
pub mod limits {
    /// Maximum HTTPS JSON request body.
    pub const MAX_JSON_BODY: usize = 64 * 1024;
    /// Maximum wallpaper upload.
    pub const MAX_WALLPAPER_BYTES: usize = 4 * 1024 * 1024;
    /// Maximum realtime frame.
    pub const MAX_WS_FRAME: usize = 1024 * 1024;
    /// Maximum SDP carried by `rtc.offer`/`rtc.answer`.
    pub const MAX_SDP_BYTES: usize = 1024 * 1024;
    /// Maximum serialized ICE candidate.
    pub const MAX_CANDIDATE_BYTES: usize = 2 * 1024;
    /// Maximum candidates relayed per session attempt and direction.
    pub const MAX_CANDIDATES: usize = 256;
    /// Sustained realtime messages per connection and second.
    pub const WS_RATE_PER_SECOND: u32 = 200;
    /// Realtime burst allowance.
    pub const WS_RATE_BURST: u32 = 400;
    /// Maximum peer (data channel) frame, equal to the SCTP max-message-size.
    pub const MAX_PEER_FRAME: usize = 524_288;
    /// Long-poll wait for an assistance decision (code check plus confirmation).
    pub const ASSIST_DECISION_WAIT_MS: u64 = 65_000;
    /// Wait for a host to answer an assistance push.
    pub const ASSIST_HOST_REPLY_WAIT_MS: u64 = 10_000;
    /// Lifetime of a single-use realtime ticket.
    pub const TICKET_TTL_SECONDS: u64 = 60;
    /// Lifetime of a resume token after the connection drops.
    pub const RESUME_TTL_SECONDS: u64 = 300;
    /// Accepted clock skew of a signed request.
    pub const REQUEST_SKEW_MS: i64 = 300_000;
    /// Window in which a request signature may not be replayed.
    pub const SIGNATURE_REPLAY_WINDOW_S: u64 = 600;
    /// Default number of concurrent participants per host.
    pub const DEFAULT_MAX_PARTICIPANTS: u32 = 8;
    /// Upper bound a host may declare.
    pub const MAX_PARTICIPANTS: u32 = 16;
    /// Lifetime of an access token without use (sliding).
    pub const ACCESS_TOKEN_IDLE_DAYS: i64 = 30;
}

/// Default timing advertised in tickets and `hello`.
pub mod timing {
    pub const CONNECT_TIMEOUT_MS: u32 = 10_000;
    pub const RETRY_DELAY_MS: u32 = 1_000;
    pub const MAX_RECONNECT_DELAY_S: u32 = 30;
    pub const HEARTBEAT_MS: u32 = 20_000;
    pub const IDLE_TIMEOUT_MS: u32 = 60_000;
    /// Requests that expect an `ack` (session.start, resume.refresh).
    pub const REQUEST_TIMEOUT_MS: u32 = 10_000;
    /// Resume-token refresh check interval and maximum token age.
    pub const RESUME_CHECK_INTERVAL_S: u64 = 30;
    pub const RESUME_MAX_AGE_S: u64 = 300;
}

/// Device platform.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    #[default]
    Windows,
    Macos,
    Linux,
    Android,
    Ios,
    /// A web browser (the server's web console): views only, never hosts
    /// (`docs/web-console.md` §7.1).
    Web,
    /// A platform newer than this build.
    #[serde(other)]
    Unknown,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Macos => "macos",
            Self::Linux => "linux",
            Self::Android => "android",
            Self::Ios => "ios",
            Self::Web => "web",
            Self::Unknown => "unknown",
        }
    }
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text.trim().to_ascii_lowercase().as_str() {
            "windows" | "win" => Self::Windows,
            "macos" | "mac" => Self::Macos,
            "linux" => Self::Linux,
            "android" => Self::Android,
            "ios" => Self::Ios,
            "web" | "browser" => Self::Web,
            _ => return None,
        })
    }
    /// Desktop platforms can host remote control.
    pub fn is_desktop(self) -> bool {
        matches!(self, Self::Windows | Self::Macos | Self::Linux)
    }
}

/// What a device can do before any session exists (reported at device
/// initialization). In-session features are negotiated by the peer HELLO.
pub mod capability {
    /// Can be controlled (has a host implementation).
    pub const HOST: &str = "host";
    pub const FILES: &str = "files";
    pub const PORTS: &str = "ports";
    /// Remote shutdown / reboot.
    pub const POWER: &str = "power";
    pub const WAKE_ON_LAN: &str = "wake_on_lan";
    /// Accepts remote assistance by connect ID.
    pub const ASSIST: &str = "assist";
}

/// Session capabilities granted to a participant (`docs/protocol/v2-design.md` §3.2).
pub mod caps {
    pub const VIEW: &str = "view";
    pub const CONTROL: &str = "control";
    pub const CLIPBOARD: &str = "clipboard";
    pub const FILES: &str = "files";
    pub const PORTS: &str = "ports";
    pub const POWER: &str = "power";
    pub const AUDIO: &str = "audio";
    pub const MICROPHONE: &str = "microphone";
    pub const DISPLAY: &str = "display";
    pub const DIAGNOSTICS: &str = "diagnostics";
    pub const ANNOTATION: &str = "annotation";
    /// See the host's cameras (`docs/protocol/camera-design.md` §2).
    pub const CAMERA: &str = "camera";
    /// Hear the host's microphone.
    pub const LISTEN: &str = "listen";

    /// Every capability, in display order.
    pub const ALL: &[&str] = &[
        VIEW,
        CONTROL,
        CLIPBOARD,
        FILES,
        PORTS,
        POWER,
        AUDIO,
        MICROPHONE,
        DISPLAY,
        DIAGNOSTICS,
        ANNOTATION,
        CAMERA,
        LISTEN,
    ];
    /// Offered to a new friend on all devices (the one accepting chooses).
    pub const DEFAULT_FRIEND: &[&str] = &[VIEW, AUDIO, ANNOTATION];
    /// Default host template for remote assistance.
    pub const DEFAULT_ASSIST: &[&str] = &[VIEW, CONTROL, CLIPBOARD, AUDIO, ANNOTATION];
    /// What a browser controller can use: a server's web limit is a subset
    /// of these, and all of them by default.
    pub const WEB: &[&str] = &[
        VIEW, CONTROL, CLIPBOARD, FILES, AUDIO, MICROPHONE, ANNOTATION, CAMERA, LISTEN,
    ];

    pub fn is_known(name: &str) -> bool {
        ALL.contains(&name)
    }

    /// Known names only, deduplicated, in [`ALL`] order.
    pub fn normalize<S: AsRef<str>>(names: &[S]) -> Vec<String> {
        ALL.iter()
            .filter(|cap| names.iter().any(|n| n.as_ref() == **cap))
            .map(|cap| (*cap).to_owned())
            .collect()
    }

    /// Every capability.
    pub fn all() -> Vec<String> {
        ALL.iter().map(|cap| (*cap).to_owned()).collect()
    }
}
