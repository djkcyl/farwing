use super::*;
use crate::features::host::wol::setup::{Action, AdapterKey};
use crate::i18n::t;

/// The setup helper reports fixed English error codes; show them translated.
fn issue_label(error: &str) -> String {
    let text = &t().settings;
    match error {
        "Power-management properties could not be read" | "Power-management readback failed" => {
            text.wol_issue_power_management.into()
        }
        "Advanced properties could not be read" | "Advanced-property readback failed" => {
            text.wol_issue_advanced.into()
        }
        "Could not enable device wake" => text.wol_issue_device_wake.into(),
        "Could not restrict wake to magic packets" => text.wol_issue_magic_packet.into(),
        _ => {
            for prefix in [
                "Could not enable ",
                "No supported enable value for ",
                "Readback did not confirm ",
            ] {
                if let Some(key) = error.strip_prefix(prefix) {
                    let p = crate::platform::windows::wol::setup::Property {
                        key: key.into(),
                        ..Default::default()
                    };
                    return (text.wol_issue_unconfirmed)(p.title());
                }
            }
            error.into()
        }
    }
}

#[derive(Default)]
pub(super) struct ViewState {
    open: bool,
    generation: u64,
    selected: Option<AdapterKey>,
    confirmed: bool,
    configure_confirmation: bool,
    pending: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    error: Option<String>,
}
impl DeviceCenterApp {
    fn wol_setup_action(&mut self, action: Action, ctx: &egui::Context) {
        let Some(host) = self.host.clone() else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.center_ui.wol_setup.pending = Some(rx);
        self.center_ui.wol_setup.error = None;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = host.wol_setup_action(action);
            let _ = tx.send(result.map_err(|e| format!("{e:#}")));
            ctx.request_repaint();
        });
    }
    pub(super) fn wol_setup_entry(&mut self, ui: &mut egui::Ui) {
        let Some(host) = self.host.as_ref().filter(|h| !h.is_guest()) else {
            return;
        };
        let status = host.status().wol_setup;
        let words = &t().settings;
        let text = match status.enabled {
            Some(true) => words.wol_enabled,
            Some(false) => words.wol_disabled,
            None => words.wol_not_checked,
        };
        let mut open = false;
        form_row(ui, words.wol_remote_wake, "", |ui| {
            ui.horizontal(|ui| {
                ui.label(text);
                open = ui.button(words.wol_set_up).clicked();
            });
        });
        if open {
            self.center_ui.wol_setup = ViewState {
                open: true,
                generation: self.login_generation,
                ..Default::default()
            };
            self.wol_setup_action(Action::Check, ui.ctx());
        }
    }
    pub(super) fn wol_setup_dialog(&mut self, ctx: &egui::Context) {
        if !self.center_ui.wol_setup.open {
            return;
        }
        if self.center_ui.wol_setup.generation != self.login_generation
            || self.host.as_ref().is_none_or(|h| h.is_guest())
        {
            self.center_ui.wol_setup = Default::default();
            return;
        }
        let host = self.host.as_ref().unwrap();
        let status = host.status().wol_setup;
        let allowed = host.allowed();
        let state = &mut self.center_ui.wol_setup;
        if let Some(rx) = &state.pending {
            match rx.try_recv() {
                Ok(result) => {
                    state.error = result.err();
                    state.pending = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    state.error = Some(t().app.operation_interrupted.into());
                    state.pending = None;
                }
                Err(_) => {}
            }
        }
        if state
            .selected
            .as_ref()
            .is_none_or(|key| !status.adapters.iter().any(|a| a.key == *key))
        {
            state.selected = status
                .adapters
                .iter()
                .find(|a| Some(a.index) == status.default_index)
                .or(status.adapters.first())
                .map(|a| a.key.clone());
            state.confirmed = false;
            state.configure_confirmation = false;
        }
        let busy = status.busy || state.pending.is_some();
        let text = &t().settings;
        let mut action = None;
        let mut close = false;
        let modal=egui::Modal::new(egui::Id::new("wol-setup")).frame(crate::ui::controls::dialog_frame()).show(ctx,|ui|{
            ui.set_width(540.);
            ui.label(RichText::new(text.wol_title).size(20.).strong());
            ui.add_space(8.);
            ui.horizontal(|ui|{
                ui.label(match status.enabled{Some(true)=>text.wol_state_on,Some(false)=>text.wol_state_off,None=>text.wol_state_unknown});
                if ui.add_enabled(!busy,egui::Button::new(text.wol_check_again)).clicked(){action=Some(Action::Check);state.confirmed=false;}
            });
            egui::ScrollArea::vertical().max_height(460.).show(ui,|ui|{
                ui.add_space(12.);ui.label(RichText::new(text.wol_step_adapter).strong());
                if status.checked && status.adapters.is_empty(){ui.colored_label(AMBER,text.wol_no_wired_adapter);}
                let before=state.selected.clone();
                ui.add_enabled_ui(!busy,|ui|{
                    egui::ComboBox::from_id_salt("wol-adapter").width(510.).selected_text(status.adapters.iter().find(|a|Some(&a.key)==state.selected.as_ref()).map(|a|format!("{} · {}",a.name,a.description)).unwrap_or_else(||text.wol_select_adapter.into())).show_ui(ui,|ui|{
                        for adapter in &status.adapters {ui.selectable_value(&mut state.selected,Some(adapter.key.clone()),format!("{} · {}",adapter.name,adapter.description));}
                    });
                });
                if before!=state.selected{state.confirmed=false;state.configure_confirmation=false;}
                if let Some(adapter)=status.adapters.iter().find(|a|Some(&a.key)==state.selected.as_ref()) {
                    ui.label(if adapter.connected{text.wol_cable_connected}else{text.wol_cable_unplugged});
                    egui::Grid::new("wol-properties").num_columns(2).min_row_height(20.).spacing(vec2(20.,5.)).show(ui,|ui|{
                        for p in &adapter.items {
                            ui.label(p.title());ui.colored_label(match p.enabled(){Some(true)=>GREEN,Some(false)=>AMBER,None=>MUTED},match p.enabled(){Some(true)=>text.wol_enabled,Some(false)=>text.wol_disabled,None=>text.wol_unreadable});ui.end_row();
                        }
                    });
                    for error in &adapter.errors{ui.colored_label(AMBER,issue_label(error));}
                    if ui.add_enabled(!busy,egui::Button::new(text.wol_configure_adapter)).clicked(){state.configure_confirmation=true;}
                    if state.configure_confirmation {
                        ui.label(text.wol_configure_notice);
                        ui.horizontal(|ui|{
                            if ui.add_enabled(!busy,egui::Button::new(text.wol_configure)).clicked(){action=Some(Action::Configure(adapter.key.clone()));state.configure_confirmation=false;state.confirmed=false;}
                            if ui.button(t().common.cancel).clicked(){state.configure_confirmation=false;}
                        });
                    }
                }
                ui.add_space(12.);ui.label(RichText::new(text.wol_step_requirements).strong());
                ui.label(text.wol_requirement_bios);
                ui.label(text.wol_requirement_power);
                ui.label(text.wol_requirement_lan);
                if status.unattended {ui.label(text.wol_service_installed);}
                else {ui.colored_label(AMBER,text.wol_service_missing);}
                ui.add_enabled_ui(!busy,|ui|{ui.checkbox(&mut state.confirmed,text.wol_requirements_met);});
                if !allowed{ui.colored_label(AMBER,text.wol_connections_off);}
            });
            ui.add_space(10.);
            if busy{ui.horizontal(|ui|{ui.spinner();ui.label(t().app.processing);});ctx.request_repaint_after(std::time::Duration::from_millis(100));}
            else if !status.message.is_empty(){ui.label(&status.message);}
            if let Some(e)=state.error.as_ref().or(status.error.as_ref()){ui.colored_label(AMBER,e);}
            ui.add_space(8.);
            ui.horizontal(|ui|{
                if ui.add_enabled(!busy && state.confirmed && allowed && state.selected.is_some(),crate::ui::controls::primary(text.wol_finish)).clicked(){action=Some(Action::Enable{adapter:state.selected.clone().unwrap(),confirmed:true});state.confirmed=false;}
                if ui.add_enabled(!busy && status.enabled==Some(true),egui::Button::new(text.wol_turn_off)).clicked(){action=Some(Action::Disable);}
                if ui.button(t().common.close).clicked(){close=true;}
            });
        });
        if close || modal.should_close() {
            self.center_ui.wol_setup.open = false;
        }
        if let Some(action) = action {
            self.wol_setup_action(action, ctx);
        }
    }
}
