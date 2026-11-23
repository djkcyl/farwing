//! Bounded, per-viewer UI history. Does not alter receiver sampling or scheduling.
use super::hud::format_resolution;
use super::hud::format_uptime;
use super::hud::health_color;
use crate::diagnostics::performance::PerformanceMonitor;
use crate::diagnostics::performance::PerformanceSnapshot;
use crate::diagnostics::performance::health::Metric;
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
        hint: &str,
        current: Option<f64>,
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
                minimum_scale,
                decimals,
                hint,
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
    egui::Window::new("性能监控")
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
                ui.label(RichText::new("性能监控").size(theme::COMPACT_TEXT).strong());
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
                    s.video_codec
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
                    panel.trace(
                        ui,
                        0,
                        "画面更新",
                        "FPS",
                        30.0,
                        1,
                        "仅统计实际呈现的新画面；静态桌面时可以低于设定帧率。",
                        Some(s.actual_fps),
                        super::hud::frame_rate_color(s),
                        now,
                    );
                    panel.trace(
                        ui,
                        1,
                        "网络往返",
                        "ms",
                        5.0,
                        2,
                        "优先媒体 RTCP RTT，缺少时使用当前 ICE 链路 RTT；不是单向网络延迟。",
                        s.health.value(Metric::Rtt),
                        health_color(s.health.level(Metric::Rtt)),
                        now,
                    );
                    panel.trace(
                        ui,
                        2,
                        "接收码率",
                        "Mbps",
                        1.0,
                        2,
                        "实际收到的视频 RTP 码率，不是配置的码率上限。",
                        Some(s.bitrate_mbps),
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
                            "近期呈现丢帧",
                            s.health
                                .recent_drop_percent
                                .map_or_else(|| "—".into(), |v| format!("{v:.2}%")),
                        );
                    });
                    ui.add_space(theme::MENU_GROUP_GAP);
                    egui::CollapsingHeader::new("网络与恢复")
                        .id_salt((id, "network"))
                        .show(ui, |ui| network(ui, s));
                    egui::CollapsingHeader::new("解码与呈现")
                        .id_salt((id, "playback"))
                        .show(ui, |ui| playback(ui, s));
                    egui::CollapsingHeader::new("媒体与切换")
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
    let row = ui.scope(|ui| {
        controls::metric_pair_colored(
            ui,
            metric.label(),
            s.health
                .value(metric)
                .map_or_else(|| "—".into(), |v| metric.format(v)),
            health_color(s.health.level(metric)),
        );
    });
    if metric == Metric::Loss {
        row.response.on_hover_text(
            "恢复前的接收丢包率，补回的数据仍计入；乱序到达的原始包会修正统计。暂无有效样本时显示 —。",
        );
    }
}
fn heading(ui: &mut Ui, title: &str) {
    ui.add_space(theme::MENU_GROUP_GAP);
    ui.label(RichText::new(title).size(theme::TINY).color(theme::MUTED));
}
fn network(ui: &mut Ui, s: &PerformanceSnapshot) {
    controls::metric_pair(
        ui,
        "自动切路",
        format!(
            "{} · 尝试 {} / 成功 {}",
            match s.network_switch_phase {
                1 => "UDP 中转",
                2 => "TLS 中转",
                _ => "直连",
            },
            s.network_switch_attempts,
            s.network_switch_successes
        ),
    );
    controls::metric_pair(
        ui,
        "入口队列",
        format!(
            "{} 包 · 峰值 {}",
            s.ingress_queue_packets, s.ingress_queue_peak_packets
        ),
    );
    controls::metric_pair(ui, "待恢复 NACK", format!("{} 包", s.outstanding_nacks));
    controls::metric_pair(
        ui,
        "放弃修复累计",
        format!("{} 包", s.abandoned_repair_packets),
    );
    controls::metric_pair(
        ui,
        "RTX 累计",
        format!(
            "回灌 {} / 收到 {} 包",
            s.rtx_packets_accepted, s.rtx_packets_received
        ),
    );
    controls::metric_pair(
        ui,
        "FEC 累计",
        format!(
            "恢复 {} / 修复包 {}",
            s.fec_packets_recovered, s.fec_packets_received
        ),
    );
    controls::metric_pair(
        ui,
        "接收总量",
        format!("{:.1} MiB", s.total_received_rtp_bytes as f64 / 1_048_576.0),
    );
    controls::metric_pair(
        ui,
        "接收调度",
        if s.low_latency_playout {
            "UU 低延迟 · 直接呈现".into()
        } else {
            format!(
                "目标 {:.1} / 抖动估计 {:.1} ms",
                s.target_playout_delay_ms, s.jitter_playout_delay_ms
            )
        },
    );
}
fn playback(ui: &mut Ui, s: &PerformanceSnapshot) {
    controls::metric_pair(
        ui,
        "接收 / 解码 / 呈现",
        format!(
            "{:.1} / {:.1} / {:.1} FPS",
            s.receive_fps, s.decode_fps, s.render_fps
        ),
    );
    health_metric(ui, s, Metric::FrameDelay);
    health_metric(ui, s, Metric::PresentationStall);
    heading(ui, "最近一帧 · 首包接收到呈现提交 · ms");
    for (label, value) in [
        ("组帧 / 恢复", s.assembly_delay_ms),
        ("输入排队", s.input_queue_delay_ms),
        ("解码", s.decode_pipeline_delay_ms),
        ("纹理交接", s.surface_transfer_delay_ms),
        ("呈现等待", s.present_wait_delay_ms),
        ("渲染排队", s.render_queue_delay_ms),
    ] {
        controls::metric_pair(ui, label, format!("{value:.2}"));
    }
    controls::metric_pair(
        ui,
        "近 300 帧",
        format!(
            "均值 {:.2} / P95 {:.2} / 最大 {:.2}",
            s.local_frame_delay_average_ms, s.local_frame_delay_p95_ms, s.local_frame_delay_max_ms
        ),
    );
    heading(ui, "帧间隔 · 均值 / P95 / 最大 · ms");
    for (label, c) in [
        ("源时间戳", s.source_cadence),
        ("组帧到达", s.receive_cadence),
        ("解码输出", s.decode_cadence),
        ("呈现提交", s.render_cadence),
    ] {
        controls::metric_pair(
            ui,
            label,
            format!("{:.2} / {:.2} / {:.2}", c.average_ms, c.p95_ms, c.max_ms),
        );
    }
    heading(ui, "队列 · 当前 / 峰值");
    for (label, current, peak) in [
        (
            "组帧缓冲",
            s.frame_buffer_frames,
            s.frame_buffer_peak_frames,
        ),
        (
            "解码队列",
            s.decoder_queue_frames,
            s.decoder_queue_peak_frames,
        ),
        (
            "呈现队列",
            s.presentation_queue_frames,
            s.presentation_queue_peak_frames,
        ),
    ] {
        controls::metric_pair(ui, label, format!("{current} / {peak} 帧"));
    }
    controls::metric_pair(
        ui,
        "累计丢弃",
        format!(
            "解码前 {} / 呈现 {} 帧",
            s.predecode_dropped_frames, s.dropped_present_frames
        ),
    );
    controls::metric_pair(
        ui,
        "累计呈现丢帧",
        format!("{:.2}%", s.presentation_drop_percent),
    );
    controls::metric_pair(
        ui,
        "长间隔累计",
        format!(
            "100–179 ms: {} · ≥180 ms: {} · ≥500 ms: {}",
            s.small_jank_count, s.jank_count, s.big_jank_count
        ),
    );
    if let Some(p) = &s.pipeline_stats {
        heading(ui, "采集至解码 · P50 / P90 · ms");
        for (label, phase) in [
            ("远端采集", p.capture),
            ("远端编码", p.encode),
            ("远端节奏控制", p.pacer),
            ("传输", p.transport),
            ("组帧", p.assembly),
            ("解码", p.decode),
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
            "发送 / 接收帧率",
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
                "发送总计",
                format!("均值 {:.2} / 最大 {:.2} ms", v.average_ms, v.max_ms),
            );
        }
        if let Some(v) = p.e2e {
            controls::metric_pair(
                ui,
                "采集→解码完成",
                format!(
                    "均值 {:.2} / P50 {:.2} / P90 {:.2} / P99 {:.2} / 最大 {:.2} ms",
                    v.average_ms, v.p50_ms, v.p90_ms, v.p99_ms, v.max_ms
                ),
            );
        }
    }
}
fn media(ui: &mut Ui, s: &PerformanceSnapshot) {
    heading(ui, "当前画面");
    for (label, value) in [
        ("画质", &s.quality),
        ("编码", &s.video_codec),
        ("码流格式", &s.video_format),
        ("本地解码器", &s.decoder),
        ("远端采集", &s.remote_capture),
        ("远端编码器", &s.remote_encoder),
    ] {
        controls::metric_pair(ui, label, if value.is_empty() { "—" } else { value });
    }
    controls::metric_pair(ui, "解码尺寸", format_resolution(s.decoded_resolution));
    controls::metric_pair(
        ui,
        "累计帧数",
        format!(
            "接收 {} / 解码 {} / 呈现 {}",
            s.total_received_frames, s.total_decoded_frames, s.total_rendered_frames
        ),
    );
    controls::metric_pair(ui, "累计关键帧", s.total_key_frames_decoded.to_string());
    controls::metric_pair(
        ui,
        "累计画面更新",
        s.total_actual_rendered_frames.to_string(),
    );
    heading(ui, "最近串流切换");
    if let Some(v) = &s.stream_switch {
        controls::metric_pair(
            ui,
            "状态",
            format!("{} · #{}, {:.0} ms", v.stage, v.sequence, v.age_ms),
        );
        controls::metric_pair(ui, "目标", &v.target);
        controls::metric_pair(
            ui,
            "分辨率",
            format!(
                "{} → {}",
                format_resolution(v.from_resolution),
                format_resolution(v.actual_resolution)
            ),
        );
        for (label, value) in [
            ("请求→回执", v.request_to_ack_ms),
            ("请求→持续画面", v.request_to_continuity_ms),
            ("请求→关键帧", v.request_to_media_ms),
            ("请求→呈现", v.request_to_present_ms),
            ("接收间隔", v.receive_gap_ms),
            ("解码间隔", v.decode_gap_ms),
            ("呈现间隔", v.presentation_gap_ms),
        ] {
            controls::metric_pair(ui, label, ms(value));
        }
        crate::ui::controls::observe_notice(
            ui.ctx(),
            "performance-switch-error",
            "画面切换异常",
            crate::ui::controls::DialogIcon::Error,
            v.error.as_deref(),
        );
    } else {
        controls::metric_pair(ui, "切换记录", "暂无");
    }
}
