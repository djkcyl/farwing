//! `dev video`: a headless controller that decodes the host's screen under
//! injected loss and reports how fast the picture recovers
//! (`docs/protocol/rfi-ltr-brief.md` §6.10).

use crate::application::viewer::dev_video::DecodedObservation;
use crate::transport::rtc::impair::LossEvent;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// How the picture came back after one injected loss.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Recovery {
    /// Loss → first decoded frame newer than the lost one.
    pub recovery: Duration,
    /// Last decoded frame before the loss → that frame, less one interval.
    pub freeze: Duration,
    /// `keyframe`, `recovery` (a frame-meta recovery frame) or `continued`
    /// (the stream went on without either, e.g. only padding was lost).
    pub by: &'static str,
    pub bytes: usize,
}

fn newer(timestamp: u32, than: u32) -> bool {
    (timestamp.wrapping_sub(than) as i32) > 0
}

/// The usual spacing of decoded frames (median).
fn interval(frames: &[DecodedObservation]) -> Duration {
    let mut gaps: Vec<Duration> = frames
        .windows(2)
        .map(|w| w[1].decoded_at.saturating_duration_since(w[0].decoded_at))
        .collect();
    if gaps.is_empty() {
        return Duration::ZERO;
    }
    gaps.sort();
    gaps[gaps.len() / 2]
}

/// Each loss and its recovery; `None` when the picture had not recovered by
/// the end of the run.
pub(crate) fn recoveries(events: &[LossEvent], frames: &[DecodedObservation]) -> Vec<Option<Recovery>> {
    let interval = interval(frames);
    events
        .iter()
        .map(|event| {
            let recovered = frames
                .iter()
                .find(|f| f.decoded_at >= event.at && newer(f.rtp_timestamp, event.rtp_timestamp))?;
            let before = frames
                .iter()
                .rev()
                .find(|f| f.decoded_at < event.at)
                .map_or(event.at, |f| f.decoded_at);
            Some(Recovery {
                recovery: recovered.decoded_at.saturating_duration_since(event.at),
                freeze: recovered
                    .decoded_at
                    .saturating_duration_since(before)
                    .saturating_sub(interval),
                by: if recovered.keyframe {
                    "keyframe"
                } else if recovered.frame_meta.is_some_and(|m| m.recovery()) {
                    "recovery"
                } else {
                    "continued"
                },
                bytes: recovered.bytes,
            })
        })
        .collect()
}

/// Playback smoothness from decode times alone: the spacing of decoded
/// frames, stalls, and how much the delivery delay varies (the delay of each
/// frame against its RTP capture time, so screen content does not matter).
fn smoothness(frames: &[DecodedObservation]) -> Value {
    if frames.len() < 3 {
        return Value::Null;
    }
    let intervals: Vec<f64> = frames
        .windows(2)
        .map(|w| ms(w[1].decoded_at.saturating_duration_since(w[0].decoded_at)))
        .collect();
    let mut sorted = intervals.clone();
    sorted.sort_by(f64::total_cmp);
    let median = sorted[sorted.len() / 2];
    // A stall: a gap well beyond the usual spacing while the sender kept
    // capturing (the RTP time advanced less than the gap).
    let stalls = |limit: f64| {
        frames
            .windows(2)
            .filter(|w| {
                let gap = ms(w[1].decoded_at.saturating_duration_since(w[0].decoded_at));
                let sent = f64::from(w[1].rtp_timestamp.wrapping_sub(w[0].rtp_timestamp)) / 90.0;
                gap >= limit && gap > sent + limit / 2.0
            })
            .count()
    };
    let first = (frames[0].rtp_timestamp, frames[0].decoded_at);
    let delays: Vec<f64> = frames
        .iter()
        .map(|f| {
            let sent = f64::from(f.rtp_timestamp.wrapping_sub(first.0) as i32) / 90.0;
            ms(f.decoded_at.saturating_duration_since(first.1)) - sent
        })
        .collect();
    let floor = delays.iter().cloned().fold(f64::MAX, f64::min);
    let gaps = |limit: f64| intervals.iter().filter(|gap| **gap >= limit).count();
    let frozen: f64 = intervals.iter().filter(|gap| **gap >= 100.0).map(|gap| gap - median).sum();
    json!({
        "median_interval_ms": median,
        "stalls_over_100ms": stalls(100.0),
        "stalls_over_250ms": stalls(250.0),
        "gaps_over_100ms": gaps(100.0),
        "gaps_over_250ms": gaps(250.0),
        "gaps_over_500ms": gaps(500.0),
        "frozen_ms": frozen,
        "interval_ms": percentiles(intervals),
        "delay_spread_ms": percentiles(delays.iter().map(|d| d - floor).collect()),
    })
}

