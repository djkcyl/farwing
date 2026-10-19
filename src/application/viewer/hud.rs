//! Performance overlay formatting and presentation modes.
use super::{COMPACT_COLUMN_GAP, COMPACT_HUD_WIDTH, COMPACT_METER_WIDTH, performance_panel};
use crate::diagnostics::performance::health::{Level, Metric};
use crate::diagnostics::performance::{PerformanceMonitor, PerformanceSnapshot};
use std::time::Duration;

pub(in crate::application) fn show_performance_overlay(
    ctx: &egui::Context,
    performance: &PerformanceMonitor,
    audio: &crate::media::audio::AudioPlayback,
    mode: PerformancePanelMode,
    always_visible: bool,
    video_bounds: egui::Rect,
    grid_id: &'static str,
) {
    let stats = performance.snapshot();
    // Keep observing and expire held alerts even on an otherwise idle desktop.
    ctx.request_repaint_after(Duration::from_millis(250));
    super::performance_alerts::show(ctx, &stats.health, video_bounds);
    if !always_visible {
        return;
    }
    match mode {
        PerformancePanelMode::Compact => show_compact_performance(ctx, &stats, audio),
        PerformancePanelMode::Detailed => {
            performance_panel::show(ctx, performance, &stats, grid_id)
        }
    }
}

pub(in crate::application) use crate::features::viewing_settings::PerformancePanelMode;

pub(super) fn show_compact_performance(
    ctx: &egui::Context,
    stats: &PerformanceSnapshot,
    audio: &crate::media::audio::AudioPlayback,
) {
    ctx.request_repaint_after(Duration::from_millis(50));
    egui::Window::new("性能简报")
        .id(egui::Id::new("performance-compact"))
        // This HUD has no controls. In particular, it must not become the
        // foreground layer used by the raw-mouse and annotation hit tests.
        .interactable(false)
        .anchor(egui::Align2::RIGHT_BOTTOM, [-12.0, -44.0])
        .min_width(COMPACT_HUD_WIDTH)
        .max_width(COMPACT_HUD_WIDTH)
        .resizable(false)
        .collapsible(false)
        .title_bar(false)
        .frame(compact_performance_frame())
        .show(ctx, |ui| {
            ui.visuals_mut().override_text_color = Some(egui::Color32::WHITE);
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.set_width(COMPACT_HUD_WIDTH);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = COMPACT_COLUMN_GAP;
                let text = ui.vertical(|ui| {
                    // Keep both columns stationary when a value gains digits.
                    ui.set_width(COMPACT_HUD_WIDTH - COMPACT_METER_WIDTH - COMPACT_COLUMN_GAP);
                    compact_hud_line(ui, &format_uptime(stats.uptime), egui::Color32::WHITE);
                    compact_hud_line(ui, &stats.connection, connection_color(&stats.connection));
                    compact_hud_line(
                        ui,
                        &format!("{:.0} fps", stats.actual_fps),
                        frame_rate_color(stats),
                    );
                    compact_hud_line(
                        ui,
                        &format!("{:.1} Mbps", stats.bitrate_mbps),
                        egui::Color32::WHITE,
                    );
                    compact_hud_line(
                        ui,
                        &format_optional_ms(stats.health.value(Metric::Rtt)),
                        health_color(stats.health.level(Metric::Rtt)),
                    );
                    compact_hud_line(
                        ui,
                        &stats.health.value(Metric::FrameDelay).map_or_else(
                            || "— ms frm.".to_owned(),
                            |value| format!("{value:.0} ms frm."),
                        ),
                        health_color(stats.health.level(Metric::FrameDelay)),
                    );
                    compact_hud_line(
                        ui,
                        &stats
                            .health
                            .value(Metric::Loss)
                            .map_or_else(|| "— loss".into(), |v| format!("{v:.2}% loss")),
                        health_color(stats.health.level(Metric::Loss)),
                    );
                    compact_hud_line(ui, &stats.quality, egui::Color32::WHITE);
                });
                compact_audio_meter(ui, audio, text.response.rect.height());
            });
        });
}

