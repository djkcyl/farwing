use super::*;
use crate::features::host::wol::setup::{Action, AdapterKey};

fn issue_label(error: &str) -> String {
    match error {
        "Power-management properties could not be read" | "Power-management readback failed" => {
            "未能读取网卡电源管理，请尝试配置或在设备管理器中检查".into()
        }
        "Advanced properties could not be read" | "Advanced-property readback failed" => {
            "未能读取网卡高级属性".into()
        }
        "Could not enable device wake" => "未能启用网卡唤醒权限".into(),
        "Could not restrict wake to magic packets" => "未能设置仅允许魔术包唤醒".into(),
        _ => {
            for prefix in [
                "Could not enable ",
                "No supported enable value for ",
                "Readback did not confirm ",
            ] {
                if let Some(key) = error.strip_prefix(prefix) {
                    let p = crate::platform::wol::setup::Property {
                        key: key.into(),
                        ..Default::default()
                    };
                    return format!("{}：未确认配置生效，请在设备管理器中检查", p.title());
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
        let text = match status.enabled {
            Some(true) => "已开启",
            Some(false) => "未开启",
            None => "尚未检查",
        };
        let mut open = false;
        form_row(ui, "远程开机", "设置本机的网络唤醒", |ui| {
            ui.horizontal(|ui| {
                ui.label(text);
                open = ui.button("设置…").clicked();
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
                    state.error = Some("设置请求中断，请重新检查".into());
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
        let mut action = None;
        let mut close = false;
        let modal=egui::Modal::new(egui::Id::new("wol-setup")).frame(crate::ui::controls::dialog_frame()).show(ctx,|ui|{
            ui.set_width(540.);
            ui.label(RichText::new("远程开机设置").size(20.).strong());
            ui.add_space(8.);
            ui.horizontal(|ui|{
                ui.label(match status.enabled{Some(true)=>"本设备：已允许远程开机",Some(false)=>"本设备：未允许远程开机",None=>"本设备：云端状态尚未确认"});
                if ui.add_enabled(!busy,egui::Button::new("重新检查")).clicked(){action=Some(Action::Check);state.confirmed=false;}
            });
            egui::ScrollArea::vertical().max_height(460.).show(ui,|ui|{
                ui.add_space(12.);ui.label(RichText::new("1 · 检查与配置有线网卡").strong());
                if status.checked && status.adapters.is_empty(){ui.colored_label(AMBER,"未找到物理有线网卡。请接入网线后重新检查。");}
                let before=state.selected.clone();
                ui.add_enabled_ui(!busy,|ui|{
                    egui::ComboBox::from_id_salt("wol-adapter").width(510.).selected_text(status.adapters.iter().find(|a|Some(&a.key)==state.selected.as_ref()).map(|a|format!("{} · {}",a.name,a.description)).unwrap_or_else(||"请选择网卡".into())).show_ui(ui,|ui|{
                        for adapter in &status.adapters {ui.selectable_value(&mut state.selected,Some(adapter.key.clone()),format!("{} · {}",adapter.name,adapter.description));}
                    });
                });
                if before!=state.selected{state.confirmed=false;state.configure_confirmation=false;}
                if let Some(adapter)=status.adapters.iter().find(|a|Some(&a.key)==state.selected.as_ref()) {
                    ui.label(if adapter.connected{"网线已连接"}else{"网线未连接"});
                    egui::Grid::new("wol-properties").num_columns(2).min_row_height(20.).spacing(vec2(20.,5.)).show(ui,|ui|{
                        for p in &adapter.items {
                            ui.label(p.title());ui.colored_label(match p.enabled(){Some(true)=>GREEN,Some(false)=>AMBER,None=>MUTED},match p.enabled(){Some(true)=>"已开启",Some(false)=>"未开启",None=>"不可读取或驱动不支持"});ui.end_row();
                        }
                    });
                    for error in &adapter.errors{ui.colored_label(AMBER,issue_label(error));}
                    if ui.add_enabled(!busy,egui::Button::new("配置所选网卡…")).clicked(){state.configure_confirmation=true;}
                    if state.configure_confirmation {
                        ui.label("将启用此网卡支持的魔术包、电源管理和关机唤醒设置；不会重启网卡。部分设置需下次重启才生效。");
                        ui.horizontal(|ui|{
                            if ui.add_enabled(!busy,egui::Button::new("确认配置")).clicked(){action=Some(Action::Configure(adapter.key.clone()));state.configure_confirmation=false;state.confirmed=false;}
                            if ui.button("取消").clicked(){state.configure_confirmation=false;}
                        });
                    }
                }
                ui.add_space(12.);ui.label(RichText::new("2 · 确认开机条件").strong());
                ui.label("在主板 BIOS 中开启 Wake on LAN / PCI-E 唤醒；若 ErP 会切断网卡待机供电，需按主板说明调整。程序无法代替 BIOS 检查。");
                ui.label("目标关机后仍需接通电源与网线，并有同网在线协助设备或已配置的 UU 路由器代发唤醒包。");
                #[cfg(windows)]
                if status.unattended {ui.label("常驻服务已就绪，Windows 启动后可自动上线。");}
                else {ui.colored_label(AMBER,"未安装常驻服务：开启时会设置登录自启动，登录 Windows 后才能自动上线。需要登录前被控时请安装常驻服务。");}
                #[cfg(not(windows))]
                ui.colored_label(AMBER,"开启时会启用登录自启动（接入 systemd 图形会话的桌面用 systemd 用户服务，其余桌面写入 XDG 自启动条目），登录桌面后才能自动上线。被控要有图形会话，Linux 无法在登录前被控。");
                ui.add_enabled_ui(!busy,|ui|{ui.checkbox(&mut state.confirmed,"我已了解上述条件，允许本设备被远程开机");});
                if !allowed{ui.colored_label(AMBER,"请先在连接设置中开启“允许被控”。");}
            });
            ui.add_space(10.);
            if busy{ui.horizontal(|ui|{ui.spinner();ui.label("正在处理，请稍候…");});ctx.request_repaint_after(std::time::Duration::from_millis(100));}
            else if !status.message.is_empty(){ui.label(&status.message);}
            if let Some(e)=state.error.as_ref().or(status.error.as_ref()){ui.colored_label(AMBER,e);}
            ui.add_space(8.);
            ui.horizontal(|ui|{
                if ui.add_enabled(!busy && state.confirmed && allowed && state.selected.is_some(),crate::ui::controls::primary("完成并开启")).clicked(){action=Some(Action::Enable{adapter:state.selected.clone().unwrap(),confirmed:true});state.confirmed=false;}
                if ui.add_enabled(!busy && status.enabled==Some(true),egui::Button::new("关闭远程开机")).clicked(){action=Some(Action::Disable);}
                if ui.button("关闭").clicked(){close=true;}
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
