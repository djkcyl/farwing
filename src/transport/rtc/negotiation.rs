use anyhow::{Context, Result, ensure};
use std::time::Duration;
use webrtc::api::media_engine::MediaEngine;
// SDP, codec and extension registration; no session ownership.
pub(super) const MAX_DATA_CHANNEL_MESSAGE_SIZE: u32 = 524_288;


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
    farwing_rtc_wire::register_codecs(media_engine).context("register codecs")
}

pub(super) fn register_header_extensions(media_engine: &mut MediaEngine) -> Result<()> {
    farwing_rtc_wire::register_header_extensions(media_engine, &[])
        .context("register RTP header extensions")
}
