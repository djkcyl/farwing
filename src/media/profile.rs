use anyhow::{Result, bail};

const FPS_CHOICES: [FrameRateChoice; 5] = [
    FrameRateChoice::Auto,
    FrameRateChoice::Fps144,
    FrameRateChoice::Fps90,
    FrameRateChoice::Fps60,
    FrameRateChoice::Fps30,
];
const FPS_LEVELS_ASCENDING: [u32; 4] = [30, 60, 90, 144];

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct LocalDisplayInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

impl LocalDisplayInfo {
    pub const FALLBACK: Self = Self {
        width: 1920,
        height: 1080,
        refresh_hz: 60,
    };
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameRateChoice {
    Auto,
    Fps144,
    Fps90,
    Fps60,
    Fps30,
}

impl FrameRateChoice {
    pub fn value(self, display: LocalDisplayInfo) -> u32 {
        match self {
            Self::Auto => max_frame_rate_level(display.refresh_hz),
            Self::Fps144 => 144,
            Self::Fps90 => 90,
            Self::Fps60 => 60,
            Self::Fps30 => 30,
        }
    }

    pub fn available(_display: LocalDisplayInfo) -> Vec<Self> {
        // The ordinary desktop menu exposes all four explicit levels. The
        // receiver refresh rate limits fps_count, not the user's level list.
        FPS_CHOICES.to_vec()
    }

    pub fn label(self, display: LocalDisplayInfo) -> String {
        let value = self.value(display);
        if self == Self::Auto {
            format!("自动 {value} FPS")
        } else {
            format!("{value} FPS")
        }
    }
    pub(crate) fn requires_faster_screen(self, display: LocalDisplayInfo, refresh_hz: u32) -> bool {
        // Levels are ceilings: a 120Hz screen fits in the 144 tier, and a
        // 75Hz screen in the 90 tier. Only a higher tier needs a virtual mode.
        refresh_hz > 0
            && self.value(display).min(display.refresh_hz) > max_frame_rate_level(refresh_hz)
    }
}

impl std::str::FromStr for FrameRateChoice {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "144" | "144fps" => Ok(Self::Fps144),
            "90" | "90fps" => Ok(Self::Fps90),
            "60" | "60fps" => Ok(Self::Fps60),
            "30" | "30fps" => Ok(Self::Fps30),
            _ => bail!("unsupported frame-rate choice: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CodecPreference {
    #[default]
    Auto,
    PreferH264,
    PreferH265,
    PreferAv1,
    H264,
    H265,
    Av1,
}

impl CodecPreference {
    pub(crate) fn accepts(self, codec: VideoCodec) -> bool {
        match self {
            Self::Auto | Self::PreferH264 | Self::PreferH265 | Self::PreferAv1 => true,
            Self::H264 => codec == VideoCodec::H264,
            Self::H265 => codec == VideoCodec::H265,
            Self::Av1 => codec == VideoCodec::Av1,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::PreferH264 => "优先 H.264",
            Self::PreferH265 => "优先 HEVC",
            Self::PreferAv1 => "优先 AV1",
            Self::H264 => "仅 H.264",
            Self::H265 => "仅 HEVC",
            Self::Av1 => "仅 AV1",
        }
    }

    pub const PREFERRED: [Self; 4] = [
        Self::Auto,
        Self::PreferAv1,
        Self::PreferH265,
        Self::PreferH264,
    ];
    pub const ONLY: [Self; 3] = [Self::Av1, Self::H265, Self::H264];
    pub fn preferred(self) -> Option<i32> {
        match self {
            Self::Auto => None,
            Self::PreferH264 | Self::H264 => Some(1),
            Self::PreferH265 | Self::H265 => Some(2),
            Self::PreferAv1 | Self::Av1 => Some(5),
        }
    }
    pub fn restricted(self) -> bool {
        matches!(self, Self::H264 | Self::H265 | Self::Av1)
    }
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::PreferAv1,
            Self::PreferAv1 | Self::Av1 => Self::PreferH265,
            Self::PreferH265 | Self::H265 => Self::PreferH264,
            _ => Self::Auto,
        }
    }
}

impl std::str::FromStr for CodecPreference {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "prefer-h264" => Ok(Self::PreferH264),
            "prefer-h265" | "prefer-hevc" => Ok(Self::PreferH265),
            "prefer-av1" => Ok(Self::PreferAv1),
            "h264" | "avc" => Ok(Self::H264),
            "h265" | "hevc" => Ok(Self::H265),
            "av1" => Ok(Self::Av1),
            _ => bail!("unsupported codec preference: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TransportChoice {
    Auto,
    P2p,
    Relay,
}

impl TransportChoice {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动（LAN/P2P/relay）",
            Self::P2p => "仅 LAN/P2P",
            Self::Relay => "仅 relay",
        }
    }
}

impl std::str::FromStr for TransportChoice {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "p2p" | "direct" | "lan" => Ok(Self::P2p),
            "relay" | "turn" => Ok(Self::Relay),
            _ => bail!("unsupported transport choice: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ConnectionMediaOptions {
    pub audio_only: bool,
    pub muted: bool,
    pub fullscreen_floating_ball: bool,
    pub frame_rate: FrameRateChoice,
    pub codec: CodecPreference,
    pub decoder: super::selection::DecoderPreference,
    pub transport: TransportChoice,
}

impl Default for ConnectionMediaOptions {
    fn default() -> Self {
        Self {
            audio_only: false,
            muted: false,
            fullscreen_floating_ball: false,
            frame_rate: FrameRateChoice::Auto,
            codec: CodecPreference::Auto,
            decoder: Default::default(),
            transport: TransportChoice::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct ConnectionMediaProfile {
    pub audio_only: bool,
    pub muted: bool,
    pub local_display: LocalDisplayInfo,
    pub stream_fps: u32,
    pub decoder_fps_cap: u32,
    pub codec: CodecPreference,
    pub decoder: super::selection::DecoderPreference,
}

impl ConnectionMediaOptions {
    pub(crate) fn resolve(self, display: LocalDisplayInfo) -> Result<ConnectionMediaProfile> {
        self.validate()?;
        let stream_fps = self.frame_rate.value(display);
        Ok(ConnectionMediaProfile {
            audio_only: self.audio_only,
            muted: self.muted,
            local_display: display,
            stream_fps,
            decoder_fps_cap: display.refresh_hz.max(stream_fps),
            codec: self.codec,
            decoder: self.decoder,
        })
    }
    pub fn validate(self) -> Result<()> {
        anyhow::ensure!(
            self.decoder.mode != super::selection::ProcessingMode::Software
                || self.codec != CodecPreference::H265,
            "软件解码不支持 HEVC，请取消仅 HEVC 限制或启用硬解"
        );
        Ok(())
    }
}

fn max_frame_rate_level(refresh_hz: u32) -> u32 {
    FPS_LEVELS_ASCENDING
        .into_iter()
        .find(|level| refresh_hz <= *level)
        .unwrap_or(144)
}

pub use openuuyc_codec::Codec as VideoCodec;
