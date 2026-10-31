//! Camera messages of the session channel (`docs/protocol/camera-design.md`
//! §4.2): the host's cameras in detail, and a participant's parameters and
//! picture adjustments.
use crate::media::camera::{self, Color, Control, ControlState, Matrix, Mode, Params, PixelFormat};

/// The cameras this participant may see (H→C push), whenever they or
/// their parameters change.
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Cameras {
    #[prost(message, repeated, tag = "1")]
    pub(crate) cameras: Vec<CameraInfo>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct CameraInfo {
    /// Its display ID in the display list.
    #[prost(int32, tag = "1")]
    pub(crate) id: i32,
    #[prost(string, tag = "2")]
    pub(crate) name: String,
    /// Why it cannot be opened now; empty when it can.
    #[prost(string, tag = "3")]
    pub(crate) unavailable: String,
    #[prost(message, repeated, tag = "4")]
    pub(crate) modes: Vec<CameraMode>,
    /// The owner's parameters (the mode filled in).
    #[prost(message, optional, tag = "5")]
    pub(crate) owner: Option<CameraParams>,
    /// This participant's own parameters, if it set any.
    #[prost(message, optional, tag = "6")]
    pub(crate) custom: Option<CameraParams>,
    /// The parameters it runs with now, if it runs for this participant.
    #[prost(message, optional, tag = "7")]
    pub(crate) running: Option<CameraParams>,
    /// It runs with another participant's parameters: their name.
    #[prost(string, tag = "8")]
    pub(crate) chosen_by: String,
    #[prost(message, repeated, tag = "9")]
    pub(crate) controls: Vec<CameraControl>,
}

#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct CameraMode {
    #[prost(uint32, tag = "1")]
    pub(crate) width: u32,
    #[prost(uint32, tag = "2")]
    pub(crate) height: u32,
    #[prost(uint32, tag = "3")]
    pub(crate) fps_numerator: u32,
    #[prost(uint32, tag = "4")]
    pub(crate) fps_denominator: u32,
    #[prost(enumeration = "CameraFormat", tag = "5")]
    pub(crate) format: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub(crate) enum CameraFormat {
    Nv12 = 0,
    Yuy2 = 1,
    Mjpeg = 2,
    H264 = 3,
}

/// Camera parameters; zero values are automatic.
#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct CameraParams {
    #[prost(message, optional, tag = "1")]
    pub(crate) mode: Option<CameraMode>,
    /// 0 automatic, 1 BT.601, 2 BT.709.
    #[prost(uint32, tag = "2")]
    pub(crate) matrix: u32,
    /// 0 automatic, 1 limited, 2 full.
    #[prost(uint32, tag = "3")]
    pub(crate) range: u32,
    /// 0: by resolution and rate.
    #[prost(uint32, tag = "4")]
    pub(crate) bitrate_kbps: u32,
    #[prost(bool, tag = "5")]
    pub(crate) mirror: bool,
    #[prost(uint32, tag = "6")]
    pub(crate) quarter_turns: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct CameraControl {
    #[prost(enumeration = "CameraControlKind", tag = "1")]
    pub(crate) control: i32,
    #[prost(sint32, tag = "2")]
    pub(crate) minimum: i32,
    #[prost(sint32, tag = "3")]
    pub(crate) maximum: i32,
    #[prost(sint32, tag = "4")]
    pub(crate) step: i32,
    #[prost(sint32, tag = "5")]
    pub(crate) default: i32,
    #[prost(sint32, tag = "6")]
    pub(crate) value: i32,
    #[prost(bool, tag = "7")]
    pub(crate) can_auto: bool,
    #[prost(bool, tag = "8")]
    pub(crate) auto: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, prost::Enumeration)]
#[repr(i32)]
pub(crate) enum CameraControlKind {
    Brightness = 0,
    Contrast = 1,
    Hue = 2,
    Saturation = 3,
    Sharpness = 4,
    Gamma = 5,
    WhiteBalance = 6,
    BacklightCompensation = 7,
    Gain = 8,
    PowerLineFrequency = 9,
    Exposure = 10,
    Focus = 11,
    Zoom = 12,
}

/// A participant's camera request (C→H) → an empty answer; the change
/// shows in the next [`Cameras`].
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct CameraRequest {
    #[prost(oneof = "CameraOp", tags = "1,2,3")]
    pub(crate) op: Option<CameraOp>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub(crate) enum CameraOp {
    /// Its own parameters for a camera.
    #[prost(message, tag = "1")]
    Params(SetCameraParams),
    /// Back to the owner's parameters (the camera's display ID).
    #[prost(int32, tag = "2")]
    Reset(i32),
    #[prost(message, tag = "3")]
    Adjust(CameraAdjust),
}

#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct SetCameraParams {
    #[prost(int32, tag = "1")]
    pub(crate) camera: i32,
    #[prost(message, optional, tag = "2")]
    pub(crate) params: Option<CameraParams>,
}

#[derive(Clone, Copy, PartialEq, Eq, prost::Message)]
pub(crate) struct CameraAdjust {
    #[prost(int32, tag = "1")]
    pub(crate) camera: i32,
    #[prost(enumeration = "CameraControlKind", tag = "2")]
    pub(crate) control: i32,
    #[prost(sint32, tag = "3")]
    pub(crate) value: i32,
    #[prost(bool, tag = "4")]
    pub(crate) auto: bool,
}

