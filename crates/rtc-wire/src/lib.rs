//! RTP wire parameters shared by Farwing clients and the server's media
//! forwarder (`docs/protocol/deployment-design.md` §5).
//!
//! The forwarder passes a host's packets to every viewer byte for byte (RS-FEC
//! covers whole packets), so both legs must negotiate the same payload types
//! and header extension IDs. Extension IDs follow registration order; every
//! party registers from these tables.

use webrtc::api::media_engine::{MIME_TYPE_H264, MIME_TYPE_HEVC, MIME_TYPE_OPUS, MediaEngine};
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTCRtpHeaderExtensionCapability, RTPCodecType,
};

pub const TRANSPORT_CC_URI: &str =
    "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01";
pub const MID_URI: &str = "urn:ietf:params:rtp-hdrext:sdes:mid";
pub const VIDEO_TIMING_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/video-timing";
pub const SENDING_DELAY_URI: &str = "urn:farwing:rtp-hdrext:sending-delay";
pub const COLOR_SPACE_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/color-space";
pub const CAPTURE_INDEX_URI: &str = "urn:farwing:rtp-hdrext:capture-index";
pub const NEW_FRAME_URI: &str = "urn:farwing:rtp-hdrext:new-frame";
pub const FRAME_META_URI: &str = "urn:farwing:rtp-hdrext:frame-meta";
/// Offered by browsers only; registered last by senders.
pub const DEPENDENCY_DESCRIPTOR_URI: &str =
    "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension";

/// RTP payload format name of Farwing RS-FEC repair packets.
pub const RSFEC_MIME_TYPE: &str = webrtc::RSFEC_MIME_TYPE;
pub const RSFEC_FMTP: &str = "max-k=109;repair-window=10000000;rtx-as-source=1";
pub const OPUS_FMTP: &str = "minptime=10;stereo=1;useinbandfec=1";
pub const OPUS_PAYLOAD_TYPE: u8 = 111;

pub const AUDIO_EXTENSIONS: [&str; 2] = [TRANSPORT_CC_URI, MID_URI];
/// Only what a Farwing host sends and a viewer reads. Eight IDs keep every
/// packet on the one-byte extension header (IDs 1..=14).
pub const VIDEO_EXTENSIONS: [&str; 8] = [
    TRANSPORT_CC_URI,
    MID_URI,
    VIDEO_TIMING_URI,
    SENDING_DELAY_URI,
    COLOR_SPACE_URI,
    CAPTURE_INDEX_URI,
    NEW_FRAME_URI,
    FRAME_META_URI,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Feedback {
    /// A primary video codec: REMB, transport-cc, FIR, NACK, PLI, RRTR.
    Primary,
    /// RTX of a primary codec: NACK, PLI, transport-cc.
    Repair,
    /// RTX of RED: NACK.
    Nack,
    None,
}

#[derive(Clone, Copy, Debug)]
pub struct VideoCodec {
    pub mime_type: &'static str,
    pub payload_type: u8,
    pub fmtp: &'static str,
    pub feedback: Feedback,
}

const fn codec(
    mime_type: &'static str,
    payload_type: u8,
    fmtp: &'static str,
    feedback: Feedback,
) -> VideoCodec {
    VideoCodec {
        mime_type,
        payload_type,
        fmtp,
        feedback,
    }
}

pub const VIDEO_CODECS: [VideoCodec; 9] = [
    codec("video/AV1", 104, "profile=0;level-idx=17;tier=0", Feedback::Primary),
    codec("video/rtx", 105, "apt=104", Feedback::Repair),
    codec("video/AV1", 106, "profile=1;level-idx=17;tier=0", Feedback::Primary),
    codec("video/rtx", 107, "apt=106", Feedback::Repair),
    codec(MIME_TYPE_HEVC, 96, "", Feedback::Primary),
    codec("video/rtx", 97, "apt=96", Feedback::Repair),
    codec(
        MIME_TYPE_H264,
        98,
        "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
        Feedback::Primary,
    ),
    codec("video/rtx", 99, "apt=98", Feedback::Repair),
    codec(RSFEC_MIME_TYPE, 36, RSFEC_FMTP, Feedback::None),
];

fn feedback(kind: Feedback) -> Vec<RTCPFeedback> {
    let entry = |typ: &str, parameter: &str| RTCPFeedback {
        typ: typ.to_owned(),
        parameter: parameter.to_owned(),
    };
    match kind {
        Feedback::Primary => vec![
            entry("goog-remb", ""),
            entry("transport-cc", ""),
            entry("ccm", "fir"),
            entry("nack", ""),
            entry("nack", "pli"),
            entry("rrtr", ""),
        ],
        Feedback::Repair => vec![
            entry("nack", ""),
            entry("nack", "pli"),
            entry("transport-cc", ""),
        ],
        Feedback::Nack => vec![entry("nack", "")],
        Feedback::None => Vec::new(),
    }
}

/// The RTX payload type repairing `payload_type`.
pub fn rtx_payload_type(payload_type: u8) -> Option<u8> {
    let apt = format!("apt={payload_type}");
    VIDEO_CODECS
        .iter()
        .find(|c| c.mime_type == "video/rtx" && c.fmtp == apt)
        .map(|c| c.payload_type)
}

/// Opus and every video codec of [`VIDEO_CODECS`].
pub fn register_codecs(media: &mut MediaEngine) -> webrtc::error::Result<()> {
    media.register_codec(
        RTCRtpCodecParameters {
            capability: RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48_000,
                channels: 2,
                sdp_fmtp_line: OPUS_FMTP.to_owned(),
                rtcp_feedback: vec![RTCPFeedback {
                    typ: "transport-cc".to_owned(),
                    parameter: String::new(),
                }],
            },
            payload_type: OPUS_PAYLOAD_TYPE,
            ..Default::default()
        },
        RTPCodecType::Audio,
    )?;
    for codec in VIDEO_CODECS {
        media.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: codec.mime_type.to_owned(),
                    clock_rate: 90_000,
                    channels: 0,
                    sdp_fmtp_line: codec.fmtp.to_owned(),
                    rtcp_feedback: feedback(codec.feedback),
                },
                payload_type: codec.payload_type,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
    }
    Ok(())
}

