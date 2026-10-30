//! Bounded, per-viewer UI history. Does not alter receiver sampling or scheduling.
use super::hud::format_resolution;
use super::hud::format_uptime;
use super::hud::health_color;
use crate::diagnostics::performance::PerformanceMonitor;
use crate::diagnostics::performance::PerformanceSnapshot;
use crate::diagnostics::performance::health::Metric;
use crate::i18n::t;
use crate::ui::{controls, theme};
use egui::{RichText, Ui};
use std::{collections::VecDeque, time::Duration};

const HISTORY_SECONDS: f64 = 30.0;
const SAMPLE_SECONDS: f64 = 0.25;

#[derive(Clone)]
struct Sample {
    at: f64,
    values: [Option<f64>; 3],
}

#[derive(Clone, Default)]
struct Panel {
    identity: usize,
    track: Option<u64>,
    samples: VecDeque<Sample>,
}

impl Panel {
    fn sample(&mut self, identity: usize, s: &PerformanceSnapshot) {
        let at = s.uptime.as_secs_f64();
        if self.identity != identity
            || self.track != s.video_track_index
            || self.samples.back().is_some_and(|sample| at < sample.at)
        {
            self.samples.clear();
            self.identity = identity;
            self.track = s.video_track_index;
        }
        if self
            .samples
            .back()
            .is_some_and(|sample| at - sample.at < SAMPLE_SECONDS)
        {
            return;
        }
        self.samples.push_back(Sample {
            at,
            values: [
                Some(s.actual_fps),
                s.health.value(Metric::Rtt),
                Some(s.bitrate_mbps),
            ]
            .map(|value| value.filter(|value| value.is_finite() && *value >= 0.0)),
        });
        while self
            .samples
            .front()
            .is_some_and(|sample| at - sample.at > HISTORY_SECONDS)
            || self.samples.len() > 121
        {
            self.samples.pop_front();
        }
    }

    fn trace(
        &self,
        ui: &mut Ui,
        index: usize,
        label: &str,
        unit: &str,
        minimum_scale: f64,
        decimals: usize,
        current: Option<f64>,
        limit: Option<f64>,
        current_color: egui::Color32,
        now: f64,
    ) {
        let points: Vec<_> = self
            .samples
            .iter()
            .map(|sample| (sample.at, sample.values[index]))
            .collect();
        controls::performance_trace(
            ui,
            now,
            controls::PerformanceTrace {
                current_color,
                label,
                unit,
                current,
                limit,
                minimum_scale,
                decimals,
                points: &points,
            },
        );
    }
}

