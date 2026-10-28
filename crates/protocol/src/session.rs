//! What a controller asks for when it starts a session (`session.start`,
//! relayed as `session.incoming`): what it can decode, its display, and the
//! stream it starts with. The server relays these; only the host reads them.

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

/// Video codecs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

/// Stream quality: automatic, a tier by output size, or a custom bitrate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    /// The host picks the tier from the bandwidth.
    #[default]
    Auto,
    /// Up to 1280×720.
    Low,
    /// Up to 1920×1080.
    Standard,
    /// Up to 2560×1440.
    High,
    /// Up to 3840×2160.
    Ultra,
    /// Native size at [`StreamSettings::bitrate_kbps`].
    Custom,
}

/// What gives way on a weak network.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WeakNetwork {
    #[default]
    Bitrate,
    FrameRate,
}

/// A format the controller decodes, up to a size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decoder {
    pub codec: Codec,
    pub chroma444: bool,
    pub bit10: bool,
    pub width: u32,
    pub height: u32,
    /// Decoded in hardware.
    pub hardware: bool,
}

/// The controller's display: displays the host creates for it take this
/// size and scale.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalDisplay {
    pub width: u32,
    pub height: u32,
    /// Percent; 0 when unknown.
    #[serde(default)]
    pub scale: u32,
}

/// The stream a participant asks for (also SESSION `StreamRequest` and
/// `StreamState` on the peer channel).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamSettings {
    pub quality: Quality,
    /// [`Quality::Custom`]: the bitrate ceiling.
    pub bitrate_kbps: u32,
    /// Frames per second, at most the controller's refresh rate.
    pub fps: u32,
    pub chroma444: bool,
    pub hdr: bool,
    /// `None`: the host chooses.
    pub codec: Option<Codec>,
    /// The cursor is drawn into the picture.
    pub cursor: bool,
    pub weak_network: WeakNetwork,
    /// Quality-first encoding for watching video.
    pub cinema: bool,
}

/// Controller connect options (`session.start`). The session's purpose,
/// type and the controller's identity are stated by the server in
/// `session.incoming`; in-session features are negotiated by the peer HELLO.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectOptions {
    pub decoders: Vec<Decoder>,
    pub display: Option<LocalDisplay>,
    /// The stream to start with; the host's defaults when absent.
    pub stream: Option<StreamSettings>,
}
