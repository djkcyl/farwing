//! Official statistics reporting and selected-route observation.
use super::tracks::VideoTrackRegistry;
use crate::diagnostics::performance::PerformanceMonitor;
use crate::features::stream_control::StreamControlHandle;
use crate::transport::rtcp_timing::RtcpTiming;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::stats::StatsReportType;

pub(super) async fn send_official_streamer_statistics(
    channel: Arc<RTCDataChannel>,
    performance: PerformanceMonitor,
) {
    let mut interval = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(30),
        Duration::from_secs(30),
    );
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let Some(payload) = build_official_streamer_statistics(&performance) else {
            continue;
        };
        let serialized = match serde_json::to_string(&payload) {
            Ok(serialized) => serialized,
            Err(error) => {
                tracing::warn!(%error, "serialize official STREAMER statistics failed");
                return;
            }
        };
        let bytes = serialized.len();
        tracing::debug!(%serialized, "local video receiver period statistics");
        if let Err(error) = channel.send_text(serialized).await {
            tracing::debug!(%error, "STREAMER statistics sender stopped");
            return;
        }
        tracing::debug!(
            bytes,
            "sent official 30-second connection_period_stats_event"
        );
    }
}

pub(super) fn build_official_streamer_statistics(
    performance: &PerformanceMonitor,
) -> Option<serde_json::Value> {
    let tracks = performance.video_tracks();
    if !tracks.is_empty() {
        let records: Vec<_> = tracks
            .iter()
            .filter_map(build_official_streamer_statistics)
            .flat_map(|value| {
                value["connection_period_stats_event"]["media_inbounds"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect();
        return Some(
            serde_json::json!({"connection_period_stats_event":{"media_inbounds":records}}),
        );
    }
    let snapshot = performance.snapshot();
    let video_track_index = snapshot.video_track_index?;
    let (packets_received, packets_lost) = performance.streamer_period_totals();
    let mut record = serde_json::Map::new();
    record.insert(
        "video_track_index".into(),
        serde_json::json!(video_track_index),
    );
    record.insert(
        "renderer_impl".into(),
        serde_json::json!("OpenUUYC native viewer"),
    );
    record.insert("decoder_impl".into(), serde_json::json!(snapshot.decoder));

    // The official decoder reads these counters as u32. Keep the UI's wider
    // cumulative counters, but serialize the declared wire representation.
    for (field, value) in [
        ("total_packets_received", packets_received),
        ("total_packets_lost", packets_lost),
        ("total_received_frames", snapshot.total_received_frames),
        ("total_decoded_frames", snapshot.total_decoded_frames),
        (
            "total_actual_rendered_frames",
            snapshot.total_actual_rendered_frames,
        ),
        (
            "total_key_frames_decoded",
            snapshot.total_key_frames_decoded,
        ),
    ] {
        record.insert(field.into(), serde_json::json!(value as u32));
    }
    for (field, value) in [
        (
            "total_retransmitted_packets_recovered",
            snapshot.rtx_packets_accepted,
        ),
        (
            "total_retransmitted_packets_received",
            snapshot.rtx_packets_received,
        ),
        (
            "total_fec_packets_recovered",
            snapshot.fec_packets_recovered,
        ),
        ("total_fec_packets_received", snapshot.fec_packets_received),
        ("total_small_jank_count", snapshot.small_jank_count),
        ("total_jank_count", snapshot.jank_count),
        ("total_big_jank_count", snapshot.big_jank_count),
    ] {
        record.insert(field.into(), serde_json::json!(value));
    }
    for (field, value) in [
        ("avg_decode_fps", snapshot.decode_fps),
        ("avg_render_fps", snapshot.render_fps),
        ("avg_received_fps", snapshot.receive_fps),
    ] {
        insert_stat_integer(&mut record, field, Some(value), i32::MAX as u64);
    }

    if let Some(stats) = &snapshot.pipeline_stats {
        for (field, value) in [
            ("streamer_avg_actual_fps", stats.source_fps),
            ("streamer_avg_actual_received_fps", Some(stats.received_fps)),
            (
                "avg_assembly_ms_interval",
                stats.assembly.map(|v| v.average_ms),
            ),
            ("avg_decode_ms_interval", stats.decode.map(|v| v.average_ms)),
        ] {
            insert_stat_integer(&mut record, field, value, i32::MAX as u64);
        }
        for (field, value) in [
            ("max_assembly_ms_interval", stats.assembly.map(|v| v.max_ms)),
            ("max_decode_ms_interval", stats.decode.map(|v| v.max_ms)),
            (
                "streamer_avg_sending_delay_interval",
                stats.sending.map(|v| v.average_ms),
            ),
            (
                "streamer_max_sending_delay_interval",
                stats.sending.map(|v| v.max_ms),
            ),
            // frame_total_delay is processing + sending + measured RTT, NOT
            // the clock-correlated E2E sample. Do not fabricate its period
            // percentiles from a different distribution.
            (
                "streamer_frame_capture_delay_ms_p50",
                stats.capture.map(|v| v.p50_ms),
            ),
            (
                "streamer_frame_capture_delay_ms_p90",
                stats.capture.map(|v| v.p90_ms),
            ),
            (
                "streamer_frame_encode_delay_ms_p50",
                stats.encode.map(|v| v.p50_ms),
            ),
            (
                "streamer_frame_encode_delay_ms_p90",
                stats.encode.map(|v| v.p90_ms),
            ),
            (
                "streamer_frame_pacer_delay_ms_p50",
                stats.pacer.map(|v| v.p50_ms),
            ),
            (
                "streamer_frame_pacer_delay_ms_p90",
                stats.pacer.map(|v| v.p90_ms),
            ),
            (
                "streamer_frame_transport_delay_ms_p50",
                stats.transport.map(|v| v.p50_ms),
            ),
            (
                "streamer_frame_transport_delay_ms_p90",
                stats.transport.map(|v| v.p90_ms),
            ),
            // The extended frame-timing distribution contains measured
            // (flags & 4) frames only. Our all-frame local assembly/decode
            // diagnostics are already reported above under their own names;
            // they must not impersonate that separate measured distribution.
            (
                "streamer_frame_e2e_delay_ms_avg",
                stats.e2e.map(|v| v.average_ms),
            ),
            (
                "streamer_frame_e2e_delay_ms_p50",
                stats.e2e.map(|v| v.p50_ms),
            ),
            (
                "streamer_frame_e2e_delay_ms_p90",
                stats.e2e.map(|v| v.p90_ms),
            ),
            (
                "streamer_frame_e2e_delay_ms_p99",
                stats.e2e.map(|v| v.p99_ms),
            ),
        ] {
            insert_stat_integer(&mut record, field, value, u32::MAX as u64);
        }
    }
    // Missing fields keep the official defaults. In particular an unmeasured
    // pre/after-switch sample is not a measured zero-loss/zero-latency sample.
    Some(serde_json::json!({"connection_period_stats_event": {"media_inbounds": [record]}}))
}

pub(super) fn insert_stat_integer(
    record: &mut serde_json::Map<String, serde_json::Value>,
    field: &str,
    value: Option<f64>,
    maximum: u64,
) {
    if let Some(value) = value.filter(|v| v.is_finite() && *v >= 0.0 && *v <= maximum as f64) {
        record.insert(field.into(), serde_json::json!(value.trunc() as u64));
    }
}

pub(super) async fn sample_network_performance(
    connection: Arc<RTCPeerConnection>,
    performance: PerformanceMonitor,
    nack_rtt_micros: Arc<AtomicU64>,
    rtcp_timing: RtcpTiming,
    stream_control: StreamControlHandle,
    tracks: VideoTrackRegistry,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pipeline_sample = 0_u8;
    let mut last_candidate_pair = None;
    let mut ice_sample: Option<(String, u64, Instant)> = None;
    loop {
        interval.tick().await;
        let mut selected_candidate_ids = None;
        let mut selected_route = None;
        if let Some(pair) = connection
            .sctp()
            .transport()
            .ice_transport()
            .get_selected_candidate_pair()
            .await
        {
            let route = connection_route(
                pair.local.typ,
                &pair.local.address,
                pair.remote.typ,
                &pair.remote.address,
            );
            let candidate_pair = format!(
                "{}:{} -> {}:{} ({route}; {}/{}; relay {}/{})",
                pair.local.address,
                pair.local.port,
                pair.remote.address,
                pair.remote.port,
                pair.local.protocol,
                pair.remote.protocol,
                pair.local.relay_protocol,
                pair.remote.relay_protocol
            );
            if last_candidate_pair.as_ref() != Some(&candidate_pair) {
                tracing::info!(
                    local_type = %pair.local.typ,
                    remote_type = %pair.remote.typ,
                    local_protocol = %pair.local.protocol,
                    remote_protocol = %pair.remote.protocol,
                    local_generation = pair.local.generation,
                    remote_generation = pair.remote.generation,
                    local_network_id = pair.local.network_id,
                    remote_network_id = pair.remote.network_id,
                    local_network_cost = pair.local.network_cost,
                    remote_network_cost = pair.remote.network_cost,
                    local_relay_protocol = %pair.local.relay_protocol,
                    remote_relay_protocol = %pair.remote.relay_protocol,
                    pair = %candidate_pair,
                    "selected ICE candidate pair changed"
                );
                last_candidate_pair = Some(candidate_pair);
            }
            tracing::trace!(
                local_type = %pair.local.typ,
                local_addr = %format_args!("{}:{}", pair.local.address, pair.local.port),
                remote_type = %pair.remote.typ,
                remote_addr = %format_args!("{}:{}", pair.remote.address, pair.remote.port),
                local_network_id = pair.local.network_id,
                remote_network_id = pair.remote.network_id,
                local_network_cost = pair.local.network_cost,
                remote_network_cost = pair.remote.network_cost,
                route,
                "selected ICE candidate pair"
            );
            performance.set_connection(format!(
                "{} {route}",
                pair.local.protocol.to_string().to_uppercase()
            ));
            selected_candidate_ids = Some((pair.local.stats_id, pair.remote.stats_id));
            selected_route = Some(route);
        }
        let mut delay_seconds = None;
        let mut ice_observed_at = None;
        let rtcp_rtt = rtcp_timing.publish_rtt();
        for report in connection.get_stats().await.reports.into_values() {
            match report {
                StatsReportType::CandidatePair(pair)
                    if selected_candidate_ids
                        .as_ref()
                        .is_some_and(|(local_id, remote_id)| {
                            pair.local_candidate_id == *local_id
                                && pair.remote_candidate_id == *remote_id
                        }) =>
                {
                    let current = pair.current_round_trip_time;
                    let average = (pair.responses_received != 0)
                        .then(|| pair.total_round_trip_time / pair.responses_received as f64);
                    let candidate =
                        (pair.responses_received != 0 && current.is_finite() && current >= 0.0)
                            .then_some(current)
                            .or(average.filter(|value| value.is_finite() && *value >= 0.0));
                    if candidate.is_some() {
                        delay_seconds = candidate;
                        if ice_sample.as_ref().is_none_or(|(id, count, _)| {
                            *id != pair.id || *count != pair.responses_received
                        }) {
                            ice_sample =
                                Some((pair.id.clone(), pair.responses_received, Instant::now()));
                        }
                        ice_observed_at = ice_sample.as_ref().map(|(_, _, at)| *at);
                    }
                }
                StatsReportType::LocalCandidate(stats)
                    if selected_route == Some("relay")
                        && selected_candidate_ids
                            .as_ref()
                            .is_some_and(|(local_id, _)| stats.id == *local_id) =>
                {
                    let transport = match stats.relay_protocol.as_str() {
                        "tls" => "TURNS",
                        "tcp" => "TCP",
                        "udp" => "UDP",
                        _ => "TURN",
                    };
                    performance.set_connection(format!("{transport} relay"));
                }
                _ => {}
            }
        }
        stream_control
            .network_control()
            .observe_rtt(delay_seconds.map(Duration::from_secs_f64));
        performance.set_measured_media_rtt(rtcp_rtt, rtcp_timing.published_rtt_at());
        for track in tracks.all() {
            if let Some(report) = rtcp_timing.receive_loss_for(track.metadata.ssrc) {
                track.performance.record_receive_loss(
                    report.highest_sequence,
                    report.lost_packets,
                    report.observed_at,
                );
            }
            let measured = rtcp_timing.rtt_for(track.metadata.ssrc);
            track.performance.set_measured_media_rtt(
                measured,
                rtcp_timing.rtt_observed_at_for(track.metadata.ssrc),
            );
            if let Some(rtt) = measured {
                track.nack_rtt_micros.store(
                    rtt.as_micros().min(u128::from(u64::MAX)) as u64,
                    Ordering::Relaxed,
                );
            }
        }
        let measured = rtcp_rtt
            .zip(rtcp_timing.published_rtt_at())
            .filter(|(_, at)| at.elapsed() <= Duration::from_secs(5));
        performance.set_current_delay(measured.or_else(|| {
            delay_seconds
                .map(Duration::from_secs_f64)
                .zip(ice_observed_at)
        }));
        if let Some(rtt) = rtcp_rtt {
            nack_rtt_micros.store(
                rtt.as_micros().min(u128::from(u64::MAX)) as u64,
                Ordering::Relaxed,
            );
        }
        stream_control.poll_timeouts();
        pipeline_sample = pipeline_sample.wrapping_add(1);
        if pipeline_sample.is_multiple_of(5) {
            let snapshot = performance.snapshot();
            let pipeline = snapshot.pipeline_stats.as_ref();
            tracing::debug!(
                local_current_ms = format_args!("{:.1}", snapshot.local_frame_delay_ms),
                local_average_ms = format_args!("{:.1}", snapshot.local_frame_delay_average_ms),
                local_p95_ms = format_args!("{:.1}", snapshot.local_frame_delay_p95_ms),
                assembly_ms = format_args!("{:.1}", snapshot.assembly_delay_ms),
                input_queue_ms = format_args!("{:.1}", snapshot.input_queue_delay_ms),
                decode_pipeline_ms = format_args!("{:.1}", snapshot.decode_pipeline_delay_ms),
                surface_ms = format_args!("{:.1}", snapshot.surface_transfer_delay_ms),
                present_wait_ms = format_args!("{:.1}", snapshot.present_wait_delay_ms),
                render_queue_ms = format_args!("{:.1}", snapshot.render_queue_delay_ms),
                ingress_queue = snapshot.ingress_queue_packets,
                outstanding_nacks = snapshot.outstanding_nacks,
                packet_loss_percent = snapshot.packet_loss_percent,
                abandoned_repair_packets = snapshot.abandoned_repair_packets,
                predecode_drops = snapshot.predecode_dropped_frames,
                decoder_queue = snapshot.decoder_queue_frames,
                frame_buffer = snapshot.frame_buffer_frames,
                frame_buffer_peak = snapshot.frame_buffer_peak_frames,
                presentation_queue = snapshot.presentation_queue_frames,
                receive_fps = format_args!("{:.1}", snapshot.receive_fps),
                decode_fps = format_args!("{:.1}", snapshot.decode_fps),
                render_fps = format_args!("{:.1}", snapshot.render_fps),
                actual_fps = format_args!("{:.1}", snapshot.actual_fps),
                actual_frames = snapshot.total_actual_rendered_frames,
                marked_frames = snapshot.total_marked_rendered_frames,
                sender_capture_p50_ms = pipeline.and_then(|p| p.capture).map(|p| p.p50_ms),
                sender_encode_p50_ms = pipeline.and_then(|p| p.encode).map(|p| p.p50_ms),
                sender_pacer_p50_ms = pipeline.and_then(|p| p.pacer).map(|p| p.p50_ms),
                sender_total_average_ms = pipeline.and_then(|p| p.sending).map(|p| p.average_ms),
                transport_p50_ms = pipeline.and_then(|p| p.transport).map(|p| p.p50_ms),
                e2e_average_ms = pipeline.and_then(|p| p.e2e).map(|p| p.average_ms),
                e2e_p90_ms = pipeline.and_then(|p| p.e2e).map(|p| p.p90_ms),
                "media pipeline snapshot"
            );
        }
        if matches!(
            connection.connection_state(),
            RTCPeerConnectionState::Closed | RTCPeerConnectionState::Failed
        ) {
            break;
        }
    }
}

pub(super) fn connection_route(
    local_type: RTCIceCandidateType,
    local_address: &str,
    remote_type: RTCIceCandidateType,
    remote_address: &str,
) -> &'static str {
    if local_type == RTCIceCandidateType::Relay || remote_type == RTCIceCandidateType::Relay {
        "relay"
    } else if (private_route_address(local_address) && private_route_address(remote_address))
        || (local_type == RTCIceCandidateType::Host
            && remote_type == RTCIceCandidateType::Host
            && same_global_ipv6_prefix(local_address, remote_address))
    {
        "LAN"
    } else {
        // Official D66EA0 requires same-/64 global IPv6 for public host/host.
        // Candidate origin alone never establishes LAN.
        "P2P"
    }
}

pub(super) fn same_global_ipv6_prefix(local: &str, remote: &str) -> bool {
    let (Ok(local), Ok(remote)) = (
        local.parse::<std::net::Ipv6Addr>(),
        remote.parse::<std::net::Ipv6Addr>(),
    ) else {
        return false;
    };
    let local = local.octets();
    let remote = remote.octets();
    local[0] & 0xe0 == 0x20 && remote[0] & 0xe0 == 0x20 && local[..8] == remote[..8]
}

pub(super) fn private_route_address(address: &str) -> bool {
    let Ok(address) = address.parse::<IpAddr>() else {
        return false;
    };
    match address {
        IpAddr::V4(address) => {
            address.is_private()
                || address.is_link_local()
                || address.is_loopback()
                || address.octets()[0] == 0
                || address.octets()[0] >= 224
        }
        IpAddr::V6(address) => {
            address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || address.is_unicast_link_local()
                || address.segments()[0] & 0xfe00 == 0xfc00
                || address
                    .to_ipv4_mapped()
                    .is_some_and(|v4| v4.is_private() || v4.is_link_local() || v4.is_loopback())
        }
    }
}
