//! The stream menu of a camera tab (`docs/protocol/camera-design.md` §6.1):
//! its parameters (the owner's or this viewer's own, remembered for the
//! device) and its picture adjustments. Changes apply at once.
use super::{menu_row, section_separator, switch_row};
use crate::features::stream_control::StreamControlHandle;
use crate::features::stream_control::camera::{CameraControlKind, CameraInfo};
use crate::i18n::t;
use crate::media::camera::{Control, Matrix, Mode, Params};
use egui::RichText;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum CameraPage {
    #[default]
    Root,
    Mode,
    Color,
    Rotation,
    Bitrate,
    Adjust,
}

impl CameraPage {
    pub(super) fn title(self) -> &'static str {
        let text = &t().camera;
        match self {
            Self::Root => text.menu_title,
            Self::Mode => text.mode,
            Self::Color => text.color,
            Self::Rotation => text.rotation,
            Self::Bitrate => text.bitrate,
            Self::Adjust => text.adjustments,
        }
    }
}

/// Bitrate ceilings offered (kbit/s); `None` is by resolution and rate.
const BITRATES: [u32; 10] = [500, 1_000, 1_500, 2_000, 3_000, 4_000, 5_000, 8_000, 12_000, 20_000];

fn mode_text(mode: &Mode) -> String {
    (t().camera.mode_value)(mode.width, mode.height, mode.fps(), mode.format.label())
}

fn kbps_text(kbps: u32) -> String {
    let mbps = f64::from(kbps) / 1000.0;
    if mbps.fract() == 0.0 {
        format!("{mbps:.0} Mbps")
    } else {
        format!("{mbps:.1} Mbps")
    }
}

fn color_text(params: &Params) -> String {
    let text = &t().camera;
    match (params.color.matrix, params.color.full_range) {
        (None, None) => text.automatic.to_owned(),
        (matrix, range) => format!(
            "{} · {}",
            match matrix {
                Some(Matrix::Bt601) => "BT.601",
                Some(Matrix::Bt709) => "BT.709",
                None => text.automatic,
            },
            match range {
                Some(true) => text.full_range,
                Some(false) => text.limited_range,
                None => text.automatic,
            }
        ),
    }
}

fn control_label(control: Control) -> &'static str {
    let text = &t().camera;
    match control {
        Control::Brightness => text.brightness,
        Control::Contrast => text.contrast,
        Control::Hue => text.hue,
        Control::Saturation => text.saturation,
        Control::Sharpness => text.sharpness,
        Control::Gamma => text.gamma,
        Control::WhiteBalance => text.white_balance,
        Control::BacklightCompensation => text.backlight,
        Control::Gain => text.gain,
        Control::PowerLineFrequency => text.anti_flicker,
        Control::Exposure => text.exposure,
        Control::Focus => text.focus,
        Control::Zoom => text.zoom,
    }
}

/// The parameters in effect for this viewer: running, its own, the owner's.
fn current(info: &CameraInfo) -> Params {
    info.running
        .or(info.custom)
        .or(info.owner)
        .map(|p| p.params())
        .unwrap_or_default()
}

