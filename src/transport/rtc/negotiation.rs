use anyhow::{Context, Result, ensure};
use std::time::Duration;
use webrtc::api::media_engine::{MIME_TYPE_H264, MIME_TYPE_HEVC, MIME_TYPE_OPUS, MediaEngine};
use webrtc::rtp_transceiver::RTCPFeedback;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTCRtpHeaderExtensionCapability, RTPCodecType,
};
// SDP, codec and extension registration; no session ownership.
pub(super) const MAX_DATA_CHANNEL_MESSAGE_SIZE: u32 = 524_288;

pub(super) const VIDEO_CONTENT_TYPE_URI: &str =
    "http://www.webrtc.org/experiments/rtp-hdrext/video-content-type";

pub(super) const VIDEO_CAPTURE_INDEX_URI: &str =
    "urn:farwing:rtp-hdrext:capture-index";

pub(crate) const VIDEO_IS_NEW_FRAME_URI: &str =
    "urn:farwing:rtp-hdrext:new-frame";

pub(super) const VIDEO_TIMING_URI: &str =
    "http://www.webrtc.org/experiments/rtp-hdrext/video-timing";

pub(super) const VIDEO_FRAME_SENDING_DELAY_URI: &str =
    "urn:farwing:rtp-hdrext:sending-delay";

pub(super) const RTP_STREAM_ID_URI: &str = "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id";

pub(super) const REPAIRED_RTP_STREAM_ID_URI: &str =
    "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id";

pub(super) fn candidate_is_relay(candidate: &str) -> bool {
    candidate
        .split_ascii_whitespace()
        .collect::<Vec<_>>()
        .windows(2)
        .any(|fields| {
            fields[0].eq_ignore_ascii_case("typ") && fields[1].eq_ignore_ascii_case("relay")
        })
}

pub(super) fn remove_relay_candidates_from_sdp(sdp: &str) -> String {
    let separator = if sdp.contains("\r\n") { "\r\n" } else { "\n" };
    let mut filtered = sdp
        .lines()
        .filter(|line| !line.starts_with("a=candidate:") || !candidate_is_relay(line))
        .collect::<Vec<_>>()
        .join(separator);
    if sdp.ends_with(separator) {
        filtered.push_str(separator);
    }
    filtered
}

// Local send-controller bootstrap, not an incoming video bitrate limit.
// The DataRate parser treats a bare `start:8100` trial value as kbps.
// Incoming-video feedback retains this transport bootstrap. The microphone's
// fixed 100 kbps encoding allocation is not a measured path-capacity estimate.
// There is no outgoing video/probe pacer supplying an evolving GCC estimate.
pub(super) const LOCAL_SEND_START_BPS: u64 = 8_100_000;

pub(super) fn transport_feedback_interval(send_bitrate_bps: u64) -> Duration {
    // A 68-byte feedback budget at 5% of the send-side estimate, bounded to
    // 25..250 ms. The host's congestion controller reacts within one
    // feedback interval, so a live stream gets feedback every 25 ms.
    let feedback_bps = send_bitrate_bps / 20;
    let micros = (68_u64 * 8 * 1_000_000)
        .checked_div(feedback_bps)
        .unwrap_or(250_000)
        .clamp(25_000, 250_000);
    Duration::from_micros(micros)
}

