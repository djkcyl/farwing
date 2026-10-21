use super::*;

#[derive(Clone, Copy)]
enum Glyph {
    Monitor,
    Headphones,
    PortMapping,
    Files,
    Diagnostics,
    Power,
    Restart,
    Board,
    Chip,
    Memory,
    Gpu,
    System,
    Version,
    Copy,
    Edit,
    Trash,
}
enum ButtonTone {
    Normal,
    Primary,
    Danger,
}

/// The hardware rows of a device's details.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hardware {
    Board,
    Processor,
    Memory,
    Graphics,
}

impl Hardware {
    const ALL: [Self; 4] = [Self::Board, Self::Processor, Self::Memory, Self::Graphics];

    /// The row in `crate::account::api::DeviceDetail::details`.
    fn row(self) -> DetailRow {
        match self {
            Self::Board => DetailRow::Board,
            Self::Processor => DetailRow::Processor,
            Self::Memory => DetailRow::Memory,
            Self::Graphics => DetailRow::Graphics,
        }
    }

    fn label(self) -> &'static str {
        let text = &t().center;
        match self {
            Self::Board => text.motherboard,
            Self::Processor => text.processor,
            Self::Memory => text.memory,
            Self::Graphics => text.graphics,
        }
    }

    fn glyph(self) -> Glyph {
        match self {
            Self::Board => Glyph::Board,
            Self::Processor => Glyph::Chip,
            Self::Memory => Glyph::Memory,
            Self::Graphics => Glyph::Gpu,
        }
    }
}

use crate::account::api::DetailRow;

fn hardware_summary(hardware: Option<Hardware>, value: &str) -> String {
    match hardware {
        Some(Hardware::Board) => {
            if let Some((maker, model)) = value.split_once("Product:") {
                let maker = maker.trim().trim_start_matches("Manufacturer:").trim();
                let maker = if maker.eq_ignore_ascii_case("ASUSTeK COMPUTER INC.") {
                    "ASUS"
                } else {
                    maker
                };
                return format!("{} {}", maker, model.trim());
            }
        }
        Some(Hardware::Processor) => {
            let clean = value.replace("(R)", "").replace("(TM)", "");
            let clean = clean.split_once(" Gen ").map_or(clean.as_str(), |(_, v)| v);
            return clean.split(" @ ").next().unwrap_or(clean).trim().to_owned();
        }
        Some(Hardware::Memory) => {
            if let Some(mb) = value
                .trim()
                .strip_suffix(" MB")
                .and_then(|v| v.trim().parse::<f64>().ok())
                && mb.is_finite()
                && mb > 0.0
            {
                return format!("{:.1} GB", mb / 1024.0);
            }
        }
        Some(Hardware::Graphics) => {
            let parts = value.split('|').map(str::trim).collect::<Vec<_>>();
            let primary = parts
                .iter()
                .find(|v| v.contains("NVIDIA") || v.contains("AMD") || v.contains("Radeon"))
                .or_else(|| parts.first())
                .copied()
                .unwrap_or(value);
            return primary
                .replace("GeForce ", "")
                .trim_end_matches(" GPU")
                .trim()
                .to_owned();
        }
        None => {}
    }
    value.trim().to_owned()
}

