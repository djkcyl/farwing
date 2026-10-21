use super::*;
use crate::features::host::format::{Backend, Codec};
use crate::i18n::t;
use crate::ui::controls::{diagnostics_empty, diagnostics_row, diagnostics_table};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Tab {
    #[default]
    Device,
    Publication,
    Encoding,
    Decoding,
    Sessions,
    Wol,
}
#[derive(Default)]
pub(super) struct ViewState {
    tab: Tab,
    encoder: Option<(u64, Backend)>,
    decoder: usize,
    publication_refresh: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    publication_error: Option<String>,
    wol_refresh: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    wol_error: Option<String>,
}

impl DeviceCenterApp {
    pub(super) fn diagnostics_page(&mut self, ui: &mut egui::Ui) {
        self.diagnostic_export(ui);
        ui.add_space(12.0);
        crate::ui::controls::page_scroll("center-diagnostics-scroll").show(ui, |ui| {
            self.diagnostics_panel(ui);
        });
        self.wol_setup_dialog(ui.ctx());
    }

    fn diagnostics_panel(&mut self, ui: &mut egui::Ui) {
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing = vec2(theme::DIAGNOSTICS_GAP, 4.0);
            ui.horizontal(|ui| {
                let text = &t().settings;
                for (tab, label) in [
                    (Tab::Device, text.diag_tab_overview),
                    (Tab::Publication, text.diag_tab_registration),
                    (Tab::Encoding, text.diag_tab_encoding),
                    (Tab::Decoding, text.diag_tab_decoding),
                    (Tab::Sessions, text.diag_tab_sessions),
                    (Tab::Wol, text.wol_remote_wake),
                ] {
                    ui.selectable_value(&mut self.center_ui.diagnostics.tab, tab, label);
                }
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if ui.link(text.tab_logs).clicked() {
                        self.center_ui.settings_tab = super::settings::SettingsTab::Logs;
                    }
                });
            });
            ui.separator();
            let tab = self.center_ui.diagnostics.tab;
            ui.allocate_ui_with_layout(
                vec2(ui.available_width(), theme::DIAGNOSTICS_BODY_MIN_HEIGHT),
                egui::Layout::top_down(Align::Min),
                |ui| {
                    ui.set_min_height(theme::DIAGNOSTICS_BODY_MIN_HEIGHT);
                    match tab {
                        Tab::Device => self.diagnostic_device(ui),
                        Tab::Publication => self.diagnostic_publication(ui),
                        Tab::Encoding => self.diagnostic_encoding(ui),
                        Tab::Decoding => self.diagnostic_decoding(ui),
                        Tab::Sessions => self.diagnostic_sessions(ui),
                        Tab::Wol => self.diagnostic_wol(ui),
                    }
                },
            );
        });
    }

    fn diagnostic_wol(&mut self, ui: &mut egui::Ui) {
        self.wol_setup_entry(ui);
        ui.separator();
        let text = &t().settings;
        let state = &mut self.center_ui.diagnostics;
        if let Some(rx) = &state.wol_refresh {
            match rx.try_recv() {
                Ok(result) => {
                    state.wol_error = result.err();
                    state.wol_refresh = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    state.wol_error = Some(text.refresh_interrupted.into());
                    state.wol_refresh = None;
                }
                Err(_) => {}
            }
        }
        let Some(host) = self.host.clone() else {
            diagnostics_empty(ui, text.host_off);
            return;
        };
        let wol = host.status().wol;
        ui.horizontal(|ui| {
            ui.label(text.lan_registration);
            if ui
                .add_enabled(
                    host.allowed()
                        && (host.wol_allowed() || host.status().wol_setup.enabled == Some(true))
                        && !wol.reporting
                        && state.wol_refresh.is_none(),
                    egui::Button::new(text.refresh_registration),
                )
                .clicked()
            {
                let (tx, rx) = std::sync::mpsc::channel();
                state.wol_refresh = Some(rx);
                state.wol_error = None;
                let ctx = ui.ctx().clone();
                std::thread::spawn(move || {
                    use crate::platform::windows::host_service::resident;
                    let result = if resident::managed() {
                        resident::call(resident::Request::RefreshWol).map(|_| ())
                    } else {
                        host.wol.refresh();
                        Ok(())
                    };
                    let _ = tx.send(result.map_err(|e| e.to_string()));
                    ctx.request_repaint();
                });
            }
        });
        diagnostics_row(
            ui,
            text.status,
            if wol.message.is_empty() {
                text.wol_assistance_off
            } else {
                &wol.message
            },
        );
        if let Some(name) = &wol.interface_name {
            diagnostics_row(ui, text.adapter, name);
        }
        if let Some(info) = &wol.network {
            diagnostics_row(ui, text.ipv4_address, &info.inner_ip);
            diagnostics_row(ui, text.subnet_mask, &info.subnet_mask);
            diagnostics_row(ui, "MAC", &info.mac);
        }
        let stamp = |v: Option<i64>| {
            v.and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|t| {
                    t.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M:%S")
                        .to_string()
                })
                .unwrap_or_else(|| text.no_record.into())
        };
        diagnostics_row(ui, text.last_registered, &stamp(wol.registered_at));
        diagnostics_row(ui, text.last_sent, &stamp(wol.last_sent_at));
        diagnostics_row(ui, text.packets_sent, &wol.packets_sent.to_string());
        if let Some(error) = wol.error.as_ref().or(state.wol_error.as_ref()) {
            ui.colored_label(theme::AMBER, error);
        }
    }

    fn diagnostic_device(&self, ui: &mut egui::Ui) {
        let text = &t().settings;
        diagnostics_row(
            ui,
            text.display,
            &format!(
                "{} × {} · {} Hz",
                self.local_display.width, self.local_display.height, self.local_display.refresh_hz
            ),
        );
        for (row, value) in &self.diagnostics.rows {
            diagnostics_row(ui, row.label(), value);
        }
        for value in &self.diagnostics.graphics {
            diagnostics_row(ui, text.render_device, value);
        }
    }

    fn diagnostic_publication(&mut self, ui: &mut egui::Ui) {
        use crate::account::reporting;
        let text = &t().settings;
        let state = &mut self.center_ui.diagnostics;
        if let Some(rx) = &state.publication_refresh {
            match rx.try_recv() {
                Ok(result) => {
                    state.publication_error = result.err();
                    state.publication_refresh = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    state.publication_error = Some(text.refresh_interrupted.into());
                    state.publication_refresh = None;
                }
                Err(_) => {}
            }
        }
        ui.horizontal(|ui| {
            ui.label(text.registration_and_wallpaper);
            if ui
                .add_enabled(
                    state.publication_refresh.is_none(),
                    egui::Button::new(text.report_again),
                )
                .clicked()
            {
                let (tx, rx) = std::sync::mpsc::channel();
                state.publication_refresh = Some(rx);
                state.publication_error = None;
                let context = ui.ctx().clone();
                std::thread::spawn(move || {
                    use crate::platform::windows::host_service::resident;
                    let result = if resident::managed() {
                        resident::call(resident::Request::RefreshPublication).map(|_| ())
                    } else {
                        reporting::REFRESH.notify_one();
                        Ok(())
                    };
                    let _ = tx.send(result.map_err(|e| format!("{e:#}")));
                    context.request_repaint();
                });
            }
        });
        if let Some(error) = &state.publication_error {
            ui.colored_label(theme::RED, error);
        }
        ui.add_space(theme::DIAGNOSTICS_GAP);
        let snapshot = reporting::snapshot();
        let value = |s: &str| {
            if s.is_empty() {
                text.not_read.to_owned()
            } else {
                s.to_owned()
            }
        };
        let time = |stamp: Option<i64>| {
            stamp
                .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                .map(|t| {
                    t.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M:%S")
                        .to_string()
                })
                .unwrap_or_else(|| text.not_confirmed.into())
        };
        diagnostics_row(ui, text.registered_device, &value(&snapshot.device_id));
        diagnostics_row(ui, text.client_id, &value(&snapshot.client_id));
        diagnostics_row(ui, text.hardware_report, &value(&snapshot.registration));
        diagnostics_row(ui, text.reported_at, &time(snapshot.registered_at));
        diagnostics_row(ui, text.reported_name, &value(&snapshot.reported_name));
        diagnostics_row(
            ui,
            text.host_allowed,
            if snapshot.reported_controllable {
                text.allowed
            } else {
                text.disallowed
            },
        );
        if let Some(hardware) = snapshot.hardware {
            diagnostics_row(ui, text.system_uuid, &hardware.system_uuid);
            diagnostics_row(ui, text.windows_id, &hardware.machine_guid);
            for (label, fact) in [
                (text.computer_name, hardware.name),
                (text.operating_system, hardware.os),
                (text.motherboard, hardware.base_board),
                (text.processor, hardware.cpu),
                (text.graphics, hardware.video.join(" / ")),
                (text.mac_address, hardware.mac),
                (text.active_display, hardware.screen),
            ] {
                diagnostics_row(
                    ui,
                    label,
                    &if fact.is_empty() {
                        text.not_provided.into()
                    } else {
                        fact
                    },
                );
            }
            diagnostics_row(
                ui,
                text.memory,
                &if hardware.memory > 0 {
                    format!("{} MiB", hardware.memory)
                } else {
                    text.not_provided.into()
                },
            );
            for error in hardware.errors {
                ui.colored_label(theme::AMBER, error);
            }
        }
        ui.separator();
        diagnostics_row(ui, text.server_readback, &value(&snapshot.readback));
        diagnostics_row(ui, text.read_back_at, &time(snapshot.readback_at));
        for (row, detail) in &snapshot.server_details {
            diagnostics_row(ui, row.label(), detail);
        }
        ui.separator();
        diagnostics_row(ui, text.wallpaper, &value(&snapshot.wallpaper));
        diagnostics_row(ui, text.image_source, &value(&snapshot.wallpaper_file));
        diagnostics_row(ui, text.confirmed_at, &time(snapshot.wallpaper_at));
        if !snapshot.wallpaper_url.is_empty() {
            if let Some(texture) = self.center_ui.wallpapers.texture(
                ui.ctx(),
                &snapshot.device_id,
                &snapshot.wallpaper_url,
            ) {
                let width = ui.available_width().min(640.0);
                ui.add(
                    egui::Image::new(&texture).fit_to_exact_size(vec2(width, width * 9.0 / 16.0)),
                );
            }
        }
    }

    fn diagnostic_encoding(&mut self, ui: &mut egui::Ui) {
        let text = &t().settings;
        let Some(caps) = self.host.as_ref().and_then(|host| host.capabilities()) else {
            diagnostics_empty(ui, text.no_encoding);
            return;
        };
        let mut groups = Vec::new();
        for cap in &caps.codecs {
            let key = (cap.adapter, cap.backend);
            if !groups.contains(&key) {
                groups.push(key);
            }
        }
        if self
            .center_ui
            .diagnostics
            .encoder
            .is_none_or(|key| !groups.contains(&key))
        {
            self.center_ui.diagnostics.encoder = groups.first().copied();
        }
        let Some((adapter, backend)) = self.center_ui.diagnostics.encoder else {
            diagnostics_empty(ui, text.no_encoders);
            return;
        };
        let backend_label = |key: (u64, Backend)| {
            if key.1 == Backend::Software {
                (text.software_backend)(key.1.name())
            } else {
                let device = caps
                    .adapters
                    .iter()
                    .find(|a| a.luid == key.0)
                    .map_or(text.graphics_adapter, |a| a.name.as_str());
                format!("{} · {}", key.1.name(), device)
            }
        };
        ui.horizontal(|ui| {
            crate::ui::controls::diagnostics_label(ui, text.encoder);
            egui::ComboBox::from_id_salt("diagnostics-encoder")
                .width(ui.available_width())
                .selected_text(backend_label((adapter, backend)))
                .show_ui(ui, |ui| {
                    for key in groups {
                        let label = backend_label(key);
                        ui.selectable_value(
                            &mut self.center_ui.diagnostics.encoder,
                            Some(key),
                            label,
                        );
                    }
                });
        });
        let device = caps
            .adapters
            .iter()
            .find(|a| a.luid == adapter)
            .map_or(text.software_encoding, |a| a.name.as_str());
        let processing_device = diagnostics_row(
            ui,
            text.processing_device,
            if backend == Backend::Software {
                "CPU"
            } else {
                device
            },
        );
        if backend == Backend::Software {
            diagnostics_row(ui, text.input_adapter, device)
                .on_hover_text((text.adapter_id)(adapter));
        } else {
            processing_device.on_hover_text((text.adapter_id)(adapter));
        }
        diagnostics_row(
            ui,
            text.probe_source,
            &format!(
                "{} · {} × {}",
                caps.screen.label(),
                caps.screen.width,
                caps.screen.height
            ),
        );
        ui.add_space(theme::DIAGNOSTICS_GAP);
        let rows: Vec<_> = caps
            .codecs
            .iter()
            .filter(|c| c.adapter == adapter && c.backend == backend)
            .map(|c| {
                vec![
                    codec_label(c.format.codec).into(),
                    chroma_label(c.format.chroma).into(),
                    format!("{} bit", c.format.depth),
                    format!("{} × {}", c.maximum.0, c.maximum.1),
                ]
            })
            .collect();
        diagnostics_table(
            ui,
            &[
                text.column_codec,
                text.column_chroma,
                text.column_depth,
                text.column_max_size,
            ],
            &rows,
            |_, _| TEXT,
        );
    }

    fn diagnostic_decoding(&mut self, ui: &mut egui::Ui) {
        let text = &t().settings;
        ui.horizontal(|ui| {
            let busy = self.diagnostics.busy();
            if crate::ui::controls::diagnostics_action(
                ui,
                busy || !self.has_viewers(),
                if busy { text.stop_check } else { text.full_check },
            )
            .clicked()
            {
                if busy {
                    self.diagnostics.cancel_probe();
                } else {
                    self.diagnostics.probe(self.local_display);
                }
            }
        });
        ui.add_space(theme::DIAGNOSTICS_GAP);
        let Some(report) = self.diagnostics.probe.as_ref() else {
            return;
        };
        let total: usize = report
            .backends
            .iter()
            .flat_map(|b| &b.rows)
            .map(|r| r.cells.len())
            .sum();
        let done = report
            .backends
            .iter()
            .flat_map(|b| &b.rows)
            .flat_map(|r| &r.cells)
            .filter(|c| c.status != crate::media::decoder::diagnostics::Status::Pending)
            .count();
        diagnostics_row(
            ui,
            text.check_status,
            &if self.diagnostics.busy() {
                (text.check_progress)(done, total)
            } else {
                report.message.clone()
            },
        );
        if report.backends.is_empty() {
            return;
        }
        self.center_ui.diagnostics.decoder = self
            .center_ui
            .diagnostics
            .decoder
            .min(report.backends.len() - 1);
        ui.horizontal(|ui| {
            crate::ui::controls::diagnostics_label(ui, text.decoder);
            egui::ComboBox::from_id_salt("diagnostics-decoder")
                .width(ui.available_width())
                .selected_text(&report.backends[self.center_ui.diagnostics.decoder].name)
                .show_ui(ui, |ui| {
                    for (index, backend) in report.backends.iter().enumerate() {
                        ui.selectable_value(
                            &mut self.center_ui.diagnostics.decoder,
                            index,
                            &backend.name,
                        );
                    }
                });
        });
        let backend = &report.backends[self.center_ui.diagnostics.decoder];
        diagnostics_row(ui, text.processing_device, backend.device.as_deref().unwrap_or("CPU"));
        ui.add_space(theme::DIAGNOSTICS_GAP);
        let mut headers = vec![text.column_format.to_owned()];
        headers.extend(backend.sizes.iter().map(|&(w, h)| match (w, h) {
            (1280, 720) => "720p".to_owned(),
            (1920, 1080) => "1080p".to_owned(),
            (2560, 1440) => "1440p".to_owned(),
            (3840, 2160) => "4K".to_owned(),
            _ => format!("{w}×{h}"),
        }));
        let rows: Vec<_> = backend
            .rows
            .iter()
            .map(|r| {
                std::iter::once(r.format.clone())
                    .chain(
                        r.cells
                            .iter()
                            .map(|c| format!("{}\n{}", c.status.label(), c.detail)),
                    )
                    .collect()
            })
            .collect();
        diagnostics_table(
            ui,
            &headers.iter().map(String::as_str).collect::<Vec<_>>(),
            &rows,
            |row, column| {
                use crate::media::decoder::diagnostics::Status;
                if column == 0 {
                    return TEXT;
                }
                match backend.rows[row].cells[column - 1].status {
                    Status::Passed => theme::GREEN,
                    Status::Failed => theme::RED,
                    Status::Busy => theme::AMBER,
                    Status::Pending => theme::ACCENT,
                    Status::Unsupported | Status::Unchecked => theme::MUTED,
                }
            },
        );
    }

    fn diagnostic_sessions(&self, ui: &mut egui::Ui) {
        let text = &t().settings;
        let host = self.host.as_ref().map(|host| host.status());
        let viewing: Vec<_> = self
            .viewers
            .active
            .iter()
            .filter_map(|s| s.handle.info().map(|info| (s.alias.clone(), info)))
            .collect();
        if !host.as_ref().is_some_and(|host| host.session_active) && viewing.is_empty() {
            diagnostics_empty(ui, text.no_sessions);
            return;
        }
        if let Some(host) = host.filter(|host| host.session_active) {
            ui.strong(text.hosting);
            diagnostics_row(ui, text.connection_state, &host.message);
            diagnostics_row(
                ui,
                text.input_backend,
                host.input_backend.as_deref().unwrap_or(text.waiting_for_control),
            );
            diagnostics_row(
                ui,
                text.clipboard_sync,
                if host.clipboard.active {
                    if host.clipboard.files {
                        text.clipboard_all
                    } else {
                        text.clipboard_text_images
                    }
                } else {
                    text.clipboard_off
                },
            );
            diagnostics_row(
                ui,
                text.port_forwarding,
                &(text.ports_summary)(
                    host.ports.connections as u64,
                    host.ports.sent,
                    host.ports.received,
                ),
            );
            if let Some(error) = &host.ports.error {
                diagnostics_row(ui, text.forwarding_status, error);
            }
            diagnostics_row(
                ui,
                text.standalone_file_transfer,
                if host.files.active {
                    text.files_as_user
                } else {
                    text.files_off
                },
            );
            if let Some(error) = &host.files.error {
                diagnostics_row(ui, text.transfer_status, error);
            }
            if let Some(error) = &host.clipboard.error {
                ui.colored_label(theme::RED, error);
            }
            if let Some(error) = &host.input_error {
                diagnostics_row(ui, text.input_status, error);
            }
            diagnostics_row(
                ui,
                text.desktop_audio,
                if host.audio.capturing {
                    text.capturing
                } else if host.audio.configured {
                    text.waiting_for_playback
                } else {
                    text.audio_not_negotiated
                },
            );
            if !host.audio.device.is_empty() {
                diagnostics_row(ui, text.audio_device, &host.audio.device);
            }
            if host.audio.target_bitrate != 0 {
                diagnostics_row(
                    ui,
                    text.audio_bitrate,
                    &format!("{} kbps", host.audio.target_bitrate / 1000),
                );
            }
            if let Some(error) = &host.audio.error {
                diagnostics_row(ui, text.audio_status, error);
            }
            diagnostics_row(
                ui,
                text.virtual_microphone,
                if host.microphone.active {
                    text.microphone_in_use
                } else if host.microphone.enabled {
                    text.microphone_waiting
                } else {
                    text.wol_disabled
                },
            );
            if let Some(error) = &host.microphone.error {
                diagnostics_row(ui, text.microphone_status, error);
            }
            let adapters = self
                .host
                .as_ref()
                .and_then(|host| host.capabilities())
                .map(|caps| caps.adapters.clone())
                .unwrap_or_default();
            for (index, stream) in host.streams {
                let Some(active) = stream.video else {
                    continue;
                };
                ui.add_space(theme::DIAGNOSTICS_GAP);
                if let Some(screen) = stream.screen {
                    ui.strong((text.video_track)(&screen.label(), index + 1));
                }
                diagnostics_row(ui, text.encoder, active.backend.name())
                    .on_hover_text((text.adapter_id)(active.adapter));
                diagnostics_row(
                    ui,
                    text.encoding_gpu,
                    adapters
                        .iter()
                        .find(|a| a.luid == active.adapter)
                        .map_or(t().common.unknown, |a| a.name.as_str()),
                );
                diagnostics_row(
                    ui,
                    text.preferred_gpu,
                    active.preferred_gpu.map_or(text.automatic, |id| {
                        adapters
                            .iter()
                            .find(|a| a.id == Some(id))
                            .map_or(text.selected_gpu_unavailable, |a| a.name.as_str())
                    }),
                );
                diagnostics_row(
                    ui,
                    text.preferred_codec,
                    match active.preferred_codec {
                        Some(1) => "H.264",
                        Some(2) => "HEVC",
                        Some(5) => "AV1",
                        _ => text.automatic,
                    },
                );
                diagnostics_row(ui, text.selection_reason, active.selection.label());
                diagnostics_row(
                    ui,
                    text.capture_method,
                    stream.capture.as_deref().unwrap_or(text.waiting_for_capture),
                );
                diagnostics_row(
                    ui,
                    text.output_format,
                    &format!(
                        "{} · {} · {} bit",
                        codec_label(active.format.codec),
                        chroma_label(active.format.chroma),
                        active.format.depth
                    ),
                );
                diagnostics_row(
                    ui,
                    text.output,
                    &(text.output_value)(active.size.0, active.size.1, active.fps),
                );
                diagnostics_row(
                    ui,
                    text.bitrate,
                    &format!("{:.2} Mbps", active.target_bps as f64 / 1_000_000.0),
                );
            }
        }
        for (alias, info) in viewing {
            ui.add_space(theme::DIAGNOSTICS_GAP);
            ui.strong((text.viewing)(&alias));
            for (label, value) in [
                (text.local_decoder, info.decoder),
                (text.received_stream, info.video_format),
                (text.route, info.connection),
                (text.remote_encoder, info.remote_encoder),
                (text.remote_capture, info.remote_capture),
            ] {
                diagnostics_row(
                    ui,
                    label,
                    if value.is_empty() {
                        text.waiting_for_session
                    } else {
                        &value
                    },
                );
            }
        }
    }
}

fn codec_label(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "H.264",
        Codec::H265 => "H.265",
        Codec::Av1 => "AV1",
    }
}
fn chroma_label(chroma: u8) -> &'static str {
    if chroma == 3 { "4:4:4" } else { "4:2:0" }
}