pub(crate) fn apply_application_attributes_for_role(
    sdp: &mut String,
    streams: &str,
    mixed_kcp: Option<u8>,
) -> Result<()> {
    const EXTMAP_ALLOW_MIXED: &str = "a=extmap-allow-mixed";
    let msid_semantic = format!("a=msid-semantic: WMS {streams}");
    let max_message_size = format!("a=max-message-size:{MAX_DATA_CHANNEL_MESSAGE_SIZE}");

    let uses_crlf = sdp.contains("\r\n");
    let mut lines = sdp.lines().map(str::to_owned).collect::<Vec<_>>();
    lines.retain(|line| !line.starts_with("a=msid-semantic:"));
    let first_media = lines
        .iter()
        .position(|line| line.starts_with("m="))
        .context("controller SDP has no media sections")?;
    let session_insert = lines[..first_media]
        .iter()
        .position(|line| line.starts_with("a=group:BUNDLE"))
        .map_or(first_media, |index| index + 1);
    for value in [EXTMAP_ALLOW_MIXED, &msid_semantic].into_iter().rev() {
        if !lines[..first_media].iter().any(|line| line == value) {
            lines.insert(session_insert, value.to_owned());
        }
    }
    let application = lines
        .iter()
        .position(|line| line.starts_with("m=application "))
        .context("controller SDP has no application media section")?;
    let end = lines[application + 1..]
        .iter()
        .position(|line| line.starts_with("m="))
        .map_or(lines.len(), |offset| application + 1 + offset);

    for index in (application + 1..end)
        .filter(|index| {
            lines[*index] == "a=sendrecv" || lines[*index].starts_with("a=x-farwing-fct:")
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        lines.remove(index);
    }
    let end = lines[application + 1..]
        .iter()
        .position(|line| line.starts_with("m="))
        .map_or(lines.len(), |offset| application + 1 + offset);
    let insert_at = lines[application + 1..end]
        .iter()
        .position(|line| line.starts_with("a=sctp-port:"))
        .map_or(end, |offset| application + 2 + offset);
    let mixed_kcp = mixed_kcp.map(|version| format!("a=x-farwing-fct:{version}"));
    for value in std::iter::once(max_message_size.as_str())
        .chain(mixed_kcp.as_deref())
        .rev()
    {
        if !lines[application + 1..end].iter().any(|line| line == value) {
            lines.insert(insert_at, value.to_owned());
        }
    }

    let separator = if uses_crlf { "\r\n" } else { "\n" };
    *sdp = lines.join(separator);
    sdp.push_str(separator);
    Ok(())
}

pub(crate) fn negotiated_mixed_kcp_version(sdp: &str) -> Result<Option<u8>> {
    let Some(value) = sdp
        .lines()
        .find_map(|line| line.strip_prefix("a=x-farwing-fct:"))
    else {
        return Ok(None);
    };
    let version = value
        .trim()
        .parse::<u8>()
        .context("invalid FCT version")?;
    if version == 0 {
        return Ok(None);
    }
    let supported = crate::transport::fct::FCT_VERSION;
    ensure!(version >= supported, "unsupported FCT version {version}");
    Ok(Some(supported))
}

pub(super) fn register_codecs(media_engine: &mut MediaEngine) -> Result<()> {
    for (mime_type, payload_type, clock_rate, channels, fmtp, transport_cc) in [(
        MIME_TYPE_OPUS,
        111,
        48_000,
        2,
        "minptime=10;stereo=1;useinbandfec=1",
        true,
    )] {
        media_engine
            .register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: mime_type.to_owned(),
                        clock_rate,
                        channels,
                        sdp_fmtp_line: fmtp.to_owned(),
                        rtcp_feedback: transport_cc
                            .then(|| RTCPFeedback {
                                typ: "transport-cc".to_owned(),
                                parameter: String::new(),
                            })
                            .into_iter()
                            .collect(),
                    },
                    payload_type,
                    ..Default::default()
                },
                RTPCodecType::Audio,
            )
            .with_context(|| format!("register audio codec {mime_type}/{payload_type}"))?;
    }

    let primary_feedback = vec![
        RTCPFeedback {
            typ: "goog-remb".to_owned(),
            parameter: String::new(),
        },
        RTCPFeedback {
            typ: "transport-cc".to_owned(),
            parameter: String::new(),
        },
        RTCPFeedback {
            typ: "ccm".to_owned(),
            parameter: "fir".to_owned(),
        },
        RTCPFeedback {
            typ: "nack".to_owned(),
            parameter: String::new(),
        },
        RTCPFeedback {
            typ: "nack".to_owned(),
            parameter: "pli".to_owned(),
        },
        RTCPFeedback {
            typ: "rrtr".to_owned(),
            parameter: String::new(),
        },
    ];
    let repair_feedback = vec![
        RTCPFeedback {
            typ: "nack".to_owned(),
            parameter: String::new(),
        },
        RTCPFeedback {
            typ: "nack".to_owned(),
            parameter: "pli".to_owned(),
        },
        RTCPFeedback {
            typ: "transport-cc".to_owned(),
            parameter: String::new(),
        },
    ];
    for (mime_type, payload_type, fmtp, rtcp_feedback) in [
        (
            "video/AV1",
            104,
            "profile=0;level-idx=17;tier=0",
            primary_feedback.clone(),
        ),
        ("video/rtx", 105, "apt=104", repair_feedback.clone()),
        (
            "video/AV1",
            106,
            "profile=1;level-idx=17;tier=0",
            primary_feedback.clone(),
        ),
        ("video/rtx", 107, "apt=106", repair_feedback.clone()),
        (MIME_TYPE_HEVC, 96, "", primary_feedback.clone()),
        ("video/rtx", 97, "apt=96", repair_feedback.clone()),
        (
            MIME_TYPE_H264,
            98,
            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
            primary_feedback,
        ),
        ("video/rtx", 99, "apt=98", repair_feedback),
        ("video/red", 100, "", Vec::new()),
        (
            "video/rtx",
            101,
            "apt=100",
            vec![RTCPFeedback {
                typ: "nack".to_owned(),
                parameter: String::new(),
            }],
        ),
        ("video/ulpfec", 102, "", Vec::new()),
        ("video/flexfec-03", 35, "repair-window=10000000", Vec::new()),
        (
            crate::transport::rsfec::MIME_TYPE,
            36,
            "max-k=109;repair-window=10000000;rtx-as-source=1",
            Vec::new(),
        ),
    ] {
        media_engine
            .register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: mime_type.to_owned(),
                        clock_rate: 90_000,
                        channels: 0,
                        sdp_fmtp_line: fmtp.to_owned(),
                        rtcp_feedback,
                    },
                    payload_type,
                    ..Default::default()
                },
                RTPCodecType::Video,
            )
            .with_context(|| format!("register video codec {mime_type}/{payload_type}"))?;
    }
    Ok(())
}

