//! Cameras as video sources (`docs/protocol/camera-design.md` §3): what a
//! camera offers and how one is opened, independent of the capture API.
use serde::{Deserialize, Serialize};

/// How a camera delivers pictures, in the default order of preference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PixelFormat {
    Nv12,
    Yuy2,
    Mjpeg,
    /// Only by choice (custom parameters).
    H264,
}

impl PixelFormat {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Nv12 => "NV12",
            Self::Yuy2 => "YUY2",
            Self::Mjpeg => "MJPEG",
            Self::H264 => "H.264",
        }
    }
}

/// One way a camera can deliver pictures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct Mode {
    pub width: u32,
    pub height: u32,
    /// Frames per second as a fraction (30000/1001 and the like).
    pub fps_numerator: u32,
    pub fps_denominator: u32,
    pub format: PixelFormat,
}

impl Mode {
    /// Whole frames per second, rounded.
    pub(crate) fn fps(&self) -> u32 {
        if self.fps_denominator == 0 {
            return 0;
        }
        (self.fps_numerator + self.fps_denominator / 2) / self.fps_denominator
    }
    pub(crate) fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

/// The YUV matrix of a camera's pictures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Matrix {
    Bt601,
    Bt709,
}

/// Color handling; `None` takes what the camera reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct Color {
    pub matrix: Option<Matrix>,
    pub full_range: Option<bool>,
}

/// How to capture a camera: its mode, color, bitrate ceiling and how
/// viewers turn the picture. `None` and the defaults mean automatic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Params {
    pub mode: Option<Mode>,
    pub color: Color,
    /// The bitrate ceiling; `None` by resolution and rate (§7).
    pub bitrate_kbps: Option<u32>,
    pub orientation: crate::media::geometry::Orientation,
}

/// The bitrate ceiling of a camera stream by default (§7).
pub(crate) fn default_bitrate_kbps(width: u32, height: u32, fps: u32) -> u32 {
    let lines = width.min(height);
    let high = fps > 30;
    match lines {
        0..=480 => if high { 2_500 } else { 1_500 },
        481..=720 => if high { 4_500 } else { 3_000 },
        721..=1080 => if high { 8_000 } else { 5_000 },
        _ => if high { 12_000 } else { 8_000 },
    }
}

/// The range a custom bitrate ceiling may take (kbit/s).
pub(crate) const BITRATE_RANGE_KBPS: std::ops::RangeInclusive<u32> = 300..=20_000;

/// A camera this machine has.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Camera {
    /// Stable across reconnects (the device's symbolic link).
    pub id: String,
    pub name: String,
    pub modes: Vec<Mode>,
    /// Why it cannot be opened now (in use, not allowed); its modes are
    /// then unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
}

/// The default mode (§3.4): 1080p30, else 720p30, else the largest at up
/// to 30 fps; among formats of that size and rate NV12, YUY2, then MJPEG.
pub(crate) fn default_mode(modes: &[Mode]) -> Option<Mode> {
    let usable = |m: &&Mode| m.format != PixelFormat::H264 && m.fps() >= 1;
    let pick = |filter: &dyn Fn(&Mode) -> bool| {
        modes
            .iter()
            .filter(usable)
            .filter(|m| filter(m))
            .min_by_key(|m| (m.format, std::cmp::Reverse(m.fps())))
            .copied()
    };
    pick(&|m| (m.width, m.height) == (1920, 1080) && m.fps() == 30)
        .or_else(|| pick(&|m| (m.width, m.height) == (1280, 720) && m.fps() == 30))
        .or_else(|| {
            let largest = modes
                .iter()
                .filter(usable)
                .filter(|m| m.fps() <= 30)
                .map(Mode::pixels)
                .max()?;
            let rate = modes
                .iter()
                .filter(usable)
                .filter(|m| m.fps() <= 30 && m.pixels() == largest)
                .map(Mode::fps)
                .max()?;
            pick(&|m| m.pixels() == largest && m.fps() == rate)
        })
        .or_else(|| modes.iter().filter(usable).min_by_key(|m| m.format).copied())
}

