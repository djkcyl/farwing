//! Login startup switch. The system's own startup entry is the setting, so
//! the switch shows what the system will do, read back after every change.
use super::*;
use crate::platform::host_service;
use host_service::startup::State;
use std::time::{Duration, Instant};

/// Reading the entry and the install mode touches the registry/filesystem, and
/// on Linux spawns `systemctl`, so the page rereads them at most this often.
const REFRESH: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(super) struct ViewState {
    checked: Option<Instant>,
    startup: Option<Result<State, String>>,
    /// Why login startup cannot work here at all, if it cannot. Windows never
    /// says so; Linux only when it has neither a systemd user unit nor an XDG
    /// autostart entry to write.
    unavailable: Option<String>,
    /// The resident service keeps the device online before login on its own.
    managed: bool,
}
impl ViewState {
    fn refresh(&mut self) {
        self.startup = Some(host_service::startup::state().map_err(|error| format!("{error:#}")));
        self.unavailable = host_service::startup::unavailable();
        self.managed = host_service::resident::managed();
        self.checked = Some(Instant::now());
    }
}

impl DeviceCenterApp {
    pub(super) fn startup_settings(&mut self, ui: &mut egui::Ui) {
        let state = &mut self.center_ui.startup;
        if state.checked.is_none_or(|at| at.elapsed() >= REFRESH) {
            state.refresh();
        }
        ui.ctx().request_repaint_after(REFRESH);
        let readable = matches!(state.startup, Some(Ok(_)));
        let system_disabled = matches!(state.startup, Some(Ok(State::DisabledByWindows)));
        let mut enabled = matches!(state.startup, Some(Ok(State::Enabled)));
        let unavailable = state.unavailable.clone();
        // 自启用不了时打开只会失败；但已经开着的还要能关掉。
        let toggleable = unavailable.is_none() || enabled;
        let managed = state.managed;
        // Installing or removing components rewrites this entry.
        let busy = self.maintenance_busy();
        section(ui, "启动");
        let mut changed = false;
        form_row(
            ui,
            "开机自启",
            if managed {
                "登录后在托盘启动；不影响常驻服务的被控设置"
            } else {
                "登录系统后在托盘启动"
            },
            |ui| {
                changed = ui
                    .add_enabled_ui(readable && !busy && !system_disabled && toggleable, |ui| {
                        crate::ui::controls::switch(ui, &mut enabled)
                    })
                    .inner
                    .changed();
            },
        );
        if changed {
            if let Err(error) = host_service::startup::configure(enabled) {
                self.status = StatusMessage::error(format!("开机自启设置失败：{error:#}"));
            }
            self.center_ui.startup.refresh();
        }
        let wol_enabled = self
            .host
            .as_ref()
            .filter(|host| !host.is_guest())
            .is_some_and(|host| host.status().wol_setup.enabled == Some(true));
        match &self.center_ui.startup.startup {
            Some(Err(error)) => {
                ui.colored_label(AMBER, format!("无法读取开机自启设置：{error}"));
            }
            Some(Ok(State::DisabledByWindows)) => {
                ui.colored_label(
                    AMBER,
                    "Windows 已禁用本程序自启动，请在“应用 → 启动”中启用 OpenUUYC。",
                );
                if ui
                    .add_enabled(!busy, egui::Button::new("打开 Windows 启动设置"))
                    .clicked()
                {
                    ui.ctx()
                        .open_url(egui::OpenUrl::new_tab("ms-settings:startupapps"));
                }
            }
            Some(Ok(State::Disabled)) if wol_enabled && !managed => {
                ui.colored_label(
                    AMBER,
                    "已开启远程开机：关闭自启后，被唤醒的电脑需登录并打开 OpenUUYC 才会上线。",
                );
            }
            _ => {}
        }
        if readable && let Some(reason) = &unavailable {
            ui.colored_label(AMBER, format!("开机自启不可用：{reason}"));
        }
    }
}
