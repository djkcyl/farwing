//! WebRTC transport. Submodules own one transport responsibility each.

mod channels;
mod clock;
mod core;
mod feedback;
mod forward;
pub(crate) mod impair;
mod ingress;
mod negotiation;
mod peer;
mod receive;
pub(crate) mod recovery;
mod statistics;
mod tracks;
mod workers;

pub use channels::DATA_CHANNEL_LABELS;
#[cfg(windows)]
pub(crate) use core::ConnectionCore;
pub(crate) use statistics::connection_route;
#[cfg(windows)]
pub(crate) use negotiation::negotiated_mixed_kcp_version;
pub use peer::{IceServer, NativePeer};
pub(crate) use tracks::{
    EncodedVideoFrame, FrameSenderTiming, PlayoutDelay, VideoFrameSink, VideoReceiverFeedback,
};
#[cfg(windows)]
pub(crate) use tracks::VideoTrackSource;
pub use tracks::{ForwardedTrack, MediaKind, RtpForwardConfig, RtpForwarder};

pub(crate) const VIDEO_TRACK_COUNT: usize = 5;