/// One row of the information tables; `hardware` tidies hardware values.
fn specification(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    label: &str,
    value: &str,
    glyph: Glyph,
    copy: bool,
    hardware: Option<Hardware>,
) {
    let font = theme::BODY;
    let icon = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 10.0, rect.center().y),
        vec2(20.0, 20.0),
    );
    paint_glyph(ui.painter(), icon, glyph, MUTED);
    ui.painter().text(
        egui::pos2(rect.left() + 36.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::proportional(font),
        MUTED,
    );
    // Room for the longest label in either language.
    let value_x = rect.left() + 156.0;
    let value_rect = egui::Rect::from_min_max(
        egui::pos2(value_x, rect.top()),
        egui::pos2(rect.right() - if copy { 24.0 } else { 0.0 }, rect.bottom()),
    );
    let mut cell = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(value_rect)
            .layout(egui::Layout::left_to_right(Align::Center)),
    );
    cell.set_clip_rect(value_rect.intersect(ui.clip_rect()));
    let display = if value.trim().is_empty() {
        t().center.not_provided.into()
    } else {
        hardware_summary(hardware, value)
    };
    cell.add(egui::Label::new(RichText::new(display).size(font)).truncate())
        .on_hover_text(value);
    if copy {
        let button_rect = egui::Rect::from_center_size(
            egui::pos2(rect.right() - 8.0, rect.center().y),
            vec2(22.0, 30.0),
        );
        let mut copy_ui = ui.new_child(egui::UiBuilder::new().max_rect(button_rect));
        let response = copy_ui
            .add_sized(button_rect.size(), egui::Button::new("").frame(false))
            .on_hover_text(t().center.copy_device_id);
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, t().center.copy_device_id)
        });
        paint_glyph(ui.painter(), button_rect.shrink(4.0), Glyph::Copy, MUTED);
        let feedback_id = ui.id().with(("copied-device-id", value));
        if response.clicked() {
            ui.ctx().copy_text(value.into());
            ui.ctx()
                .data_mut(|data| data.insert_temp(feedback_id, Instant::now()));
        }
        if let Some(at) = ui.ctx().data(|data| data.get_temp::<Instant>(feedback_id))
            && at.elapsed() < Duration::from_secs(2)
        {
            response.show_tooltip_text(t().center.device_id_copied);
            ui.ctx().request_repaint_after(
                Duration::from_secs(2) - at.elapsed().min(Duration::from_secs(2)),
            );
        }
    }
    ui.painter().hline(
        rect.left()..=rect.right(),
        rect.bottom(),
        Stroke::new(1.0, LINE),
    );
}

/// "获取诊断包" with its progress and result popup; whether a fetch starts.
fn diagnostics_button(
    ui: &mut egui::Ui,
    size: egui::Vec2,
    controller_id: &str,
    id: &str,
    state: Option<&crate::diagnostics::remote::Snapshot>,
    device: &DeviceInfo,
) -> bool {
    let label = super::remote_diagnostics::button_label(state);
    let can_start = device.is_connected() && device.controllable && device.controlled_support;
    let button = detail_button(
        ui,
        &label,
        Glyph::Diagnostics,
        can_start || state.is_some(),
        size,
        ButtonTone::Normal,
    );
    if let Some(state) = state
        && state.busy
        && state.progress.total > 0
    {
        crate::ui::controls::task_button_progress(ui, button.rect, state.progress.fraction());
    }
    let start = super::remote_diagnostics::popup(&button, controller_id, id, state, can_start);
    ui.ctx().request_repaint_after(Duration::from_millis(200));
    start
}

/// A footer button's width: at least 144, enough for its glyph and label.
fn footer_width(ui: &egui::Ui, label: &str) -> f32 {
    let label = ui
        .painter()
        .layout_no_wrap(label.into(), FontId::proportional(crate::ui::theme::BODY), TEXT)
        .size()
        .x;
    (label + 29.0 + 24.0).max(144.0)
}

fn detail_button(
    ui: &mut egui::Ui,
    label: &str,
    glyph: Glyph,
    enabled: bool,
    size: egui::Vec2,
    tone: ButtonTone,
) -> egui::Response {
    let button = match tone {
        ButtonTone::Primary => crate::ui::controls::primary(""),
        ButtonTone::Normal | ButtonTone::Danger => egui::Button::new(""),
    };
    let color = match tone {
        ButtonTone::Primary => Color32::WHITE,
        ButtonTone::Normal => TEXT,
        ButtonTone::Danger => RED,
    };
    let (slot, _) = ui.allocate_exact_size(size, Sense::hover());
    let rect = egui::Rect::from_center_size(
        slot.center(),
        vec2(size.x, crate::ui::controls::HEIGHT.min(size.y)),
    );
    let response = ui.add_enabled_ui(enabled, |ui| ui.put(rect, button)).inner;
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled() && enabled, label)
    });
    let color = if ui.is_enabled() && enabled {
        color
    } else {
        MUTED.gamma_multiply(0.45)
    };
    let galley = ui.painter().layout_no_wrap(
        label.into(),
        FontId::proportional(crate::ui::theme::BODY),
        color,
    );
    let left = response.rect.center().x - (galley.size().x + 29.0) / 2.0;
    paint_glyph(
        ui.painter(),
        egui::Rect::from_center_size(
            egui::pos2(left + 9.0, response.rect.center().y),
            vec2(19.0, 19.0),
        ),
        glyph,
        color,
    );
    ui.painter().galley(
        egui::pos2(
            left + 29.0,
            response.rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        color,
    );
    response
}

