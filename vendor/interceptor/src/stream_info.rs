use crate::Attributes;

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtcpMode {
    Off,
    #[default]
    Compound,
    ReducedSize,
}

/// Negotiated receive-module settings. Updating them must not reset RTP
/// reception statistics or replace the stream's interceptors.
#[derive(Default, Debug, Clone, Copy)]
pub struct ReceiveRtcpParameters {
    pub mode: RtcpMode,
    pub local_ssrc: Option<u32>,
}

/// RTPHeaderExtension represents a negotiated RFC5285 RTP header extension.
#[derive(Default, Debug, Clone)]
pub struct RTPHeaderExtension {
    pub uri: String,
    pub id: isize,
}

/// StreamInfo is the Context passed when a StreamLocal or StreamRemote has been Binded or Unbinded
#[derive(Default, Debug, Clone)]
pub struct StreamInfo {
    pub id: String,
    pub attributes: Attributes,
    pub ssrc: u32,
    pub payload_type: u8,
    pub rtp_header_extensions: Vec<RTPHeaderExtension>,
    pub mime_type: String,
    pub clock_rate: u32,
    /// Negotiated RTP clock rates by payload type; the first codec is not
    /// necessarily the codec received on this SSRC.
    pub payload_clock_rates: Vec<(u8, u32)>,
    pub channels: u16,
    pub sdp_fmtp_line: String,
    pub rtcp_feedback: Vec<RTCPFeedback>,
    pub associated_stream: Option<AssociatedStreamInfo>,
    pub receiver_rtcp: Option<ReceiveRtcpParameters>,
}

/// AssociatedStreamInfo provides a mapping from an auxiliary stream (RTX, FEC,
/// etc.) back to the original stream.
#[derive(Default, Debug, Clone)]
pub struct AssociatedStreamInfo {
    pub ssrc: u32,
    pub payload_type: u8,
}

/// RTCPFeedback signals the connection to use additional RTCP packet types.
/// <https://draft.ortc.org/#dom-rtcrtcpfeedback>
#[derive(Default, Debug, Clone)]
pub struct RTCPFeedback {
    /// Type is the type of feedback.
    /// see: <https://draft.ortc.org/#dom-rtcrtcpfeedback>
    /// valid: ack, ccm, nack, goog-remb, transport-cc
    pub typ: String,

    /// The parameter value depends on the type.
    /// For example, type="nack" parameter="pli" will send Picture Loss Indicator packets.
    pub parameter: String,
}