/// What to capture: a camera, in a mode, with a color handling, on the
/// adapter that encodes it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct Spec {
    pub camera: String,
    pub name: String,
    pub mode: Mode,
    pub color: Color,
    pub adapter: u64,
    /// A chosen bitrate ceiling.
    pub bitrate_kbps: Option<u32>,
    /// Identifies the capture among the encoder pipelines: the same camera
    /// in the same mode and color.
    key: String,
}

impl Spec {
    pub(crate) fn new(camera: &Camera, mode: Mode, color: Color, adapter: u64) -> Self {
        let key = format!(
            "camera:{}:{}x{}@{}/{}:{:?}:{:?}:{:?}",
            camera.id,
            mode.width,
            mode.height,
            mode.fps_numerator,
            mode.fps_denominator,
            mode.format,
            color.matrix,
            color.full_range
        );
        Self {
            camera: camera.id.clone(),
            name: camera.name.clone(),
            mode,
            color,
            adapter,
            bitrate_kbps: None,
            key,
        }
    }
    pub(crate) fn key(&self) -> &str {
        &self.key
    }
    /// The bitrate ceiling: chosen, or by resolution and rate (§7).
    pub(crate) fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
            .map(|kbps| kbps.clamp(*BITRATE_RANGE_KBPS.start(), *BITRATE_RANGE_KBPS.end()))
            .unwrap_or_else(|| default_bitrate_kbps(self.mode.width, self.mode.height, self.mode.fps()))
    }
}

/// A picture adjustment a camera may support (`IAMVideoProcAmp` and
/// `IAMCameraControl` properties).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Control {
    Brightness,
    Contrast,
    Hue,
    Saturation,
    Sharpness,
    Gamma,
    WhiteBalance,
    BacklightCompensation,
    Gain,
    /// Anti-flicker: 0 off, 1 50 Hz, 2 60 Hz, 3 automatic.
    PowerLineFrequency,
    Exposure,
    Focus,
    Zoom,
}

impl Control {
    pub(crate) const ALL: [Self; 13] = [
        Self::Brightness,
        Self::Contrast,
        Self::Hue,
        Self::Saturation,
        Self::Sharpness,
        Self::Gamma,
        Self::WhiteBalance,
        Self::BacklightCompensation,
        Self::Gain,
        Self::PowerLineFrequency,
        Self::Exposure,
        Self::Focus,
        Self::Zoom,
    ];
}

/// A supported adjustment: its range and current value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ControlState {
    pub control: Control,
    pub minimum: i32,
    pub maximum: i32,
    pub step: i32,
    pub default: i32,
    pub value: i32,
    /// The camera can set it automatically.
    pub can_auto: bool,
    pub auto: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(width: u32, height: u32, fps: u32, format: PixelFormat) -> Mode {
        Mode {
            width,
            height,
            fps_numerator: fps,
            fps_denominator: 1,
            format,
        }
    }

    #[test]
    fn the_default_prefers_1080p30_then_720p30_then_the_largest() {
        use PixelFormat::*;
        let modes = [
            mode(640, 480, 30, Yuy2),
            mode(1280, 720, 30, Mjpeg),
            mode(1280, 720, 10, Yuy2),
            mode(1920, 1080, 30, Mjpeg),
            mode(1920, 1080, 5, Yuy2),
            mode(1920, 1080, 30, H264),
        ];
        assert_eq!(default_mode(&modes), Some(mode(1920, 1080, 30, Mjpeg)));
        assert_eq!(default_mode(&modes[..3]), Some(mode(1280, 720, 30, Mjpeg)));
        // Same size and rate: the uncompressed format wins.
        let both = [mode(1280, 720, 30, Mjpeg), mode(1280, 720, 30, Nv12)];
        assert_eq!(default_mode(&both), Some(mode(1280, 720, 30, Nv12)));
        // Neither: the largest at up to 30 fps, at its best rate.
        let other = [mode(800, 600, 15, Yuy2), mode(800, 600, 25, Mjpeg), mode(2592, 1944, 60, Mjpeg)];
        assert_eq!(default_mode(&other), Some(mode(800, 600, 25, Mjpeg)));
        // Only H.264: nothing by default.
        assert_eq!(default_mode(&modes[5..]), None);
    }
}
