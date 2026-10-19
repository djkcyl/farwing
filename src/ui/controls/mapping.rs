use super::{switch, theme};
use egui::{
    Align, Align2, FontId, Layout, Rect, RichText, Sense, Stroke, Ui, UiBuilder, pos2, vec2,
};

pub(crate) enum MappingRowAction {
    None,
    Enable(bool),
    Probe,
    Edit,
    Delete,
}
pub(crate) struct MappingRow<'a> {
    pub name: &'a str,
    pub local: &'a str,
    pub target: &'a str,
    pub status: &'a str,
    pub detail: &'a str,
    pub hint: &'a str,
    pub speed: &'a str,
    pub traffic: &'a str,
    pub enabled: bool,
    pub color: egui::Color32,
    pub reachable: Option<bool>,
    pub url: Option<&'a str>,
    pub probing: bool,
    pub can_probe: bool,
}
fn columns(rect: Rect) -> [Rect; 7] {
    let rect = rect.shrink2(vec2(12.0, 0.0));
    let width = rect.width();
    let fixed = 28.0 + 118.0 + 120.0 + 104.0 + 110.0;
    let flexible = (width - fixed).max(300.0);
    let widths = [
        28.0,
        flexible * 0.30,
        flexible * 0.70,
        118.0,
        120.0,
        104.0,
        110.0,
    ];
    let mut x = rect.left();
    std::array::from_fn(|i| {
        let cell = Rect::from_min_max(pos2(x, rect.top()), pos2(x + widths[i], rect.bottom()));
        x += widths[i];
        cell
    })
}
pub(crate) fn mapping_table_header(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::MAPPING_TABLE_HEADER),
        Sense::hover(),
    );
    for (rect, label) in
        columns(rect)
            .into_iter()
            .zip(["", "规则", "地址", "连通状态", "网速", "累计流量", "操作"])
    {
        ui.painter().text(
            pos2(rect.left(), rect.center().y),
            Align2::LEFT_CENTER,
            label,
            FontId::proportional(theme::SMALL),
            theme::MUTED,
        );
    }
    ui.painter()
        .hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, theme::LINE));
}
pub(crate) fn mapping_row(ui: &mut Ui, row: MappingRow<'_>, last: bool) -> MappingRowAction {
    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::MAPPING_ROW_HEIGHT),
        Sense::hover(),
    );
    let cells = columns(rect);
    let center = pos2(cells[0].left() + 9.0, cells[0].center().y);
    let color = if row.url.is_some() {
        theme::ACCENT
    } else if row.reachable == Some(false) {
        theme::RED
    } else {
        theme::MUTED
    };
    let stroke = Stroke::new(theme::ICON_STROKE, color);
    if row.url.is_some() {
        ui.painter().circle_stroke(center, 8.0, stroke);
        ui.painter().add(egui::Shape::line(
            (0..=24)
                .map(|i| {
                    let angle = i as f32 * std::f32::consts::TAU / 24.0;
                    center + vec2(angle.cos() * 3.5, angle.sin() * 8.0)
                })
                .collect(),
            stroke,
        ));
        ui.painter()
            .line_segment([center - vec2(8.0, 0.0), center + vec2(8.0, 0.0)], stroke);
    } else {
        ui.painter().rect_stroke(
            Rect::from_center_size(center, vec2(16.0, 12.0)),
            2,
            stroke,
            egui::StrokeKind::Inside,
        );
        ui.painter()
            .line_segment([center + vec2(-5.0, 9.0), center + vec2(5.0, 9.0)], stroke);
    }
    ui.interact(cells[0], ui.id().with("service-type"), Sense::hover())
        .on_hover_text(if row.url.is_some() {
            "HTTP 服务"
        } else if row.reachable == Some(true) {
            "TCP 可达，未识别为 HTTP"
        } else {
            "服务类型尚未确认"
        });
    line(
        ui,
        centered_line(cells[1]),
        RichText::new(row.name).strong(),
        false,
    )
    .on_hover_text(row.name);
    for (index, (label, value)) in [("本地", row.local), ("目标", row.target)]
        .into_iter()
        .enumerate()
    {
        let rect = pair_line(cells[2], index);
        let mut cell = cell_ui(ui, rect);
        cell.spacing_mut().item_spacing.x = 6.0;
        cell.label(
            RichText::new(label)
                .size(theme::COMPACT_TEXT)
                .color(theme::MUTED),
        );
        let response = cell.add(
            egui::Label::new(
                RichText::new(value)
                    .monospace()
                    .size(theme::COMPACT_TEXT)
                    .color(if index == 0 && row.url.is_some() {
                        theme::ACCENT
                    } else {
                        theme::TEXT
                    }),
            )
            .truncate()
            .sense(if index == 0 {
                Sense::click()
            } else {
                Sense::hover()
            }),
        );
        if index == 0 {
            if response.clicked() {
                if let Some(url) = row.url {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                } else {
                    ui.ctx().copy_text(value.to_owned());
                }
            }
            response.on_hover_text(format!(
                "{value}\n{}",
                if row.url.is_some() {
                    "打开 HTTP 服务"
                } else {
                    "复制本地地址"
                }
            ));
        } else {
            response.on_hover_text(value);
        }
    }
    let detail = if row.probing {
        "探测中…"
    } else {
        row.detail
    };
    let status_rect = if detail.is_empty() {
        centered_line(cells[3])
    } else {
        pair_line(cells[3], 0)
    };
    let response = line(
        ui,
        status_rect,
        RichText::new(row.status).color(row.color),
        row.can_probe,
    );
    let mut action = if response.clicked() {
        MappingRowAction::Probe
    } else {
        MappingRowAction::None
    };
    response.on_hover_text(if row.can_probe {
        format!("{}\n点击重新探测", row.hint)
    } else {
        row.hint.to_owned()
    });
    if !detail.is_empty() {
        line(
            ui,
            pair_line(cells[3], 1),
            RichText::new(detail).color(theme::MUTED),
            false,
        );
    }
    for (rect, value) in [(cells[4], row.speed), (cells[5], row.traffic)] {
        let lines = value.lines().collect::<Vec<_>>();
        for (index, text) in lines.iter().take(2).enumerate() {
            let rect = if lines.len() == 1 {
                centered_line(rect)
            } else {
                pair_line(rect, index)
            };
            line(
                ui,
                rect,
                RichText::new(*text).monospace().color(theme::MUTED),
                false,
            )
            .on_hover_text(value);
        }
    }
    let mut actions = ui.new_child(
        UiBuilder::new()
            .max_rect(Rect::from_center_size(
                cells[6].center(),
                vec2(cells[6].width(), theme::COMPACT_HEIGHT),
            ))
            .layout(Layout::left_to_right(Align::Center)),
    );
    actions.spacing_mut().item_spacing.x = 8.0;
    let mut enabled = row.enabled;
    if switch(&mut actions, &mut enabled)
        .on_hover_text(if row.enabled {
            "停用规则"
        } else {
            "启用规则"
        })
        .changed()
    {
        action = MappingRowAction::Enable(enabled);
    }
    if super::edit_button(&mut actions, "编辑规则", theme::COMPACT_HEIGHT).clicked() {
        action = MappingRowAction::Edit;
    }
    if super::close_button(&mut actions, "删除规则", theme::COMPACT_HEIGHT).clicked() {
        action = MappingRowAction::Delete;
    }
    if !last {
        ui.painter().hline(
            (rect.left() + 12.0)..=(rect.right() - 12.0),
            rect.bottom(),
            Stroke::new(1.0, theme::LINE),
        );
    }
    action
}