/// Draw `page` of camera `id`; returns a page to go to.
pub(super) fn draw(
    ui: &mut egui::Ui,
    handle: &StreamControlHandle,
    id: i32,
    page: CameraPage,
    error: &mut Option<String>,
) -> Option<CameraPage> {
    let Some(info) = handle.cameras().into_iter().find(|c| c.id == id) else {
        ui.label(RichText::new(t().camera.not_found).color(crate::ui::theme::MUTED));
        return None;
    };
    let mut next = None;
    let params = current(&info);
    let mut apply = |change: &dyn Fn(&mut Params)| {
        let mut params = info.custom.or(info.owner).map(|p| p.params()).unwrap_or_default();
        if params.mode.is_none() {
            params.mode = current(&info).mode;
        }
        change(&mut params);
        if let Err(e) = handle.set_camera_params(id, Some(params)) {
            *error = Some(e.to_string());
        }
    };
    match page {
        CameraPage::Root => {
            let status = if !info.unavailable.is_empty() {
                Some(info.unavailable.clone())
            } else if !info.chosen_by.is_empty() {
                Some((t().camera.chosen_by)(&info.chosen_by))
            } else {
                handle.camera_error()
            };
            if let Some(status) = status {
                ui.label(RichText::new(status).size(crate::ui::theme::SMALL).color(crate::ui::theme::MUTED));
                section_separator(ui);
            }
            let mode = params.mode.as_ref().map_or_else(String::new, mode_text);
            if menu_row(ui, t().camera.mode, &mode, None, !info.modes.is_empty(), true).clicked() {
                next = Some(CameraPage::Mode);
            }
            if menu_row(ui, t().camera.color, &color_text(&params), None, true, true).clicked() {
                next = Some(CameraPage::Color);
            }
            let mut mirror = params.orientation.mirror;
            if switch_row(ui, t().camera.mirror, &mut mirror).changed() {
                apply(&|p| p.orientation.mirror = mirror);
            }
            let rotation = format!("{}°", u32::from(params.orientation.quarter_turns) * 90);
            if menu_row(ui, t().camera.rotation, &rotation, None, true, true).clicked() {
                next = Some(CameraPage::Rotation);
            }
            let bitrate = params
                .bitrate_kbps
                .map_or_else(|| t().camera.automatic.to_owned(), kbps_text);
            if menu_row(ui, t().camera.bitrate, &bitrate, None, true, true).clicked() {
                next = Some(CameraPage::Bitrate);
            }
            if menu_row(ui, t().camera.adjustments, "", None, !info.controls.is_empty(), true).clicked() {
                next = Some(CameraPage::Adjust);
            }
            let mut listening = handle.listening();
            if ui
                .add_enabled_ui(super::granted(handle, farwing_protocol::caps::LISTEN), |ui| {
                    switch_row(ui, t().camera.listen, &mut listening)
                })
                .inner
                .changed()
                && let Err(e) = handle.set_listening(listening)
            {
                *error = Some(e.to_string());
            }
            section_separator(ui);
            if menu_row(ui, t().camera.owner_settings, "", None, info.custom.is_some(), false).clicked()
                && let Err(e) = handle.set_camera_params(id, None)
            {
                *error = Some(e.to_string());
            }
        }
        CameraPage::Mode => {
            egui::ScrollArea::vertical()
                .id_salt("camera-modes")
                .max_height(320.0)
                .show(ui, |ui| {
                    for mode in info.modes.iter().filter_map(|m| m.mode()) {
                        if menu_row(ui, &mode_text(&mode), "", Some(params.mode == Some(mode)), true, false).clicked()
                            && params.mode != Some(mode)
                        {
                            apply(&|p| p.mode = Some(mode));
                        }
                    }
                });
        }
        CameraPage::Color => {
            let choices = [
                (None, None),
                (Some(Matrix::Bt601), Some(false)),
                (Some(Matrix::Bt601), Some(true)),
                (Some(Matrix::Bt709), Some(false)),
                (Some(Matrix::Bt709), Some(true)),
            ];
            for (matrix, full_range) in choices {
                let mut choice = params;
                choice.color.matrix = matrix;
                choice.color.full_range = full_range;
                let selected = params.color == choice.color;
                if menu_row(ui, &color_text(&choice), "", Some(selected), true, false).clicked() && !selected {
                    apply(&|p| p.color = choice.color);
                }
            }
        }
        CameraPage::Rotation => {
            for turns in 0..4u8 {
                let selected = params.orientation.quarter_turns == turns;
                if menu_row(ui, &format!("{}°", u32::from(turns) * 90), "", Some(selected), true, false).clicked()
                    && !selected
                {
                    apply(&|p| p.orientation.quarter_turns = turns);
                }
            }
        }
        CameraPage::Bitrate => {
            let automatic = params.mode.map_or_else(String::new, |m| {
                kbps_text(crate::media::camera::default_bitrate_kbps(m.width, m.height, m.fps()))
            });
            if menu_row(ui, t().camera.automatic, &automatic, Some(params.bitrate_kbps.is_none()), true, false).clicked()
                && params.bitrate_kbps.is_some()
            {
                apply(&|p| p.bitrate_kbps = None);
            }
            for kbps in BITRATES {
                let selected = params.bitrate_kbps == Some(kbps);
                if menu_row(ui, &kbps_text(kbps), "", Some(selected), true, false).clicked() && !selected {
                    apply(&|p| p.bitrate_kbps = Some(kbps));
                }
            }
        }
        CameraPage::Adjust => adjustments(ui, handle, &info, error),
    }
    next
}

fn adjustments(ui: &mut egui::Ui, handle: &StreamControlHandle, info: &CameraInfo, error: &mut Option<String>) {
    let mut send = |control: Control, value: i32, auto: bool| {
        if let Err(e) = handle.adjust_camera(info.id, control, value, auto) {
            *error = Some(e.to_string());
        }
    };
    egui::ScrollArea::vertical()
        .id_salt("camera-adjustments")
        .max_height(360.0)
        .show(ui, |ui| {
            for state in &info.controls {
                let Ok(kind) = CameraControlKind::try_from(state.control) else {
                    continue;
                };
                let control = Control::from(kind);
                let editing = egui::Id::new(("camera-adjust", info.id, state.control));
                let mut value = ui.data(|d| d.get_temp::<i32>(editing)).unwrap_or(state.value);
                ui.horizontal(|ui| {
                    ui.label(control_label(control));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if state.can_auto {
                            let mut auto = state.auto;
                            if ui.checkbox(&mut auto, t().camera.automatic).changed() {
                                send(control, value, auto);
                            }
                        }
                        ui.label(RichText::new(value.to_string()).color(crate::ui::theme::MUTED));
                    });
                });
                let response = ui
                    .scope(|ui| {
                        ui.spacing_mut().slider_width = ui.available_width();
                        ui.spacing_mut().slider_rail_height = 3.0;
                        ui.add_enabled(
                            !state.auto,
                            egui::Slider::new(&mut value, state.minimum..=state.maximum)
                                .step_by(f64::from(state.step))
                                .show_value(false)
                                .trailing_fill(true)
                                .handle_shape(egui::style::HandleShape::Circle),
                        )
                    })
                    .inner;
                if response.dragged() {
                    ui.data_mut(|d| d.insert_temp(editing, value));
                } else {
                    ui.data_mut(|d| d.remove::<i32>(editing));
                }
                // Sent when let go (or clicked), not for every step of a drag.
                if (response.drag_stopped() || (response.changed() && !response.dragged())) && value != state.value {
                    send(control, value, false);
                }
                ui.add_space(4.0);
            }
            section_separator(ui);
            if menu_row(ui, t().camera.reset_adjustments, "", None, true, false).clicked() {
                for state in &info.controls {
                    if let Ok(kind) = CameraControlKind::try_from(state.control) {
                        send(Control::from(kind), state.default, state.can_auto);
                    }
                }
            }
        });
}