fn paint_glyph(p: &egui::Painter, r: egui::Rect, g: Glyph, color: Color32) {
    let c = r.center();
    let k = r.width() / 20.0;
    let q = |x: f32, y: f32| c + vec2(x * k, y * k);
    let s = Stroke::new(1.4, color);
    match g {
        Glyph::Power | Glyph::Restart => {
            let start = if matches!(g, Glyph::Power) {
                -0.95
            } else {
                -1.3
            };
            let end = if matches!(g, Glyph::Power) { 4.1 } else { 4.55 };
            let points = (0..25)
                .map(|i| {
                    let a = start + (end - start) * i as f32 / 24.0;
                    q(a.cos() * 7.0, a.sin() * 7.0)
                })
                .collect();
            p.add(egui::Shape::line(points, s));
            if matches!(g, Glyph::Power) {
                p.line_segment([q(0.0, -10.0), q(0.0, -1.0)], s);
            } else {
                p.add(egui::Shape::line(
                    vec![q(0.0, -9.0), q(4.0, -7.0), q(3.0, -3.0)],
                    s,
                ));
            }
        }
        Glyph::Monitor => paint_icon(p, r, Icon::Monitor, color),
        Glyph::Headphones => crate::ui::controls::paint_headphones(p, r, color),
        Glyph::PortMapping => crate::ui::controls::paint_port_mapping_icon(p, r, color),
        Glyph::Diagnostics => crate::ui::controls::paint_file_icon(p, r, color, false),
        Glyph::Files => crate::ui::controls::paint_file_icon(p, r, color, true),
        Glyph::Edit => paint_icon(p, r, Icon::Edit, color),
        Glyph::System => {
            for y in [-7.0, 1.0] {
                for x in [-7.0, 1.0] {
                    p.rect_filled(
                        egui::Rect::from_min_size(q(x, y), vec2(6.0 * k, 6.0 * k)),
                        0.0,
                        color,
                    );
                }
            }
        }
        Glyph::Copy => {
            p.rect_stroke(
                egui::Rect::from_min_max(q(-7.0, -5.0), q(4.0, 8.0)),
                2.0,
                s,
                egui::StrokeKind::Inside,
            );
            p.add(egui::Shape::line(
                vec![q(-3.0, -9.0), q(8.0, -9.0), q(8.0, 4.0)],
                s,
            ));
        }
        Glyph::Trash => {
            p.rect_stroke(
                egui::Rect::from_min_max(q(-6.0, -5.0), q(6.0, 9.0)),
                1.0,
                s,
                egui::StrokeKind::Inside,
            );
            for x in [-2.0, 2.0] {
                p.line_segment([q(x, -2.0), q(x, 6.0)], s);
            }
            p.line_segment([q(-8.0, -7.0), q(8.0, -7.0)], s);
            p.line_segment([q(-3.0, -10.0), q(3.0, -10.0)], s);
        }
        Glyph::Version => {
            p.add(egui::Shape::closed_line(
                vec![
                    q(0.0, -9.0),
                    q(8.0, -5.0),
                    q(8.0, 5.0),
                    q(0.0, 9.0),
                    q(-8.0, 5.0),
                    q(-8.0, -5.0),
                ],
                s,
            ));
            for (x, y) in [(-8.0, -5.0), (8.0, -5.0), (0.0, 9.0)] {
                p.line_segment([q(0.0, 0.0), q(x, y)], s);
            }
        }
        Glyph::Chip | Glyph::Board => {
            p.rect_stroke(
                egui::Rect::from_center_size(c, vec2(12.0 * k, 12.0 * k)),
                2.0,
                s,
                egui::StrokeKind::Inside,
            );
            p.rect_stroke(
                egui::Rect::from_center_size(c, vec2(5.0 * k, 5.0 * k)),
                0.0,
                s,
                egui::StrokeKind::Inside,
            );
            for x in [-4.0, 0.0, 4.0] {
                for sign in [-1.0, 1.0] {
                    p.line_segment([q(x, sign * 6.0), q(x, sign * 9.0)], s);
                    p.line_segment([q(sign * 6.0, x), q(sign * 9.0, x)], s);
                }
            }
        }
        Glyph::Memory | Glyph::Gpu => {
            p.rect_stroke(
                egui::Rect::from_min_max(q(-9.0, -6.0), q(9.0, 6.0)),
                1.0,
                s,
                egui::StrokeKind::Inside,
            );
            if matches!(g, Glyph::Gpu) {
                p.circle_stroke(c, 3.5 * k, s);
            } else {
                for x in [-5.0, 0.0, 5.0] {
                    p.line_segment([q(x, -3.0), q(x, 3.0)], s);
                }
            }
            for x in [-6.0, -2.0, 2.0, 6.0] {
                p.line_segment([q(x, 6.0), q(x, 9.0)], s);
            }
        }
    }
}