fn centered_line(rect: Rect) -> Rect {
    Rect::from_center_size(
        rect.center(),
        vec2(rect.width(), theme::MAPPING_LINE_HEIGHT),
    )
}
fn pair_line(rect: Rect, index: usize) -> Rect {
    Rect::from_center_size(
        pos2(
            rect.center().x,
            rect.center().y + (index as f32 - 0.5) * theme::MAPPING_LINE_HEIGHT,
        ),
        vec2(rect.width(), theme::MAPPING_LINE_HEIGHT),
    )
}
fn cell_ui(ui: &mut Ui, rect: Rect) -> Ui {
    let mut cell = ui.new_child(
        UiBuilder::new()
            .max_rect(Rect::from_min_max(rect.min, rect.max - vec2(8.0, 0.0)))
            .layout(Layout::left_to_right(Align::Center)),
    );
    cell.set_clip_rect(rect.intersect(ui.clip_rect()));
    cell
}
fn line(ui: &mut Ui, rect: Rect, text: RichText, clickable: bool) -> egui::Response {
    cell_ui(ui, rect).add(
        egui::Label::new(text.size(theme::COMPACT_TEXT))
            .truncate()
            .sense(if clickable {
                Sense::click()
            } else {
                Sense::hover()
            }),
    )
}

pub(crate) fn mapping_empty(ui: &mut Ui, alias: &str, height: f32) -> bool {
    ui.set_min_height(height);
    ui.add_space(((height - theme::MAPPING_EMPTY_HEIGHT) / 2.0).max(12.0));
    let mut add = false;
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(320.0, 58.0), Sense::hover());
        for (x, title, subtitle) in [
            (rect.left(), "本机", "127.0.0.1"),
            (rect.right() - 128.0, alias, "目标服务"),
        ] {
            let node = Rect::from_min_size(pos2(x, rect.top()), vec2(128.0, 58.0));
            ui.painter()
                .rect_filled(node, theme::CONTROL_RADIUS, theme::SURFACE);
            let mut cell = ui.new_child(
                UiBuilder::new()
                    .max_rect(node.shrink2(vec2(10.0, 8.0)))
                    .layout(Layout::top_down(Align::Center)),
            );
            cell.add(egui::Label::new(RichText::new(title).size(theme::BODY)).truncate())
                .on_hover_text(title);
            cell.label(
                RichText::new(subtitle)
                    .size(theme::SMALL)
                    .color(theme::MUTED),
            );
        }
        let p = ui.painter();
        let c = rect.center();
        let stroke = Stroke::new(theme::ICON_STROKE, theme::MUTED);
        p.line_segment([c - vec2(19.0, 0.0), c + vec2(19.0, 0.0)], stroke);
        p.line_segment([c + vec2(13.0, -5.0), c + vec2(19.0, 0.0)], stroke);
        p.line_segment([c + vec2(13.0, 5.0), c + vec2(19.0, 0.0)], stroke);
        ui.add_space(20.0);
        ui.label(
            RichText::new("连接你的远端服务")
                .size(theme::SECTION)
                .strong(),
        );
        ui.add_space(4.0);
        ui.label(
            RichText::new("添加规则，通过本地端口访问目标服务")
                .color(theme::MUTED)
                .size(theme::COMPACT_TEXT),
        );
        ui.add_space(16.0);
        add = ui
            .add_sized(
                vec2(132.0, theme::CONTROL_HEIGHT),
                super::primary("添加规则"),
            )
            .clicked();
    });
    add
}
