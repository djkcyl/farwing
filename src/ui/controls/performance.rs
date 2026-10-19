//! Shared metric and time-series controls; callers supply existing measurements.
use super::*;
use egui::{Align2, FontId, Rect, Sense, pos2};

pub(crate) fn metric_pair(ui: &mut egui::Ui, label: &str, value: impl Into<String>) {
    metric_pair_colored(ui, label, value, TEXT);
}

pub(crate) fn metric_pair_colored(
    ui: &mut egui::Ui,
    label: &str,
    value: impl Into<String>,
    color: Color32,
) {
    let value = value.into();
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            vec2(116.0, 18.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_min_width(116.0);
                ui.label(RichText::new(label).size(theme::SMALL).color(MUTED));
            },
        );
        ui.add(egui::Label::new(RichText::new(&value).size(theme::SMALL).color(color)).wrap());
    });
}

pub(crate) fn performance_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(theme::BG)
        .stroke(Stroke::new(1.0, LINE))
        .corner_radius(theme::PANEL_RADIUS)
        .inner_margin(14)
}

fn columns(rect: Rect) -> ([f32; 3], Rect) {
    let start = rect.left() + theme::PERFORMANCE_LABEL_WIDTH;
    let columns = [start + 52.0, start + 108.0, start + 164.0];
    let plot = Rect::from_min_max(
        pos2(start + 180.0, rect.top() + 6.0),
        pos2(rect.right(), rect.bottom() - 6.0),
    );
    (columns, plot)
}

pub(crate) fn performance_header(ui: &mut egui::Ui) {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
    let (columns, plot) = columns(rect);
    let font = FontId::proportional(theme::TINY);
    ui.painter().text(
        rect.left_center(),
        Align2::LEFT_CENTER,
        "指标",
        font.clone(),
        MUTED,
    );
    for (x, label) in columns.into_iter().zip(["当前", "均值", "峰值"]) {
        ui.painter().text(
            pos2(x, rect.center().y),
            Align2::RIGHT_CENTER,
            label,
            font.clone(),
            MUTED,
        );
    }
    ui.painter().text(
        pos2(plot.right(), rect.center().y),
        Align2::RIGHT_CENTER,
        "30 秒趋势",
        font,
        MUTED,
    );
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, LINE),
    );
    response.on_hover_text("均值与峰值来自近 30 秒的可见采样，不是逐帧统计。每行趋势独立刻度，下沿为 0；悬停可查看读数与刻度。");
}

pub(crate) struct PerformanceTrace<'a> {
    pub current_color: Color32,
    pub label: &'a str,
    pub unit: &'a str,
    pub current: Option<f64>,
    pub minimum_scale: f64,
    pub decimals: usize,
    pub hint: &'a str,
    pub points: &'a [(f64, Option<f64>)],
}

pub(crate) fn performance_trace(ui: &mut egui::Ui, now: f64, trace: PerformanceTrace<'_>) {
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::PERFORMANCE_ROW_HEIGHT),
        Sense::hover(),
    );
    let (columns, plot) = columns(rect);
    let painter = ui.painter_at(rect);
    if response.hovered() {
        painter.rect_filled(rect, theme::CONTROL_RADIUS, theme::SURFACE);
    }
    painter.text(
        rect.left_top() + vec2(0.0, 8.0),
        Align2::LEFT_TOP,
        trace.label,
        FontId::proportional(theme::SMALL),
        TEXT,
    );
    painter.text(
        rect.left_bottom() - vec2(0.0, 6.0),
        Align2::LEFT_BOTTOM,
        trace.unit,
        FontId::monospace(theme::MICRO),
        MUTED,
    );
    let valid = |v: f64| v.is_finite() && v >= 0.0;
    let values: Vec<_> = trace
        .points
        .iter()
        .filter(|(time, _)| *time >= now - 30.0 && *time <= now)
        .filter_map(|(_, v)| v.filter(|v| valid(*v)))
        .collect();
    let mean = (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64);
    let peak = values.iter().copied().reduce(f64::max);
    let format_value = |v: Option<f64>| {
        v.filter(|v| valid(*v))
            .map_or_else(|| "—".into(), |v| format!("{:.*}", trace.decimals, v))
    };
    for (index, (x, value)) in columns
        .into_iter()
        .zip([trace.current, mean, peak])
        .enumerate()
    {
        painter.text(
            pos2(x, rect.center().y),
            Align2::RIGHT_CENTER,
            format_value(value),
            FontId::monospace(theme::SMALL),
            if index == 0 {
                trace.current_color
            } else {
                MUTED
            },
        );
    }
    let maximum = peak.unwrap_or_default().max(trace.minimum_scale).max(0.001);
    let exponent = 10.0f64.powf(maximum.log10().floor());
    let top = (maximum / exponent).ceil() * exponent;
    painter.rect_filled(plot, 2.0, theme::SIDEBAR);
    for fraction in [0.0, 0.5, 1.0] {
        let x = plot.left() + plot.width() * fraction;
        painter.line_segment(
            [pos2(x, plot.top()), pos2(x, plot.bottom())],
            Stroke::new(0.5, LINE),
        );
    }
    painter.line_segment(
        [plot.left_bottom(), plot.right_bottom()],
        Stroke::new(0.5, LINE),
    );
    let mut previous = None;
    for &(at, value) in trace.points {
        let Some(value) = value.filter(|v| valid(*v)) else {
            previous = None;
            continue;
        };
        if at < now - 30.0 || at > now {
            previous = None;
            continue;
        }
        let point = pos2(
            plot.right() - ((now - at) / 30.0) as f32 * plot.width(),
            plot.bottom() - (value / top).clamp(0.0, 1.0) as f32 * plot.height(),
        );
        if let Some((_, last)) = previous.filter(|(time, _)| at - time <= 1.0) {
            painter.line_segment([last, point], Stroke::new(1.25, theme::ACCENT));
        } else {
            painter.circle_filled(point, 1.25, theme::ACCENT);
        }
        previous = Some((at, point));
    }
    let hovered = response.hover_pos().filter(|p| plot.contains(*p));
    if let Some(pos) = hovered {
        painter.line_segment(
            [pos2(pos.x, plot.top()), pos2(pos.x, plot.bottom())],
            Stroke::new(1.0, TEXT),
        );
    }
    response.on_hover_ui(|ui| {
        ui.label(RichText::new(trace.label).strong());
        if let Some(pos) = hovered {
            let time = now - 30.0 + (pos.x - plot.left()) as f64 / plot.width() as f64 * 30.0;
            let value = trace
                .points
                .iter()
                .min_by(|a, b| (a.0 - time).abs().total_cmp(&(b.0 - time).abs()))
                .filter(|(at, _)| (*at - time).abs() < 0.6)
                .and_then(|(_, v)| *v);
            ui.label(format!(
                "{:.1} 秒前  {} {}",
                now - time,
                format_value(value),
                trace.unit
            ));
        }
        ui.label(format!("纵轴 0–{top:.2} {}", trace.unit));
        ui.label(trace.hint);
    });
}