/// [`AUDIO_EXTENSIONS`], then [`VIDEO_EXTENSIONS`] and `video_extra`.
pub fn register_header_extensions(
    media: &mut MediaEngine,
    video_extra: &[&str],
) -> webrtc::error::Result<()> {
    let audio = AUDIO_EXTENSIONS.iter().map(|uri| (RTPCodecType::Audio, *uri));
    let video = VIDEO_EXTENSIONS
        .iter()
        .chain(video_extra)
        .map(|uri| (RTPCodecType::Video, *uri));
    for (kind, uri) in audio.chain(video) {
        media.register_header_extension(
            RTCRtpHeaderExtensionCapability {
                uri: uri.to_owned(),
            },
            kind,
            None,
        )?;
    }
    Ok(())
}

/// The forwarder's layer report to the host (RTCP APP, RFC 3550 §6.7, name
/// `FWLO`), sent with its limit on SSRC 0: the viewer sessions whose
/// downlinks cannot carry the main stream and get the low layer
/// (`docs/protocol/deployment-design.md` §5.5).
pub mod layers {
    use webrtc::rtcp::raw_packet::RawPacket;

    const NAME: &[u8; 4] = b"FWLO";
    const APP: u8 = 204;

    /// The report naming `sessions`.
    pub fn report(sessions: &[String]) -> RawPacket {
        let mut body = sessions.join("\n").into_bytes();
        while body.len() % 4 != 0 {
            body.push(0);
        }
        let words = (8 + body.len()) / 4;
        let mut packet = Vec::with_capacity(4 + words * 4);
        packet.extend_from_slice(&[0x80, APP]);
        packet.extend_from_slice(&(words as u16).to_be_bytes());
        packet.extend_from_slice(&0u32.to_be_bytes());
        packet.extend_from_slice(NAME);
        packet.extend_from_slice(&body);
        RawPacket(packet.into())
    }

    /// The sessions a report names; `None` for other packets.
    pub fn parse(packet: &RawPacket) -> Option<Vec<String>> {
        let bytes = &packet.0;
        if bytes.len() < 12 || bytes[1] != APP || &bytes[8..12] != NAME {
            return None;
        }
        let text = String::from_utf8_lossy(&bytes[12..]);
        Some(
            text.trim_end_matches('\0')
                .split('\n')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect(),
        )
    }

    #[cfg(test)]
    mod tests {
        use webrtc::util::marshal::{Marshal, Unmarshal};

        #[test]
        fn a_report_survives_the_wire() {
            let sessions = vec!["a1b2".to_owned(), "c3d4e".to_owned()];
            let wire = super::report(&sessions).marshal().unwrap();
            assert_eq!(wire.len() % 4, 0);
            let packets = webrtc::rtcp::packet::unmarshal(&mut wire.clone()).unwrap();
            let raw = packets[0]
                .as_any()
                .downcast_ref::<webrtc::rtcp::raw_packet::RawPacket>()
                .unwrap();
            assert_eq!(super::parse(raw), Some(sessions));
            let empty = super::report(&[]).marshal().unwrap();
            let raw = webrtc::rtcp::raw_packet::RawPacket::unmarshal(&mut empty.clone()).unwrap();
            assert_eq!(super::parse(&raw), Some(Vec::new()));
        }
    }
}