impl DeviceCenterApp {
    pub(super) fn reconcile_device_details(&mut self) {
        if self.center_ui.page != Page::DeviceDetails {
            return;
        }
        let Some(id) = self.center_ui.detail_id.clone() else {
            self.center_ui.close_details();
            return;
        };
        if self.detail_device(&id).is_none() {
            self.extra_details.remove(&id);
            if self.detail_pending.as_deref() == Some(&id) {
                self.detail_pending = None;
            }
            self.center_ui.close_details();
        }
    }

    fn detail_device(&self, id: &str) -> Option<&DeviceInfo> {
        self.devices
            .as_ref()
            .and_then(|list| {
                all_devices(list)
                    .find(|(_, d)| d.device_id == id)
                    .map(|(_, d)| d)
            })
            .or_else(|| {
                self.catalog.as_ref().and_then(|catalog| {
                    catalog
                        .groups
                        .entries()
                        .find(|(_, d)| d.device_id == id)
                        .map(|(_, d)| d)
                })
            })
    }

    pub(super) fn device_details_page(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.center_ui.detail_id.clone() else {
            self.center_ui.close_details();
            return;
        };
        let Some(device) = self.detail_device(&id).cloned() else {
            self.center_ui.close_details();
            return;
        };
        let viewport = ui.available_size();
        // Insert behind the toolbar now, then position the backdrop after the
        // scroll area resolves its offset. It shares one image with the header.
        let backdrop_painter = ui
            .painter()
            .with_clip_rect(ui.available_rect_before_wrap().intersect(ui.clip_rect()));
        let backdrop = backdrop_painter.add(egui::Shape::Noop);
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut back = false;
        let mut refresh = false;
        // Renaming, removing and refreshing go through the server.
        let blocker = self.blocker();
        let reason = blocker.map(|b| b.reason());
        let mut ports = false;
        let mut files = false;
        let mut diagnostics = false;
        let controller_id = self
            .devices
            .as_ref()
            .map(|list| list.current_device.device_id.clone())
            .unwrap_or_default();
        let diagnostic_state = crate::diagnostics::remote::service::snapshot(&controller_id, &id);
        // Own devices hold every capability; a friend's device what its owner granted.
        let holds = |cap: &str| {
            device.platform.is_desktop()
                && self.devices.as_ref().is_some_and(|list| {
                    list.current_device.device_id != id && list.with_capability(&id, cap).is_ok()
                })
        };
        let has_files = holds(farwing_protocol::caps::FILES);
        let has_ports = holds(farwing_protocol::caps::PORTS);
        let has_diagnostics = holds(farwing_protocol::caps::DIAGNOSTICS)
            && device.platform == farwing_protocol::Platform::Windows;
        let (header, _) = ui.allocate_exact_size(vec2(viewport.x, 48.0), Sense::hover());
        let mut header_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(header.shrink2(vec2(20.0, 0.0)))
                .layout(egui::Layout::left_to_right(Align::Center)),
        );
        header_ui.horizontal(|ui| {
            back = crate::ui::controls::back_button(
                ui,
                &(t().center.back_to)(self.center_ui.detail_parent.title()),
            )
            .clicked();
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let button = ui.add_enabled(
                    self.detail_pending.as_deref() != Some(&id) && reason.is_none(),
                    egui::Button::new(if self.detail_pending.as_deref() == Some(&id) {
                        t().center.refreshing
                    } else {
                        t().center.refresh_details
                    }),
                );
                refresh = match reason {
                    Some(reason) => button.on_disabled_hover_text(reason),
                    None => button,
                }
                .clicked();
            });
        });
        if self.center_ui.edit.is_none()
            && self.center_ui.power.is_none()
            && !self.logout_confirmation
            && !self.exit_requested
            && !egui::Popup::is_any_open(ui.ctx())
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            back = true;
        }
        if back {
            self.center_ui.close_details();
            return;
        }
        if refresh {
            self.center_ui.wallpapers.refresh(&id);
            self.extra_details.remove(&id);
            self.detail_pending = Some(id.clone());
            if self
                .worker
                .commands
                .send(GuiCommand::Detail(id.clone()))
                .is_err()
            {
                self.detail_pending = None;
            }
        }
        let current = self
            .catalog
            .as_ref()
            .is_some_and(|c| c.groups.current_device_id == id)
            || self
                .devices
                .as_ref()
                .is_some_and(|l| l.current_device.device_id == id);
        let owned = self
            .catalog
            .as_ref()
            .is_some_and(|c| c.groups.entries().any(|(_, d)| d.device_id == id));
        let detail = self
            .extra_details
            .get(&id)
            .or_else(|| {
                self.catalog
                    .as_ref()
                    .and_then(|c| c.details.get(&id))
                    .map(|d| &d.value)
            })
            .cloned();
        let texture = self
            .center_ui
            .wallpapers
            .texture(ui.ctx(), &id, &device.wallpaper_url);
        let text = &t().center;
        let wallpaper_note = if device.wallpaper_url.is_empty() {
            text.no_wallpaper
        } else if self.center_ui.wallpapers.failed(&id) {
            text.wallpaper_failed
        } else {
            text.loading_wallpaper
        };
        let fields = detail.as_ref().and_then(|r| r.as_ref().ok());
        let os = fields
            .and_then(|d| {
                d.details
                    .iter()
                    .find(|(row, _)| *row == DetailRow::System)
            })
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| device.platform_label());
        let version = if device.version_name.is_empty() {
            text.not_provided
        } else {
            &device.version_name
        };
        let mut connect = false;
        let mut audio_only = false;
        let mut edit = None;
        let mut power = None;
        let mut exit_account = false;
        self.alert(ui);
        crate::ui::controls::observe_notice(
            ui.ctx(),
            ("device-detail-error", &id),
            text.hardware_read_failed,
            crate::ui::controls::DialogIcon::Warning,
            detail
                .as_ref()
                .and_then(|result| result.as_ref().err())
                .filter(|_| !self.server_unreachable)
                .map(String::as_str),
        );
        crate::ui::controls::observe_notice(
            ui.ctx(),
            ("device-detail-refresh", &id),
            text.hardware_refresh_failed,
            crate::ui::controls::DialogIcon::Warning,
            self.catalog
                .as_ref()
                .and_then(|c| c.details.get(&id))
                .and_then(|d| d.refresh_error.as_deref())
                .filter(|_| !self.server_unreachable),
        );
        crate::ui::controls::page_scroll(("device-details-page", &id)).show(ui, |ui| {
            let width = ui.available_width();
            // Heights and spacing are logical pixels, independent of the viewport.
            let pad = 20.0;
            let hero_h = 220.0;
            let card_h = 116.0;
            let card_y = hero_h - 24.0;
            let has_power = owned && !current && device.platform.is_desktop();
            let viewing = self.is_viewing_target(&id);
            // Own devices keep diagnostics with device management below.
            let row_diagnostics = has_diagnostics && !owned;
            let power_y = card_y + card_h + 16.0;
            let info_y = if has_power || has_ports || has_files || row_diagnostics || viewing {
                power_y + theme::CONTROL_HEIGHT + 24.0
            } else {
                card_y + card_h + 24.0
            };
            let row_h = 42.0;
            let table_bottom = info_y + 28.0 + row_h * 4.0;
            let divider_y = table_bottom + 24.0;
            let footer_y = divider_y + 20.0;
            let body_h = if owned {
                footer_y + theme::CONTROL_HEIGHT + 24.0
            } else {
                table_bottom + 24.0
            };
            let (canvas, _) = ui.allocate_exact_size(vec2(width, body_h), Sense::hover());
            let at = |x: f32, y: f32| canvas.min + vec2(x, y);
            backdrop_painter.set(
                backdrop,
                device_visuals::detail_wallpaper(
                    egui::Rect::from_min_max(canvas.min - vec2(0.0, header.height()), canvas.max),
                    texture.as_ref(),
                    header.height() + card_y - 80.0,
                    header.height() + info_y - 12.0,
                ),
            );
            if texture.is_none() {
                ui.painter().text(
                    at(pad, 18.0),
                    egui::Align2::LEFT_TOP,
                    wallpaper_note,
                    FontId::proportional(crate::ui::theme::COMPACT_TEXT),
                    MUTED,
                );
            }
            let card = egui::Rect::from_min_size(at(pad, card_y), vec2(width - pad * 2.0, card_h));
            ui.painter().rect_filled(card, theme::PANEL_RADIUS, SURFACE);
            ui.painter().rect_stroke(
                card,
                theme::PANEL_RADIUS,
                Stroke::new(1.0, LINE),
                egui::StrokeKind::Inside,
            );
            let tile_side = 88.0;
            let tile = egui::Rect::from_min_size(
                card.min + vec2(20.0, (card_h - tile_side) / 2.0),
                vec2(tile_side, tile_side),
            );
            ui.painter().rect_filled(tile, theme::PANEL_RADIUS, SIDEBAR);
            ui.painter().rect_stroke(
                tile,
                theme::PANEL_RADIUS,
                Stroke::new(1.0, LINE),
                egui::StrokeKind::Inside,
            );
            device_visuals::system_icon(
                ui.painter(),
                tile.shrink(tile_side * 0.16),
                device.platform,
            );
            let title_x = tile.right() + 24.0;
            let mut name = ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_max(
                egui::pos2(title_x, card.top() + 16.0),
                egui::pos2(
                    card.right() - if viewing { 196.0 } else { 20.0 },
                    card.top() + 48.0,
                ),
            )));
            name.add(
                egui::Label::new(
                    RichText::new(display_alias(&device))
                        .size(crate::ui::theme::TITLE)
                        .strong(),
                )
                .truncate(),
            )
            .on_hover_text(display_alias(&device));
            let status = self.device_status(&device);
            crate::ui::controls::device_status_badge(
                ui,
                egui::Rect::from_min_size(
                    egui::pos2(title_x, card.top() + 48.0),
                    theme::DEVICE_STATUS_SIZE.into(),
                ),
                status.label(),
                status.color(),
            );
            let mut summary =
                ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_max(
                    egui::pos2(title_x, card.top() + 88.0),
                    card.max - vec2(15.0, 5.0),
                )));
            let summary_text = (text.detail_summary)(&os, &id, version);
            summary
                .add(
                    egui::Label::new(
                        RichText::new(&summary_text)
                            .size(crate::ui::theme::COMPACT_TEXT)
                            .color(MUTED),
                    )
                    .truncate(),
                )
                .on_hover_text(&summary_text);
            if viewing {
                let rect = egui::Rect::from_min_size(
                    egui::pos2(card.right() - 176.0, card.top() + 28.0),
                    vec2(156.0, theme::CONTROL_HEIGHT),
                );
                let mut button_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                let issue = self.viewer_action_issue(&device);
                let own_session = self.viewer_for_device(&id).is_some();
                let button = detail_button(
                    &mut button_ui,
                    if own_session {
                        text.open_viewer
                    } else {
                        text.connect_device
                    },
                    Glyph::Monitor,
                    !self.mutation_pending && !self.logout_pending && issue.is_none(),
                    rect.size(),
                    ButtonTone::Primary,
                );
                connect = button.clicked();
                if let Some(issue) = issue {
                    button.on_disabled_hover_text(issue);
                }
            }
            let power_actions = [
                if device.is_connected() {
                    crate::account::power::PowerAction::Shutdown
                } else {
                    crate::account::power::PowerAction::Wake
                },
                crate::account::power::PowerAction::Reboot,
            ];
            let power_count = if has_power { power_actions.len() } else { 0 };
            // Row order: power, ports, files, diagnostics, audio only.
            let ports_at = power_count;
            let files_at = ports_at + usize::from(has_ports);
            let diagnostics_at = files_at + usize::from(has_files);
            let viewing_at = diagnostics_at + usize::from(row_diagnostics);
            let action_count = viewing_at + usize::from(viewing);
            let gap = 16.0;
            let button_w = (card.width() - action_count.saturating_sub(1) as f32 * gap)
                / action_count.max(1) as f32;
            if has_power {
                for (i, action) in power_actions.into_iter().enumerate() {
                    let rect = egui::Rect::from_min_size(
                        at(pad + i as f32 * (button_w + gap), power_y),
                        vec2(button_w, theme::CONTROL_HEIGHT),
                    );
                    let mut button_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                    let issue = self.power_available(&device, action).err();
                    let button = detail_button(
                        &mut button_ui,
                        action.label(),
                        if action == crate::account::power::PowerAction::Reboot {
                            Glyph::Restart
                        } else {
                            Glyph::Power
                        },
                        issue.is_none(),
                        rect.size(),
                        ButtonTone::Normal,
                    );
                    if button.clicked() {
                        power = Some(action);
                    }
                    if let Some(issue) = issue {
                        button.on_disabled_hover_text(issue.to_string());
                    }
                }
            }
            if has_files {
                let file_rect = egui::Rect::from_min_size(
                    at(pad + files_at as f32 * (button_w + gap), power_y),
                    vec2(button_w, theme::CONTROL_HEIGHT),
                );
                let transferring = self.devices.as_ref().is_some_and(|list| {
                    crate::features::file_transfer::service::is_transferring(
                        &list.current_device.device_id,
                        &id,
                    )
                });
                let mut file_ui = ui.new_child(egui::UiBuilder::new().max_rect(file_rect));
                files = detail_button(
                    &mut file_ui,
                    text.file_transfer,
                    Glyph::Files,
                    !self.logout_pending
                        && device.is_connected()
                        && device.controllable
                        && device.controlled_support,
                    file_rect.size(),
                    if transferring {
                        ButtonTone::Primary
                    } else {
                        ButtonTone::Normal
                    },
                )
                .clicked();
            }
            if row_diagnostics {
                let rect = egui::Rect::from_min_size(
                    at(pad + diagnostics_at as f32 * (button_w + gap), power_y),
                    vec2(button_w, theme::CONTROL_HEIGHT),
                );
                let mut button_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                diagnostics |= diagnostics_button(
                    &mut button_ui,
                    rect.size(),
                    &controller_id,
                    &id,
                    diagnostic_state.as_ref(),
                    &device,
                );
            }
            if has_ports {
                let rect = egui::Rect::from_min_size(
                    at(pad + ports_at as f32 * (button_w + gap), power_y),
                    vec2(button_w, theme::CONTROL_HEIGHT),
                );
                let mut button_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                let service = self.devices.as_ref().and_then(|list| {
                    crate::features::port_mapping::service::status(
                        &list.current_device.device_id,
                        &id,
                    )
                });
                let running = service.as_ref().is_some_and(|s| s.enabled);
                ui.ctx().request_repaint_after(Duration::from_millis(200));
                let available = !self.logout_pending
                    && (service.is_some()
                        || (device.is_connected()
                            && device.controllable
                            && device.controlled_support));
                ports = detail_button(
                    &mut button_ui,
                    text.port_forwarding,
                    Glyph::PortMapping,
                    available,
                    rect.size(),
                    if running {
                        ButtonTone::Primary
                    } else {
                        ButtonTone::Normal
                    },
                )
                .on_disabled_hover_text(text.offline_or_not_allowed)
                .clicked();
            }
            if viewing {
                let rect = egui::Rect::from_min_size(
                    at(pad + viewing_at as f32 * (button_w + gap), power_y),
                    vec2(button_w, theme::CONTROL_HEIGHT),
                );
                let mut button_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect));
                let issue = self.viewer_action_issue(&device);
                let button = detail_button(
                    &mut button_ui,
                    text.audio_only,
                    Glyph::Headphones,
                    !self.mutation_pending && !self.logout_pending && issue.is_none(),
                    rect.size(),
                    ButtonTone::Normal,
                );
                audio_only = button.clicked();
                if let Some(issue) = issue {
                    button.on_disabled_hover_text(issue);
                }
            }
            let left = card.left() + 12.0;
            let middle = card.center().x;
            let right = middle + 24.0;
            let left_w = middle - left - 24.0;
            let right_w = card.right() - right - 12.0;
            ui.painter().text(
                egui::pos2(left, canvas.top() + info_y),
                egui::Align2::LEFT_TOP,
                text.hardware_info,
                FontId::proportional(crate::ui::theme::SECTION),
                TEXT,
            );
            ui.painter().text(
                egui::pos2(right, canvas.top() + info_y),
                egui::Align2::LEFT_TOP,
                text.device_info,
                FontId::proportional(crate::ui::theme::SECTION),
                TEXT,
            );
            for (i, hardware) in Hardware::ALL.into_iter().enumerate() {
                let value = fields
                    .and_then(|d| d.details.iter().find(|(row, _)| *row == hardware.row()))
                    .map(|(_, v)| v.as_str())
                    .unwrap_or(if detail.is_some() {
                        text.not_provided
                    } else if self.server_unreachable {
                        // Read once the server is back.
                        "—"
                    } else {
                        text.reading
                    });
                let rect = egui::Rect::from_min_size(
                    egui::pos2(left, canvas.top() + info_y + 28.0 + i as f32 * row_h),
                    vec2(left_w, row_h),
                );
                specification(
                    ui,
                    rect,
                    hardware.label(),
                    value,
                    hardware.glyph(),
                    false,
                    Some(hardware),
                );
            }
            for (i, (label, value, glyph, copy)) in [
                (text.operating_system, os.as_str(), Glyph::System, false),
                (text.device_id, id.as_str(), Glyph::Monitor, true),
                (text.client_version, version, Glyph::Version, false),
            ]
            .into_iter()
            .enumerate()
            {
                let rect = egui::Rect::from_min_size(
                    egui::pos2(right, canvas.top() + info_y + 28.0 + i as f32 * row_h),
                    vec2(right_w, row_h),
                );
                specification(ui, rect, label, value, glyph, copy, None);
            }
            ui.painter().line_segment(
                [
                    egui::pos2(middle, canvas.top() + info_y),
                    egui::pos2(middle, canvas.top() + table_bottom),
                ],
                Stroke::new(1.0, LINE),
            );
            if owned {
                ui.painter().hline(
                    card.left()..=card.right(),
                    canvas.top() + divider_y,
                    Stroke::new(1.0, LINE),
                );
                let heading = ui.painter().text(
                    egui::pos2(
                        card.left() + 8.0,
                        canvas.top() + footer_y + theme::CONTROL_HEIGHT / 2.0,
                    ),
                    egui::Align2::LEFT_CENTER,
                    text.device_management,
                    FontId::proportional(crate::ui::theme::SECTION),
                    TEXT,
                );
                // The buttons start after the heading, however long it is.
                let inset = (heading.right() - card.left() + 16.0).max(108.0);
                let mut footer = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(egui::Rect::from_min_size(
                            at(pad + inset, footer_y),
                            vec2(card.width() - inset - 8.0, theme::CONTROL_HEIGHT),
                        ))
                        .layout(egui::Layout::left_to_right(Align::Center)),
                );
                let diagnostics_label =
                    super::remote_diagnostics::button_label(diagnostic_state.as_ref());
                footer.spacing_mut().item_spacing.x = 16.0;
                footer.add_enabled_ui(!self.mutation_pending && !self.logout_pending, |ui| {
                    let rename = detail_button(
                        ui,
                        text.rename_device,
                        Glyph::Edit,
                        reason.is_none(),
                        vec2(footer_width(ui, text.rename_device), theme::CONTROL_HEIGHT),
                        ButtonTone::Normal,
                    );
                    if let Some(reason) = reason {
                        rename.clone().on_disabled_hover_text(reason);
                    }
                    if rename.clicked() {
                        edit = Some(EditAction::Rename);
                    }
                    if has_diagnostics {
                        diagnostics |= diagnostics_button(
                            ui,
                            vec2(footer_width(ui, &diagnostics_label), theme::CONTROL_HEIGHT),
                            &controller_id,
                            &id,
                            diagnostic_state.as_ref(),
                            &device,
                        );
                    }
                    if current {
                        if detail_button(
                            ui,
                            t().common.sign_out,
                            Glyph::Trash,
                            true,
                            vec2(footer_width(ui, t().common.sign_out), theme::CONTROL_HEIGHT),
                            ButtonTone::Danger,
                        )
                        .clicked()
                        {
                            exit_account = true;
                        }
                    } else {
                        let remove = detail_button(
                            ui,
                            text.remove_from_account,
                            Glyph::Trash,
                            reason.is_none(),
                            vec2(footer_width(ui, text.remove_from_account), theme::CONTROL_HEIGHT),
                            ButtonTone::Danger,
                        );
                        if let Some(reason) = reason {
                            remove.clone().on_disabled_hover_text(reason);
                        }
                        if remove.clicked() {
                            edit = Some(EditAction::Remove);
                        }
                    }
                });
            }
            crate::ui::controls::progress_notice(
                ui.ctx(),
                "device-operation-progress",
                text.device_operation,
                self.mutation_pending.then_some(text.processing_device_operation),
            );
        });
        if diagnostics {
            self.fetch_diagnostics(device.clone());
        }
        if ports {
            self.open_port_mapping(id.clone());
        }
        if files {
            self.open_file_transfer(id.clone());
        }
        if connect || audio_only {
            self.selected_device_id = Some(id);
            self.start_viewer_mode(audio_only);
        }
        if let Some(action) = edit {
            self.center_ui.edit = Some(DeviceEdit {
                alias: device.alias.clone(),
                device: device.clone(),
                action,
            });
        }
        if let Some(action) = power {
            self.begin_power(device, action);
        }
        if exit_account {
            self.logout();
        }
    }
}