pub(super) fn compact_audio_meter(
    ui: &mut egui::Ui,
    audio: &crate::media::audio::AudioPlayback,
    height: f32,
) {
    let settings = audio.settings();
    let muted = settings.muted || settings.volume == 0;
    let fill = audio.output_levels().map(|level| {
        if level > 0.0 {
            ((20.0 * level.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
        } else {
            0.0
        }
    });
    let now = ui.input(|input| input.time);
    let peak_id = ui.id().with("stereo-meter-peaks");
    let peaks = ui.ctx().data_mut(|data| {
        let mut peaks = data
            .get_temp::<[(f32, f64); 2]>(peak_id)
            .unwrap_or_default();
        for channel in 0..2 {
            if muted {
                peaks[channel] = (0.0, now);
            } else if fill[channel] >= peaks[channel].0 || now >= peaks[channel].1 {
                peaks[channel] = (fill[channel], now + 0.8);
            }
        }
        data.insert_temp(peak_id, peaks);
        peaks
    });
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(COMPACT_METER_WIDTH, height),
        egui::Sense::hover(),
    );
    let painter = ui.painter();
    let ink = crate::ui::theme::MUTED;
    let font = egui::FontId::monospace(7.5);
    let top = rect.top() + 12.0;
    let bottom = rect.bottom() - 4.0;
    let meter_height = bottom - top;
    let y_at = |level: f32| bottom - meter_height * level;
    for (channel, name) in ["L", "R"].into_iter().enumerate() {
        let left = rect.left() + channel as f32 * 8.0;
        painter.text(
            egui::pos2(left + 2.5, rect.top()),
            egui::Align2::CENTER_TOP,
            name,
            font.clone(),
            ink,
        );
        for segment in 0..24 {
            let color = if fill[channel] * 24.0 > segment as f32 {
                match segment {
                    22.. => crate::ui::theme::RED,
                    17..=21 => crate::ui::theme::AMBER,
                    _ => crate::ui::theme::GREEN,
                }
            } else {
                egui::Color32::from_white_alpha(24)
            };
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(left, y_at((segment + 1) as f32 / 24.0)),
                    egui::vec2(5.0, (meter_height / 24.0 - 1.0).max(1.0)),
                ),
                0.5,
                color,
            );
        }
        if peaks[channel].0 > 0.0 {
            let y = y_at(peaks[channel].0);
            painter.line_segment(
                [egui::pos2(left, y), egui::pos2(left + 5.0, y)],
                egui::Stroke::new(1.0, crate::ui::theme::TEXT),
            );
        }
    }
    painter.text(
        egui::pos2(rect.right(), rect.top()),
        egui::Align2::RIGHT_TOP,
        "dB",
        font.clone(),
        ink,
    );
    for db in [0, -6, -12, -24, -36, -48, -60] {
        let y = y_at((db as f32 + 60.0) / 60.0);
        painter.line_segment(
            [
                egui::pos2(rect.left() + 16.0, y),
                egui::pos2(rect.left() + 18.0, y),
            ],
            egui::Stroke::new(1.0, crate::ui::theme::DISABLED),
        );
        painter.text(
            egui::pos2(rect.right(), y),
            egui::Align2::RIGHT_CENTER,
            db.to_string(),
            font.clone(),
            ink,
        );
    }
}

pub(super) fn compact_performance_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(egui::Color32::from_black_alpha(165))
        .stroke(egui::Stroke::NONE)
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(7, 5))
}

pub(super) fn compact_hud_line(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    ui.add(
        egui::Label::new(
            egui::RichText::new(text)
                .monospace()
                .size(crate::ui::theme::MICRO)
                .color(color.gamma_multiply(0.78)),
        )
        .truncate(),
    );
}

pub(super) fn connection_color(connection: &str) -> egui::Color32 {
    if connection.contains("P2P") || connection.contains("LAN") {
        good_color()
    } else if connection.to_ascii_lowercase().contains("relay") {
        warning_color()
    } else {
        egui::Color32::WHITE
    }
}

pub(super) fn frame_rate_color(stats: &PerformanceSnapshot) -> egui::Color32 {
    // Receiving more frames than can be presented is not itself a failure.
    health_color(stats.health.level(Metric::PresentationStall))
}

pub(super) fn format_uptime(value: Duration) -> String {
    let seconds = value.as_secs();
    let hours = seconds / 3600;
    let minutes = seconds % 3600 / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

pub(super) fn format_resolution(value: Option<(u32, u32)>) -> String {
    value.map_or_else(
        || "—".to_owned(),
        |(width, height)| format!("{width}×{height}"),
    )
}

pub(super) fn health_color(level: Option<Level>) -> egui::Color32 {
    match level {
        Some(Level::Normal) => good_color(),
        Some(Level::Warning) => warning_color(),
        Some(Level::Bad) => bad_color(),
        None => crate::ui::theme::MUTED,
    }
}

pub(super) fn good_color() -> egui::Color32 {
    crate::ui::theme::GREEN
}

pub(super) fn warning_color() -> egui::Color32 {
    crate::ui::theme::AMBER
}

pub(super) fn bad_color() -> egui::Color32 {
    crate::ui::theme::RED
}

pub(super) fn format_optional_ms(value: Option<f64>) -> String {
    value.map_or_else(|| "—".to_owned(), |value| format!("{value:.0} ms"))
}