fn percentiles(mut values: Vec<f64>) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    values.sort_by(f64::total_cmp);
    let at = |q: f64| values[((values.len() - 1) as f64 * q).round() as usize];
    json!({
        "p50": at(0.5),
        "p95": at(0.95),
        "p99": at(0.99),
        "max": values[values.len() - 1],
        "mean": values.iter().sum::<f64>() / values.len() as f64,
    })
}

fn ms(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 10_000.0).round() / 10.0
}

pub(crate) struct Run<'a> {
    pub codec: &'a str,
    pub loss: Option<&'a str>,
    pub recovery_declared: bool,
    pub started: Instant,
    pub elapsed: Duration,
    pub events: &'a [LossEvent],
    pub frames: &'a [DecodedObservation],
    pub keyframe_requests: u64,
    pub received_rtp_bytes: u64,
    /// `rate:` rule: packets lost in the bottleneck queue, longest wait.
    pub queue_drops: u64,
    pub queue_max: Duration,
    /// `net:` rule: packets dropped before repair.
    pub network_drops: u64,
}

pub(crate) fn report(run: &Run<'_>) -> Value {
    let recoveries = recoveries(run.events, run.frames);
    let recovered: Vec<&Recovery> = recoveries.iter().flatten().collect();
    let keyframes = run.frames.iter().filter(|f| f.keyframe).count();
    let minutes = run.elapsed.as_secs_f64() / 60.0;
    let count = |by: &str| recovered.iter().filter(|r| r.by == by).count();
    let delta_frames = run.frames.iter().filter(|f| !f.keyframe).map(|f| f.bytes as f64);
    let delta_count = run.frames.iter().filter(|f| !f.keyframe).count();
    json!({
        "codec": run.codec,
        "loss": run.loss,
        "recovery_declared": run.recovery_declared,
        "seconds": ms(run.elapsed) / 1000.0,
        "frames_decoded": run.frames.len(),
        "frames_with_meta": run.frames.iter().filter(|f| f.frame_meta.is_some()).count(),
        "keyframes_decoded": keyframes,
        "keyframes_per_minute": if minutes > 0.0 { keyframes as f64 / minutes } else { 0.0 },
        "keyframe_requests": run.keyframe_requests,
        "received_rtp_bytes": run.received_rtp_bytes,
        "queue_drops": run.queue_drops,
        "network_drops": run.network_drops,
        "max_queue_ms": ms(run.queue_max),
        "mean_delta_frame_bytes": if delta_count > 0 { delta_frames.sum::<f64>() / delta_count as f64 } else { 0.0 },
        "smoothness": smoothness(run.frames),
        "frame_bytes": {
            "delta": percentiles(run.frames.iter().filter(|f| !f.keyframe && !f.frame_meta.is_some_and(|m| m.recovery())).map(|f| f.bytes as f64).collect()),
            "marked": percentiles(run.frames.iter().filter(|f| !f.keyframe && f.frame_meta.is_some_and(|m| m.marks_ltr())).map(|f| f.bytes as f64).collect()),
            "keyframe": percentiles(run.frames.iter().filter(|f| f.keyframe).map(|f| f.bytes as f64).collect()),
        },
        "loss_events": run.events.len(),
        "lost_packets": run.events.iter().map(|e| u64::from(e.packets)).sum::<u64>(),
        "recovered": recovered.len(),
        "unrecovered": recoveries.len() - recovered.len(),
        "recovered_by": {
            "keyframe": count("keyframe"),
            "recovery": count("recovery"),
            "continued": count("continued"),
        },
        "recovery_ms": percentiles(recovered.iter().map(|r| ms(r.recovery)).collect()),
        "freeze_ms": percentiles(recovered.iter().map(|r| ms(r.freeze)).collect()),
        "recovery_frame_bytes": percentiles(recovered.iter().map(|r| r.bytes as f64).collect()),
        // Software decoding: every picture's hash, to compare with another
        // viewer of the same stream (`[epoch, frame_id, hash]`).
        "pictures": run.frames.iter().filter_map(|f| {
            Some(json!([f.frame_meta?.epoch, f.frame_meta?.frame_id, format!("{:016x}", f.picture?)]))
        }).collect::<Vec<_>>(),
        "events": run.events.iter().zip(&recoveries).map(|(event, recovery)| json!({
            "at_ms": ms(event.at.saturating_duration_since(run.started)),
            "packets": event.packets,
            "recovery_ms": recovery.as_ref().map(|r| ms(r.recovery)),
            "freeze_ms": recovery.as_ref().map(|r| ms(r.freeze)),
            "by": recovery.as_ref().map(|r| r.by),
            "bytes": recovery.as_ref().map(|r| r.bytes),
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::frame_meta::{FrameMeta, flags};

    fn frame(start: Instant, ms: u64, timestamp: u32, keyframe: bool, recovery: bool) -> DecodedObservation {
        DecodedObservation {
            rtp_timestamp: timestamp,
            decoded_at: start + Duration::from_millis(ms),
            keyframe,
            frame_meta: Some(FrameMeta {
                flags: if recovery { flags::RECOVERY } else { 0 },
                ..Default::default()
            }),
            bytes: if keyframe { 100_000 } else { 5_000 },
            picture: None,
        }
    }

    #[test]
    fn measures_recovery_and_freeze_per_loss() {
        let start = Instant::now();
        // 60 fps (≈ 16 ms): frame 3 (ts 3000) is lost at 50 ms, a keyframe
        // decodes at 200 ms; frame 10 is lost at 400 ms and a recovery frame
        // decodes at 450 ms; the last loss never recovers.
        let mut frames = vec![
            frame(start, 0, 0, true, false),
            frame(start, 16, 1500, false, false),
            frame(start, 33, 2250, false, false),
            frame(start, 200, 4500, true, false),
            frame(start, 216, 5250, false, false),
            frame(start, 233, 6000, false, false),
            frame(start, 250, 6750, false, false),
            frame(start, 266, 7500, false, false),
        ];
        frames.push(frame(start, 450, 9750, false, true));
        frames.push(frame(start, 466, 10500, false, false));
        let events = [
            LossEvent {
                at: start + Duration::from_millis(50),
                rtp_timestamp: 3000,
                packets: 4,
            },
            LossEvent {
                at: start + Duration::from_millis(400),
                rtp_timestamp: 9000,
                packets: 2,
            },
            LossEvent {
                at: start + Duration::from_millis(480),
                rtp_timestamp: 11250,
                packets: 1,
            },
        ];
        let result = recoveries(&events, &frames);
        assert_eq!(
            result[0],
            Some(Recovery {
                recovery: Duration::from_millis(150),
                // Median interval 17 ms: 200 - 33 - 17.
                freeze: Duration::from_millis(150),
                by: "keyframe",
                bytes: 100_000,
            })
        );
        let second = result[1].as_ref().unwrap();
        assert_eq!((second.recovery, second.by), (Duration::from_millis(50), "recovery"));
        assert_eq!(result[2], None);
        let report = report(&Run {
            codec: "H.264",
            loss: Some("frame:3"),
            recovery_declared: true,
            started: start,
            elapsed: Duration::from_secs(1),
            events: &events,
            frames: &frames,
            keyframe_requests: 1,
            received_rtp_bytes: 0,
            queue_drops: 0,
            queue_max: Duration::ZERO,
            network_drops: 0,
        });
        assert_eq!(report["recovered"], 2);
        assert_eq!(report["unrecovered"], 1);
        assert_eq!(report["recovered_by"]["recovery"], 1);
        assert_eq!(report["recovery_ms"]["max"], 150.0);
    }
}
