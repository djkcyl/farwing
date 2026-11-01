//! Host settings → cameras and microphone (`docs/protocol/camera-design.md`
//! §5): whether others may see the cameras and hear the microphone, and the
//! owner's parameters for each camera.
use super::*;
use crate::i18n::t;
use crate::media::camera::{Camera, Matrix, Mode, Params};
use std::sync::{Arc, Mutex};

/// The camera list is read again when the page shows after this.
const REREAD: Duration = Duration::from_secs(30);
const COMBO_WIDTH: f32 = 238.0;
const BITRATES: [u32; 10] = [500, 1_000, 1_500, 2_000, 3_000, 4_000, 5_000, 8_000, 12_000, 20_000];

#[derive(Default)]
pub(super) struct CameraSettingsUi {
    cameras: Arc<Mutex<Option<Vec<Camera>>>>,
    /// Recording devices: endpoint ID and name.
    microphones: Arc<Mutex<Vec<(String, String)>>>,
    read_at: Option<Instant>,
    editing: Option<Editor>,
}

/// The parameters dialog of one camera.
struct Editor {
    camera: Camera,
    custom: bool,
    params: Params,
}

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

fn color_text(matrix: Option<Matrix>, full_range: Option<bool>) -> String {
    let text = &t().camera;
    match (matrix, full_range) {
        (None, None) => text.automatic.to_owned(),
        _ => format!(
            "{} · {}",
            match matrix {
                Some(Matrix::Bt601) => "BT.601",
                Some(Matrix::Bt709) => "BT.709",
                None => text.automatic,
            },
            match full_range {
                Some(true) => text.full_range,
                Some(false) => text.limited_range,
                None => text.automatic,
            }
        ),
    }
}

