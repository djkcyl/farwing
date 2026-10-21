use super::*;
use crate::diagnostics::bundle::{Exported, Input, Task};
use crate::i18n::t;
#[derive(Default)]
pub(super) struct ExportUi {
    task: Option<Task>,
    result: Option<Exported>,
    error: Option<String>,
}
impl DeviceCenterApp {
    /// Export (and cancel), right to left on a title row.
    pub(super) fn diagnostic_export_button(&mut self, ui: &mut egui::Ui) {
        let text = &t().settings;
        let state = &self.center_ui.logs.export;
        let running = state.task.is_some();
        let start = ui
            .add_enabled(
                !running,
                egui::Button::new(if running {
                    text.exporting
                } else {
                    text.export_diagnostics
                }),
            )
            .clicked();
        if let Some(task) = &state.task
            && ui.button(t().common.cancel).clicked()
        {
            task.cancel();
        }
        if start {
            let input = self.diagnostic_bundle_input();
            let state = &mut self.center_ui.logs.export;
            state.error = None;
            state.result = None;
            match Task::start(input, ui.ctx().clone()) {
                Ok(task) => state.task = Some(task),
                Err(e) => state.error = Some((text.export_failed)(&format!("{e:#}"))),
            }
        }
    }

    /// How the export goes, or how it went: nothing while there is none.
    pub(super) fn diagnostic_export_status(&mut self, ui: &mut egui::Ui) {
        let text = &t().settings;
        let state = &mut self.center_ui.logs.export;
        if let Some(task) = &state.task {
            match task.receiver.try_recv() {
                Ok(Ok(result)) => {
                    state.result = Some(result);
                    state.task = None;
                }
                Ok(Err(error)) => {
                    state.error = Some(error);
                    state.task = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    state.error = Some(text.export_interrupted.into());
                    state.task = None;
                }
                Err(_) => {
                    ui.ctx().request_repaint_after(Duration::from_millis(200));
                }
            }
        }
        if let Some(result) = &state.result {
            let mut open = false;
            ui.horizontal(|ui| {
                ui.label((text.exported)(result.logs, result.warnings > 0));
                open = ui.link(t().app.open_folder).clicked();
            });
            if open && let Err(e) = crate::diagnostics::bundle::open_folder(&result.path) {
                state.error = Some(format!("{e:#}"));
            }
        }
        if let Some(task) = &state.task {
            let progress = task.progress();
            ui.add(
                egui::ProgressBar::new(progress.fraction())
                    .animate(progress.total == 0)
                    .text(if progress.total == 0 {
                        progress.stage
                    } else {
                        format!("{} · {:.0}%", progress.stage, progress.fraction() * 100.0)
                    }),
            );
        }
        if let Some(error) = &state.error {
            ui.colored_label(theme::RED, error);
        }
    }
    fn diagnostic_bundle_input(&self) -> Input {
        use serde_json::json;
        let mut private_values = vec![self.account_name.clone()];
        if let Some(devices) = &self.devices {
            for device in std::iter::once(&devices.current_device).chain(&devices.devices)
            {
                private_values.extend([device.device_id.clone(), display_alias(device).to_owned()]);
            }
        }
        for (row, value) in &self.diagnostics.rows {
            if *row == super::super::diagnostics::Row::DeviceName {
                private_values.push(value.clone());
            }
        }
        let viewers:Vec<_>=self.viewers.active.iter().filter_map(|entry|entry.handle.info()).map(|v|{
            if let Some(target)=&v.target{private_values.extend([target.device_id.clone(),target.alias.clone()]);}
            json!({"connection":v.connection,"decoder":v.decoder,"video_format":v.video_format,"remote_encoder":v.remote_encoder,"remote_capture":v.remote_capture,"playing":v.playing})
        }).collect();
        let host = self.host.as_ref().map(|h| h.status());
        let decoding=self.diagnostics.probe.as_ref().map(|r|json!({"message":r.message,"backends":r.backends.iter().map(|b|json!({"backend":b.name,"device":b.device,"sizes":b.sizes,"formats":b.rows.iter().map(|r|json!({"format":r.format,"results":r.cells.iter().map(|c|json!({"status":c.status.label(),"detail":c.detail})).collect::<Vec<_>>()})).collect::<Vec<_>>()})).collect::<Vec<_>>()}));
        Input {
            private_values,
            summary: json!({"graphics":self.diagnostics.graphics,"local_display":{"width":self.local_display.width,"height":self.local_display.height,"refresh_hz":self.local_display.refresh_hz},"viewers":viewers,"host":host,"decoder_checks":decoding}),
        }
    }
}