pub(super) fn show(
    ctx: &egui::Context,
    monitor: &PerformanceMonitor,
    s: &PerformanceSnapshot,
    id: &'static str,
) {
    let key = egui::Id::new((ctx.viewport_id(), id, "performance-trends"));
    let mut panel = ctx.data_mut(|d| d.get_temp::<Panel>(key).unwrap_or_default());
    panel.sample(monitor.history_identity(), s);
    ctx.request_repaint_after(Duration::from_millis(250));
    let width = theme::PERFORMANCE_WIDTH.min((ctx.content_rect().width() - 56.0).max(340.0));
    egui::Window::new(t().viewer.performance_monitor)
        .id(egui::Id::new((id, "monitor")))
        .anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, -12.0])
        .default_width(width)
        .resizable(false)
        .collapsible(false)
        .title_bar(false)
        .frame(controls::performance_frame())
        .show(ctx, |ui| {
            controls::configure(ui.style_mut(), theme::COMPACT_HEIGHT);
            ui.set_width(width);
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 4.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(t().viewer.performance_monitor)
                        .size(theme::COMPACT_TEXT)
                        .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format_uptime(s.uptime))
                            .monospace()
                            .size(theme::SMALL)
                            .color(theme::MUTED),
                    );
                });
            });
            ui.label(
                RichText::new(format!(
                    "{}   ·   {}   ·   {}",
                    s.connection,
                    format_resolution(s.decoded_resolution),
                    s.video_codec,
                ))
                .size(theme::SMALL)
                .color(theme::MUTED),
            );
            ui.add_space(theme::MENU_GROUP_GAP);
            egui::ScrollArea::vertical()
                .id_salt((id, "monitor-content"))
                .max_height((ctx.content_rect().height() - 138.0).clamp(160.0, 680.0))
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    controls::performance_header(ui);
                    let now = s.uptime.as_secs_f64();
                    let t = &t().viewer;
                    panel.trace(
                        ui,
                        0,
                        t.frame_updates,
                        "FPS",
                        30.0,
                        1,
                        Some(s.actual_fps),
                        None,
                        super::hud::frame_rate_color(s),
                        now,
                    );
                    panel.trace(
                        ui,
                        1,
                        t.network_rtt,
                        "ms",
                        5.0,
                        2,
                        s.health.value(Metric::Rtt),
                        None,
                        health_color(s.health.level(Metric::Rtt)),
                        now,
                    );
                    panel.trace(
                        ui,
                        2,
                        t.receive_bitrate,
                        "Mbps",
                        1.0,
                        2,
                        Some(s.bitrate_mbps),
                        // The server's egress budget while relayed.
                        (s.server_budget_kbps > 0).then(|| s.server_budget_kbps as f64 / 1000.0),
                        theme::TEXT,
                        now,
                    );
                    ui.add_space(theme::MENU_GROUP_GAP);
                    ui.columns(2, |columns| {
                        health_metric(&mut columns[0], s, Metric::LocalDelay);
                        health_metric(&mut columns[0], s, Metric::Jitter);
                        health_metric(&mut columns[1], s, Metric::Loss);
                        controls::metric_pair(
                            &mut columns[1],
                            t.recent_frame_drops,
                            s.health
                                .recent_drop_percent
                                .map_or_else(|| "—".into(), |v| format!("{v:.2}%")),
                        );
                    });
                    ui.add_space(theme::MENU_GROUP_GAP);
                    egui::CollapsingHeader::new(t.section_network)
                        .id_salt((id, "network"))
                        .show(ui, |ui| network(ui, s));
                    egui::CollapsingHeader::new(t.section_playback)
                        .id_salt((id, "playback"))
                        .show(ui, |ui| playback(ui, s));
                    egui::CollapsingHeader::new(t.section_media)
                        .id_salt((id, "media"))
                        .show(ui, |ui| media(ui, s));
                });
        });
    ctx.data_mut(|d| d.insert_temp(key, panel));
}