pub(super) fn register_header_extensions(media_engine: &mut MediaEngine) -> Result<()> {
    // Only what a Farwing host sends and this receiver reads. Eight IDs keep
    // every packet on the one-byte extension header (IDs 1..=14). Send time
    // is covered by transport-cc; the playout buffer is decided locally.
    const AUDIO_EXTENSIONS: [&str; 2] = [
        "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
        "urn:ietf:params:rtp-hdrext:sdes:mid",
    ];
    const VIDEO_EXTENSIONS: [&str; 8] = [
        "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
        "urn:ietf:params:rtp-hdrext:sdes:mid",
        "http://www.webrtc.org/experiments/rtp-hdrext/video-timing",
        "urn:farwing:rtp-hdrext:sending-delay",
        crate::media::video_color::COLOR_SPACE_URI,
        "urn:farwing:rtp-hdrext:capture-index",
        VIDEO_IS_NEW_FRAME_URI,
        crate::media::frame_meta::URI,
    ];
    for (kind, uris) in [
        (RTPCodecType::Audio, AUDIO_EXTENSIONS.as_slice()),
        (RTPCodecType::Video, VIDEO_EXTENSIONS.as_slice()),
    ] {
        for uri in uris {
            media_engine
                .register_header_extension(
                    RTCRtpHeaderExtensionCapability {
                        uri: (*uri).to_owned(),
                    },
                    kind,
                    None,
                )
                .with_context(|| format!("register RTP header extension {uri}"))?;
        }
    }
    Ok(())
}
