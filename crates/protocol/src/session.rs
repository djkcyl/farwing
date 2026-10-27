//! Session negotiation payloads carried by `session.start` / `session.incoming`.
//!
//! The server validates their size and relays them; only the two clients
//! interpret the fields.

use serde::{Deserialize, Serialize};

/// Connection type of a session.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectType {
    /// Same-account device access.
    Account,
    /// Remote assistance by connect ID.
    Assist,
}


#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    pub width: i32,
    pub height: i32,
}

/// Initial capture parameters requested by the controller.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureParams {
    /// Frame-rate tier: 1 = 30, 2 = 60, 3 = 90, 4 = 144 FPS.
    pub fps: i32,
    /// Video quality tier 1..=6 (5 = automatic, 6 = custom bitrate); see stream control.
    pub quality: i32,
    pub cursor_capture: bool,
    /// 3 = follow the remote (host) resolution.
    pub resolution_type: i32,
    /// The controller's physical display size.
    pub local_resolution: Option<Resolution>,
    pub chosen_resolution: Option<Resolution>,
    /// 1 = 4:2:0, 3 = 4:4:4 (true color).
    pub chroma: i32,
    /// Custom maximum bitrate in bit/s; 0 = automatic.
    pub bitrate: i32,
    pub hdr: bool,
    /// Automatic-quality sub-tier.
    pub auto_quality: i32,
    /// Controller display refresh limit (FPS).
    pub fps_count: i32,
    /// What gives way on a weak network: 0 = bitrate first, 1 = frame rate first.
    pub weak_network: i32,
    /// Quality-first encoding for watching video (cinema mode).
    pub cinema: bool,
    /// The video codec the controller chose (1 = H.264, 2 = H.265,
    /// 5 = AV1); 0 lets the host choose.
    pub codec: i32,
}

/// One decoder profile the controller can receive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DecoderCapability {
    /// Receiver frame-rate capability (FPS).
    pub fps: i32,
    /// 1 = H.264, 2 = H.265, 5 = AV1.
    pub codec: i32,
    pub width: i32,
    pub height: i32,
    /// 1 = 4:2:0, 3 = 4:4:4.
    pub chroma: i32,
}

/// Controller connect options (sent in `session.start`). Only media and
/// display preferences: the session's purpose, type and the controller's
/// identity are stated by the server in `session.incoming`, and in-session
/// features are negotiated by the peer HELLO.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectOptions {
    /// Initially selected screen; -1 = host default.
    pub screen_id: i32,
    pub capture: Option<CaptureParams>,
    pub decoders: Vec<DecoderCapability>,
    /// Controller application version.
    pub version: String,
}