fn ms(value: Option<f64>) -> String {
    value.map_or_else(|| "—".into(), |v| format!("{v:.2} ms"))
}
fn health_metric(ui: &mut Ui, s: &PerformanceSnapshot, metric: Metric) {
    let allowance = s.health.allowance(metric);
    controls::metric_pair_colored(
        ui,
        metric.label(),
        s.health.value(metric).map_or_else(
            || "—".into(),
            // The cinema buffer is part of the value but not judged.
            |v| match allowance >= 0.5 {
                true => (t().viewer.with_cinema_buffer)(&metric.format(v), allowance),
                false => metric.format(v),
            },
        ),
        health_color(s.health.level(metric)),
    );
}
fn heading(ui: &mut Ui, title: &str) {
    ui.add_space(theme::MENU_GROUP_GAP);
    ui.label(RichText::new(title).size(theme::TINY).color(theme::MUTED));
}
fn network(ui: &mut Ui, s: &PerformanceSnapshot) {
    let t = &t().viewer;
    controls::metric_pair(
        ui,
        t.ingress_queue,
        (t.ingress_queue_value)(s.ingress_queue_packets, s.ingress_queue_peak_packets),
    );
    controls::metric_pair(ui, t.pending_nack, (t.packets)(s.outstanding_nacks));
    controls::metric_pair(
        ui,
        t.unrepaired_loss,
        format!("{:.2}%", s.unrepaired_loss_percent),
    );
    controls::metric_pair(
        ui,
        t.lost_packets,
        (t.lost_packets_value)(s.lost_packets, s.unrepaired_lost_packets),
    );
    controls::metric_pair(
        ui,
        t.rtx_total,
        (t.rtx_value)(s.rtx_packets_accepted, s.rtx_packets_received),
    );
    controls::metric_pair(
        ui,
        t.fec_total,
        (t.fec_value)(s.fec_packets_recovered, s.fec_packets_received),
    );
    controls::metric_pair(
        ui,
        t.total_received,
        format!("{:.1} MiB", s.total_received_rtp_bytes as f64 / 1_048_576.0),
    );
    controls::metric_pair(
        ui,
        t.playout,
        if s.low_latency_playout {
            t.low_latency_direct.into()
        } else {
            (t.playout_target)(s.target_playout_delay_ms, s.jitter_playout_delay_ms)
        },
    );
}
fn playback(ui: &mut Ui, s: &PerformanceSnapshot) {
    let t = &t().viewer;
    controls::metric_pair(
        ui,
        t.receive_decode_present,
        format!(
            "{:.1} / {:.1} / {:.1} FPS",
            s.receive_fps, s.decode_fps, s.render_fps
        ),
    );
    health_metric(ui, s, Metric::FrameDelay);
    health_metric(ui, s, Metric::PresentationStall);
    heading(ui, t.heading_last_frame);
    for (label, value) in [
        (t.assembly_recovery, s.assembly_delay_ms),
        (t.input_queue, s.input_queue_delay_ms),
        (t.decode, s.decode_pipeline_delay_ms),
        (t.texture_handoff, s.surface_transfer_delay_ms),
        (t.present_wait, s.present_wait_delay_ms),
        (t.render_queue, s.render_queue_delay_ms),
    ] {
        controls::metric_pair(ui, label, format!("{value:.2}"));
    }
    if !s.low_latency_playout {
        controls::metric_pair(
            ui,
            t.cinema_buffer_part,
            format!("{:.2}", s.playout_hold_ms),
        );
    }
    controls::metric_pair(
        ui,
        t.last_300_frames,
        (t.average_p95_max)(
            s.local_frame_delay_average_ms,
            s.local_frame_delay_p95_ms,
            s.local_frame_delay_max_ms,
        ),
    );
    heading(ui, t.heading_frame_interval);
    for (label, c) in [
        (t.source_timestamps, s.source_cadence),
        (t.assembled_arrival, s.receive_cadence),
        (t.decoder_output, s.decode_cadence),
        (t.present_submit, s.render_cadence),
    ] {
        controls::metric_pair(
            ui,
            label,
            format!("{:.2} / {:.2} / {:.2}", c.average_ms, c.p95_ms, c.max_ms),
        );
    }
    heading(ui, t.heading_queues);
    for (label, current, peak) in [
        (
            t.frame_buffer,
            s.frame_buffer_frames,
            s.frame_buffer_peak_frames,
        ),
        (
            t.decoder_queue,
            s.decoder_queue_frames,
            s.decoder_queue_peak_frames,
        ),
        (
            t.present_queue,
            s.presentation_queue_frames,
            s.presentation_queue_peak_frames,
        ),
    ] {
        controls::metric_pair(ui, label, (t.frames_current_peak)(current, peak));
    }
    controls::metric_pair(
        ui,
        t.dropped,
        (t.dropped_value)(s.predecode_dropped_frames, s.dropped_present_frames),
    );
    controls::metric_pair(
        ui,
        t.present_drops,
        format!("{:.2}%", s.presentation_drop_percent),
    );
    controls::metric_pair(
        ui,
        t.long_intervals,
        format!(
            "100–179 ms: {} · ≥180 ms: {} · ≥500 ms: {}",
            s.small_jank_count, s.jank_count, s.big_jank_count
        ),
    );
    if let Some(p) = &s.pipeline_stats {
        heading(ui, t.heading_capture_to_decode);
        for (label, phase) in [
            (t.remote_capture, p.capture),
            (t.remote_encode, p.encode),
            (t.remote_pacing, p.pacer),
            (t.transport, p.transport),
            (t.assembly, p.assembly),
            (t.decode, p.decode),
        ] {
            controls::metric_pair(
                ui,
                label,
                phase.map_or_else(
                    || "—".into(),
                    |v| format!("{:.2} / {:.2}", v.p50_ms, v.p90_ms),
                ),
            );
        }
        controls::metric_pair(
            ui,
            t.send_receive_fps,
            format!(
                "{} / {:.1} FPS",
                p.source_fps
                    .map_or_else(|| "—".into(), |v| format!("{v:.1}")),
                p.received_fps
            ),
        );
        if let Some(v) = p.sending {
            controls::metric_pair(
                ui,
                t.sending_total,
                (t.average_max_ms)(v.average_ms, v.max_ms),
            );
        }
        if let Some(v) = p.e2e {
            controls::metric_pair(
                ui,
                t.capture_to_decoded,
                (t.end_to_end_value)(v.average_ms, v.p50_ms, v.p90_ms, v.p99_ms, v.max_ms),
            );
        }
    }
}
fn media(ui: &mut Ui, s: &PerformanceSnapshot) {
    let t = &t().viewer;
    heading(ui, t.heading_current_video);
    for (label, value) in [
        (t.quality, &s.quality),
        (t.codec, &s.video_codec),
        (t.stream_format, &s.video_format),
        (t.local_decoder, &s.decoder),
        (t.remote_capture, &s.remote_capture),
        (t.remote_encoder, &s.remote_encoder),
    ] {
        controls::metric_pair(ui, label, if value.is_empty() { "—" } else { value });
    }
    controls::metric_pair(ui, t.decoded_size, format_resolution(s.decoded_resolution));
    controls::metric_pair(
        ui,
        t.total_frames,
        (t.total_frames_value)(
            s.total_received_frames,
            s.total_decoded_frames,
            s.total_rendered_frames,
        ),
    );
    controls::metric_pair(ui, t.key_frames, s.total_key_frames_decoded.to_string());
    controls::metric_pair(
        ui,
        t.total_frame_updates,
        s.total_actual_rendered_frames.to_string(),
    );
    heading(ui, t.heading_last_switch);
    if let Some(v) = &s.stream_switch {
        controls::metric_pair(
            ui,
            t.switch_state,
            format!("{} · #{}, {:.0} ms", v.stage, v.sequence, v.age_ms),
        );
        controls::metric_pair(ui, t.switch_target, &v.target);
        controls::metric_pair(
            ui,
            t.resolution,
            format!(
                "{} → {}",
                format_resolution(v.from_resolution),
                format_resolution(v.actual_resolution)
            ),
        );
        for (label, value) in [
            (t.request_to_ack, v.request_to_ack_ms),
            (t.request_to_steady_video, v.request_to_continuity_ms),
            (t.request_to_key_frame, v.request_to_media_ms),
            (t.request_to_present, v.request_to_present_ms),
            (t.receive_gap, v.receive_gap_ms),
            (t.decode_gap, v.decode_gap_ms),
            (t.present_gap, v.presentation_gap_ms),
        ] {
            controls::metric_pair(ui, label, ms(value));
        }
        crate::ui::controls::observe_notice(
            ui.ctx(),
            "performance-switch-error",
            t.video_switch_failed,
            crate::ui::controls::DialogIcon::Error,
            v.error.as_deref(),
        );
    } else {
        controls::metric_pair(ui, t.switch_log, crate::i18n::t().common.none);
    }
}