impl From<PixelFormat> for CameraFormat {
    fn from(format: PixelFormat) -> Self {
        match format {
            PixelFormat::Nv12 => Self::Nv12,
            PixelFormat::Yuy2 => Self::Yuy2,
            PixelFormat::Mjpeg => Self::Mjpeg,
            PixelFormat::H264 => Self::H264,
        }
    }
}

impl From<CameraFormat> for PixelFormat {
    fn from(format: CameraFormat) -> Self {
        match format {
            CameraFormat::Nv12 => Self::Nv12,
            CameraFormat::Yuy2 => Self::Yuy2,
            CameraFormat::Mjpeg => Self::Mjpeg,
            CameraFormat::H264 => Self::H264,
        }
    }
}

impl From<Mode> for CameraMode {
    fn from(mode: Mode) -> Self {
        Self {
            width: mode.width,
            height: mode.height,
            fps_numerator: mode.fps_numerator,
            fps_denominator: mode.fps_denominator,
            format: CameraFormat::from(mode.format) as i32,
        }
    }
}

impl CameraMode {
    pub(crate) fn mode(&self) -> Option<Mode> {
        Some(Mode {
            width: self.width,
            height: self.height,
            fps_numerator: self.fps_numerator,
            fps_denominator: self.fps_denominator,
            format: CameraFormat::try_from(self.format).ok()?.into(),
        })
    }
}

impl From<Params> for CameraParams {
    fn from(params: Params) -> Self {
        Self {
            mode: params.mode.map(Into::into),
            matrix: match params.color.matrix {
                None => 0,
                Some(Matrix::Bt601) => 1,
                Some(Matrix::Bt709) => 2,
            },
            range: match params.color.full_range {
                None => 0,
                Some(false) => 1,
                Some(true) => 2,
            },
            bitrate_kbps: params.bitrate_kbps.unwrap_or(0),
            mirror: params.orientation.mirror,
            quarter_turns: u32::from(params.orientation.quarter_turns),
        }
    }
}

impl CameraParams {
    pub(crate) fn params(&self) -> Params {
        Params {
            mode: self.mode.as_ref().and_then(CameraMode::mode),
            color: Color {
                matrix: match self.matrix {
                    1 => Some(Matrix::Bt601),
                    2 => Some(Matrix::Bt709),
                    _ => None,
                },
                full_range: match self.range {
                    1 => Some(false),
                    2 => Some(true),
                    _ => None,
                },
            },
            bitrate_kbps: (self.bitrate_kbps > 0).then(|| {
                self.bitrate_kbps
                    .clamp(*camera::BITRATE_RANGE_KBPS.start(), *camera::BITRATE_RANGE_KBPS.end())
            }),
            orientation: crate::media::geometry::Orientation {
                mirror: self.mirror,
                quarter_turns: (self.quarter_turns % 4) as u8,
            },
        }
    }
}

impl From<Control> for CameraControlKind {
    fn from(control: Control) -> Self {
        match control {
            Control::Brightness => Self::Brightness,
            Control::Contrast => Self::Contrast,
            Control::Hue => Self::Hue,
            Control::Saturation => Self::Saturation,
            Control::Sharpness => Self::Sharpness,
            Control::Gamma => Self::Gamma,
            Control::WhiteBalance => Self::WhiteBalance,
            Control::BacklightCompensation => Self::BacklightCompensation,
            Control::Gain => Self::Gain,
            Control::PowerLineFrequency => Self::PowerLineFrequency,
            Control::Exposure => Self::Exposure,
            Control::Focus => Self::Focus,
            Control::Zoom => Self::Zoom,
        }
    }
}

impl From<CameraControlKind> for Control {
    fn from(kind: CameraControlKind) -> Self {
        match kind {
            CameraControlKind::Brightness => Self::Brightness,
            CameraControlKind::Contrast => Self::Contrast,
            CameraControlKind::Hue => Self::Hue,
            CameraControlKind::Saturation => Self::Saturation,
            CameraControlKind::Sharpness => Self::Sharpness,
            CameraControlKind::Gamma => Self::Gamma,
            CameraControlKind::WhiteBalance => Self::WhiteBalance,
            CameraControlKind::BacklightCompensation => Self::BacklightCompensation,
            CameraControlKind::Gain => Self::Gain,
            CameraControlKind::PowerLineFrequency => Self::PowerLineFrequency,
            CameraControlKind::Exposure => Self::Exposure,
            CameraControlKind::Focus => Self::Focus,
            CameraControlKind::Zoom => Self::Zoom,
        }
    }
}

impl From<ControlState> for CameraControl {
    fn from(state: ControlState) -> Self {
        Self {
            control: CameraControlKind::from(state.control) as i32,
            minimum: state.minimum,
            maximum: state.maximum,
            step: state.step,
            default: state.default,
            value: state.value,
            can_auto: state.can_auto,
            auto: state.auto,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_round_trip_through_the_wire() {
        let params = Params {
            mode: Some(Mode {
                width: 1280,
                height: 720,
                fps_numerator: 30000,
                fps_denominator: 1001,
                format: PixelFormat::Mjpeg,
            }),
            color: Color {
                matrix: Some(Matrix::Bt709),
                full_range: Some(true),
            },
            bitrate_kbps: Some(2_000),
            orientation: crate::media::geometry::Orientation {
                mirror: true,
                quarter_turns: 3,
            },
        };
        assert_eq!(CameraParams::from(params).params(), params);
        assert_eq!(CameraParams::default().params(), Params::default());
        // A ceiling out of range comes back within it.
        let wild = CameraParams {
            bitrate_kbps: 99_000,
            ..Default::default()
        };
        assert_eq!(wild.params().bitrate_kbps, Some(20_000));
    }
}