impl DeviceCenterApp {
    pub(super) fn host_camera_settings(&mut self, ui: &mut egui::Ui, host: &crate::features::host::Handle) {
        let text = &t().settings;
        section(ui, text.section_camera);
        let mut settings = host.camera_settings();
        let before = settings.clone();
        form_row(ui, text.allow_camera, "", |ui| {
            crate::ui::controls::switch(ui, &mut settings.allowed);
        });
        form_row(ui, text.allow_listen, "", |ui| {
            crate::ui::controls::switch(ui, &mut settings.listen);
        });
        let state = &mut self.center_ui.host_cameras;
        if state.read_at.is_none_or(|at| at.elapsed() >= REREAD) {
            state.read_at = Some(Instant::now());
            let cameras = state.cameras.clone();
            let microphones = state.microphones.clone();
            let context = ui.ctx().clone();
            let _ = std::thread::Builder::new().name("camera-settings".into()).spawn(move || {
                use cpal::traits::{DeviceTrait, HostTrait};
                let found = cpal::default_host()
                    .input_devices()
                    .map(|devices| {
                        devices
                            .filter_map(|d| Some((d.id().ok()?.to_string(), d.description().ok()?.name().to_owned())))
                            .collect()
                    })
                    .unwrap_or_default();
                *microphones.lock().unwrap_or_else(|e| e.into_inner()) = found;
                let read = crate::features::host::cameras::read();
                *cameras.lock().unwrap_or_else(|e| e.into_inner()) = Some(read);
                context.request_repaint();
            });
        }
        if settings.listen {
            let microphones = state.microphones.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let selected = settings
                .microphone
                .as_ref()
                .map(|id| {
                    microphones
                        .iter()
                        .find(|(m, _)| m == id)
                        .map_or_else(|| id.clone(), |(_, name)| name.clone())
                })
                .unwrap_or_else(|| text.system_default_device.to_owned());
            form_row(ui, text.listen_microphone, "", |ui| {
                egui::ComboBox::from_id_salt("host-listen-microphone")
                    .width(COMBO_WIDTH)
                    .truncate()
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut settings.microphone, None, text.system_default_device);
                        for (id, name) in &microphones {
                            ui.selectable_value(&mut settings.microphone, Some(id.clone()), name);
                        }
                    });
            });
        }
        let cameras = state.cameras.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match cameras {
            None => form_row(ui, text.cameras, t().common.loading, |_| {}),
            Some(list) if list.is_empty() => form_row(ui, text.cameras, text.no_cameras, |_| {}),
            Some(list) => {
                for camera in list {
                    let summary = match settings.defaults.get(&camera.id) {
                        None => text.camera_default.to_owned(),
                        Some(params) => match params.mode {
                            Some(mode) => format!("{} · {}", text.camera_custom, mode_text(&mode)),
                            None => text.camera_custom.to_owned(),
                        },
                    };
                    let summary = camera.unavailable.clone().unwrap_or(summary);
                    let mut open = false;
                    form_row(ui, &camera.name, &summary, |ui| {
                        open = ui
                            .add_enabled(
                                camera.unavailable.is_none() && !camera.modes.is_empty(),
                                crate::ui::controls::secondary(text.camera_settings),
                            )
                            .clicked();
                    });
                    if open {
                        let saved = settings.defaults.get(&camera.id).copied();
                        let mut params = saved.unwrap_or_default();
                        if params.mode.is_none() {
                            params.mode = crate::media::camera::default_mode(&camera.modes);
                        }
                        self.center_ui.host_cameras.editing = Some(Editor {
                            camera,
                            custom: saved.is_some(),
                            params,
                        });
                    }
                }
            }
        }
        if let Some(result) = self.camera_editor(ui.ctx()) {
            match result {
                (id, Some(params)) => settings.defaults.insert(id, params),
                (id, None) => settings.defaults.remove(&id),
            };
        }
        if settings != before {
            match host.set_camera_settings(settings) {
                Ok(()) => self.save_host_settings(),
                Err(error) => self.status = StatusMessage::error(error.to_string()),
            }
        }
    }

    /// The open parameters dialog; a saved choice when it closes with Save.
    fn camera_editor(&mut self, ctx: &egui::Context) -> Option<(String, Option<Params>)> {
        let editor = self.center_ui.host_cameras.editing.as_mut()?;
        let text = &t().settings;
        let camera_text = &t().camera;
        let mut result = None;
        let mut close = false;
        egui::Modal::new(egui::Id::new("host-camera-editor"))
            .frame(crate::ui::controls::dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(420.0);
                close |= crate::ui::controls::dialog_header(
                    ui,
                    &editor.camera.name,
                    crate::ui::controls::DialogIcon::Info,
                    true,
                );
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut editor.custom, false, text.camera_default);
                    ui.selectable_value(&mut editor.custom, true, text.camera_custom);
                });
                ui.add_space(8.0);
                ui.add_enabled_ui(editor.custom, |ui| {
                    egui::Grid::new("host-camera-params")
                        .num_columns(2)
                        .spacing(egui::vec2(12.0, 8.0))
                        .show(ui, |ui| {
                            let params = &mut editor.params;
                            ui.label(camera_text.mode);
                            egui::ComboBox::from_id_salt("host-camera-mode")
                                .width(COMBO_WIDTH)
                                .selected_text(params.mode.as_ref().map_or_else(String::new, mode_text))
                                .show_ui(ui, |ui| {
                                    for mode in &editor.camera.modes {
                                        ui.selectable_value(&mut params.mode, Some(*mode), mode_text(mode));
                                    }
                                });
                            ui.end_row();
                            ui.label(camera_text.color);
                            egui::ComboBox::from_id_salt("host-camera-color")
                                .width(COMBO_WIDTH)
                                .selected_text(color_text(params.color.matrix, params.color.full_range))
                                .show_ui(ui, |ui| {
                                    for (matrix, range) in [
                                        (None, None),
                                        (Some(Matrix::Bt601), Some(false)),
                                        (Some(Matrix::Bt601), Some(true)),
                                        (Some(Matrix::Bt709), Some(false)),
                                        (Some(Matrix::Bt709), Some(true)),
                                    ] {
                                        let chosen = params.color.matrix == matrix && params.color.full_range == range;
                                        if ui.selectable_label(chosen, color_text(matrix, range)).clicked() {
                                            params.color.matrix = matrix;
                                            params.color.full_range = range;
                                        }
                                    }
                                });
                            ui.end_row();
                            ui.label(camera_text.mirror);
                            crate::ui::controls::switch(ui, &mut params.orientation.mirror);
                            ui.end_row();
                            ui.label(camera_text.rotation);
                            egui::ComboBox::from_id_salt("host-camera-rotation")
                                .width(COMBO_WIDTH)
                                .selected_text(format!("{}°", u32::from(params.orientation.quarter_turns) * 90))
                                .show_ui(ui, |ui| {
                                    for turns in 0..4u8 {
                                        ui.selectable_value(
                                            &mut params.orientation.quarter_turns,
                                            turns,
                                            format!("{}°", u32::from(turns) * 90),
                                        );
                                    }
                                });
                            ui.end_row();
                            ui.label(camera_text.bitrate);
                            egui::ComboBox::from_id_salt("host-camera-bitrate")
                                .width(COMBO_WIDTH)
                                .selected_text(
                                    params
                                        .bitrate_kbps
                                        .map_or_else(|| camera_text.automatic.to_owned(), kbps_text),
                                )
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut params.bitrate_kbps, None, camera_text.automatic);
                                    for kbps in BITRATES {
                                        ui.selectable_value(&mut params.bitrate_kbps, Some(kbps), kbps_text(kbps));
                                    }
                                });
                            ui.end_row();
                        });
                });
                let (save, cancel) = crate::ui::controls::dialog_actions(
                    ui,
                    Some(crate::ui::controls::DialogAction::new(t().common.save)),
                    Some(t().common.cancel),
                );
                if save {
                    result = Some((editor.camera.id.clone(), editor.custom.then_some(editor.params)));
                    close = true;
                }
                close |= cancel;
            });
        if close {
            self.center_ui.host_cameras.editing = None;
        }
        result
    }
}
