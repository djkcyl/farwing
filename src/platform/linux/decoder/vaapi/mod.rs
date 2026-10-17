//! VA-API hardware decode for Linux.
//!
//! Only the codecs this client negotiates are wired up: H.264 today, with the
//! session enum kept so HEVC slots in beside it.
mod avc;
mod display;
mod output;
mod ref_list;

use cros_libva::VAProfile;

use crate::media::VideoCodec;
use crate::media::decode_api::{DecodeError, VideoDecoderConfig};

pub(super) use avc::{Frame, FrameData};

/// The shared VA display, for the host encoder that opens its own config on it.
pub(crate) fn shared_display() -> Result<std::rc::Rc<cros_libva::Display>, DecodeError> {
    display::display_handle()
}

/// Whether the driver offers an H.264 encode entrypoint.
pub(crate) fn encoder_available() -> bool {
    display::supports_encode()
}

pub(super) struct Session {
    codec: Codec,
    /// The parameter sets from the negotiation, prepended to the first access
    /// unit so the parser can activate an SPS/PPS before the first slice.
    extra: Option<bytes::Bytes>,
}

enum Codec {
    H264(Box<avc::Avc>),
}

impl Session {
    pub(super) fn open(config: &VideoDecoderConfig) -> Result<Self, DecodeError> {
        if config.width == 0 || config.height == 0 || config.width > 16384 || config.height > 16384
        {
            return Err(DecodeError::InvalidInput);
        }
        if !config.extra_data.is_empty()
            && !config.extra_data.starts_with(&[0, 0, 1])
            && !config.extra_data.starts_with(&[0, 0, 0, 1])
        {
            return Err(DecodeError::Unsupported);
        }
        match config.codec {
            VideoCodec::H264 => {
                // The profile the stream actually uses is only known once its
                // SPS arrives; this rejects drivers that decode no H.264 at all.
                if !probe(VideoCodec::H264, config.width, config.height, 8, 1) {
                    return Err(DecodeError::Unsupported);
                }
                Ok(Self {
                    codec: Codec::H264(Box::new(avc::Avc::new())),
                    extra: Some(config.extra_data.clone()),
                })
            }
            // H.265 and AV1 slice parameters are not written for VA-API yet.
            VideoCodec::H265 | VideoCodec::Av1 => Err(DecodeError::Unsupported),
        }
    }

    pub(super) fn decode(
        &mut self,
        payload: &[u8],
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Frame, DecodeError> {
        let extra = self.extra.take().filter(|extra| !extra.is_empty());
        let combined;
        let data = match extra {
            Some(extra) => {
                combined = [extra.as_ref(), payload].concat();
                &combined[..]
            }
            None => payload,
        };
        match &mut self.codec {
            Codec::H264(session) => session.decode(data, cancel),
        }
    }

    pub(super) fn reset(&mut self) {
        match &mut self.codec {
            Codec::H264(session) => session.reset(),
        }
    }
}

/// Whether the local driver decodes this codec at this size in hardware.
pub(super) fn probe(codec: VideoCodec, width: u32, height: u32, depth: u8, chroma: u8) -> bool {
    if depth != 8 || chroma != 1 {
        // 10-bit and 4:4:4 need surface formats this backend does not read back.
        return false;
    }
    let profiles: &[VAProfile::Type] = match codec {
        VideoCodec::H264 => &[
            VAProfile::VAProfileH264High,
            VAProfile::VAProfileH264Main,
            VAProfile::VAProfileH264ConstrainedBaseline,
        ],
        _ => return false,
    };
    profiles
        .iter()
        .any(|profile| display::supports(*profile, width, height))
}
