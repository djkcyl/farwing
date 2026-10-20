//! One product installation entry; service and driver backends remain separate.
use super::*;
use crate::platform::windows::components::{self as install, Kind, Operation, Status};
struct ResultState {
    status: Result<Status, String>,
    components: Vec<(Kind, Result<Status, String>)>,
    operation: Option<(Kind, Operation, Result<bool, String>)>,
}
#[derive(Default)]
pub(in crate::application::app) struct Manager {
    status: Option<Status>,
    components: Vec<(Kind, Result<Status, String>)>,
    pending: Option<std::sync::mpsc::Receiver<ResultState>>,
    confirm: Option<Operation>,
    audio_confirm: Option<Operation>,
    error: Option<String>,
    reboot: bool,
    allow_sas: bool,
    removal: install::RemovalOptions,
    offered: bool,
    completed: bool,
    handoff: bool,
}
impl Manager {
    fn work(&mut self, operation: Option<Operation>, ctx: egui::Context) {
        self.work_kind(Kind::Suite, operation, ctx);
    }
    fn work_kind(&mut self, kind: Kind, operation: Option<Operation>, ctx: egui::Context) {
        if self.pending.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending = Some(rx);
        self.error = None;
        let allow_sas = self.allow_sas;
        let removal = self.removal;
        std::thread::spawn(move || {
            let operation = operation.map(|op| {
                (
                    kind,
                    op,
                    install::request(
                        kind,
                        op,
                        kind == Kind::Suite && op == Operation::Install && allow_sas,
                        if kind == Kind::Suite && op == Operation::Uninstall {
                            removal
                        } else {
                            Default::default()
                        },
                    )
                    .map_err(|e| format!("{e:#}")),
                )
            });
            let status = install::status(Kind::Suite).map_err(|e| format!("{e:#}"));
            let components = Kind::ALL
                .into_iter()
                .map(|kind| (kind, install::status(kind).map_err(|e| format!("{e:#}"))))
                .collect();
            let _ = tx.send(ResultState {
                status,
                components,
                operation,
            });
            ctx.request_repaint();
        });
    }
    fn poll(&mut self) {
        let Some(rx) = &self.pending else { return };
        match rx.try_recv() {
            Ok(result) => {
                self.pending = None;
                self.components = result.components;
                match result.status {
                    Ok(status) => self.status = Some(status),
                    Err(error) => self.error = Some(error),
                }
                if let Some((kind, operation, result)) = result.operation {
                    match result {
                        Ok(reboot) => {
                            self.reboot |= reboot;
                            self.completed = true;
                            self.handoff =
                                kind == Kind::Suite && operation == Operation::Install && !reboot;
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => (),
            Err(_) => {
                self.pending = None;
                self.error = Some("安装任务已中断，请重新检查".into());
            }
        }
    }
    pub(in crate::application::app) fn busy(&self) -> bool {
        self.pending.is_some()
    }
    pub(super) fn button(&mut self, ui: &mut egui::Ui, active: bool) {
        // Machine-wide components (service, drivers, installed program)
        // belong to the default profile; other profiles run portable.
        if !crate::product::is_default_profile() {
            return;
        }
        self.poll();
        if self.status.is_none() && self.error.is_none() && self.pending.is_none() {
            self.work(None, ui.ctx().clone());
        }
        if !self.reboot && self.status.as_ref().is_none_or(|s| s.installed && s.ready) {
            return;
        }
        let installed = self.status.as_ref().is_some_and(|s| s.installed);
        let label = if self.pending.is_some() {
            "正在处理…"
        } else if self.reboot {
            "需要重启 Windows"
        } else if installed {
            "更新服务"
        } else {
            "安装服务"
        };
        let actionable = self
            .status
            .as_ref()
            .is_some_and(|s| !s.installed || !s.ready);
        let response = ui.add_enabled(
            actionable && !active && !self.reboot && self.pending.is_none(),
            crate::ui::controls::centered_primary(label).min_size(vec2(ui.available_width(), 30.0)),
        );
        if response.clicked() {
            self.confirm = Some(Operation::Install);
        }
        response.on_hover_text(if active {
            "请先结束本机被控会话"
        } else {
            self.status
                .as_ref()
                .map_or("正在检查服务", |s| s.label.as_str())
        });
    }
    /// What the background service means for reaching this device, and the
    /// action it calls for (default profile only).
    pub(super) fn reach(&self) -> Option<(&'static str, Option<&'static str>)> {
        if !crate::product::is_default_profile() {
            return None;
        }
        let status = self.status.as_ref()?;
        Some(if status.ready {
            ("已安装：锁屏、注销或重启后（无需登录 Windows）也能连接本机", None)
        } else if status.installed {
            ("需要完成安装或更新，完成前锁屏或重启后可能无法连接本机", Some("更新服务"))
        } else {
            ("未安装：只有本程序在运行且已登录 Windows 时，才能连接本机", Some("安装服务"))
        })
    }
    pub(super) fn request_install(&mut self) {
        if self.pending.is_none() && !self.reboot {
            self.confirm = Some(Operation::Install);
        }
    }
    pub(super) fn management(&mut self, ui: &mut egui::Ui, active: bool) {
        if !crate::product::is_default_profile() {
            section(ui, "服务管理");
            ui.label(format!(
                "当前 profile「{}」以便携模式运行。后台服务、驱动和安装只属于默认 profile。",
                crate::product::profile()
            ));
            return;
        }
        self.poll();
        section(ui, "服务管理");
        let label = self
            .status
            .as_ref()
            .map_or("正在检查…", |s| s.label.as_str());
        let removable = self.status.as_ref().is_some_and(|s| s.removable);
        let installable = self.status.as_ref().is_some_and(|s| !s.installed);
        let hint = format!("{label} · 锁屏、注销或重启后（无需登录 Windows）也能连接本机，不需要账号");
        form_row(ui, "常驻服务", &hint, |ui| {
            if installable
                && ui
                    .add_enabled(
                        !active && !self.reboot && self.pending.is_none(),
                        crate::ui::controls::primary("安装服务"),
                    )
                    .on_disabled_hover_text(if active {
                        "请先结束本机被控会话"
                    } else {
                        "正在处理"
                    })
                    .clicked()
            {
                self.confirm = Some(Operation::Install);
            }
            if !removable {
                return;
            }
            let idle = !active && !self.reboot && self.pending.is_none();
            let busy = if active {
                "请先结束本机被控会话"
            } else {
                "正在处理"
            };
            if ui
                .add_enabled(idle, egui::Button::new("修复安装"))
                .on_disabled_hover_text(busy)
                .clicked()
            {
                self.confirm = Some(Operation::Install);
            }
            if ui
                .add_enabled(idle, egui::Button::new("卸载服务"))
                .on_disabled_hover_text(busy)
                .clicked()
            {
                self.removal = Default::default();
                self.confirm = Some(Operation::Uninstall);
            }
        });
        self.audio_management(ui, active);
    }
    fn audio_management(&mut self, ui: &mut egui::Ui, active: bool) {
        let audio_result = self
            .components
            .iter()
            .find(|(kind, _)| *kind == Kind::AudioDriver)
            .map(|(_, status)| status);
        let audio = audio_result.and_then(|status| status.as_ref().ok());
        let installed = audio.is_some_and(|s| s.installed);
        let removable = audio.is_some_and(|s| s.removable);
        let label = match audio_result {
            Some(Ok(status)) => status.label.clone(),
            Some(Err(error)) => format!("检查失败：{error}"),
            None => "正在检查…".into(),
        };
        form_row(ui, "虚拟声卡", &label, |ui| {
            if !audio.is_some_and(|status| status.ready)
                && ui
                    .add_enabled(
                        !active && self.pending.is_none(),
                        egui::Button::new(if installed {
                            "更新驱动"
                        } else {
                            "安装驱动"
                        }),
                    )
                    .clicked()
            {
                self.audio_confirm = Some(Operation::Install);
            }
            if ui
                .add_enabled(
                    !active && self.pending.is_none() && removable,
                    egui::Button::new("卸载驱动"),
                )
                .clicked()
            {
                self.audio_confirm = Some(Operation::Uninstall);
            }
        });
        if audio.is_some_and(|status| status.installed && !status.ready) {
            ui.label(
                "虚拟声卡使用测试签名驱动，需要 Windows 允许加载；重新安装不一定能解决加载失败。",
            );
        }
    }
    fn audio_dialog(&mut self, ctx: &egui::Context, active: bool) {
        if let Some(operation) = self.audio_confirm {
            let title = if operation == Operation::Install {
                if self.components.iter().any(|(kind, status)| {
                    *kind == Kind::AudioDriver && status.as_ref().is_ok_and(|s| s.installed)
                }) {
                    "更新虚拟声卡"
                } else {
                    "安装虚拟声卡"
                }
            } else {
                "卸载虚拟声卡"
            };
            let mut accepted = false;
            let mut dismissed = false;
            let result = egui::Modal::new(egui::Id::new("audio-driver-confirm"))
                .frame(crate::ui::controls::dialog_frame()).show(ctx, |ui| {
                    ui.set_width(460.0);
                    dismissed = crate::ui::controls::dialog_header(ui, title, crate::ui::controls::DialogIcon::Warning, true);
                    ui.label(if operation == Operation::Install {
                        "安装 Farwing 虚拟扬声器和虚拟麦克风。此构建使用测试签名驱动，需要在允许测试驱动的启动环境中安装；是否允许加载由 Windows 判断。"
                    } else { "移除 Farwing 虚拟扬声器和虚拟麦克风。请先关闭正在使用它们的应用。" });
                    let (yes, close) = crate::ui::controls::dialog_actions(ui,
                        Some(crate::ui::controls::DialogAction::new(title).enabled(!active && self.pending.is_none())), Some("取消"));
                    accepted = yes;
                    dismissed |= close;
                });
            if accepted {
                self.audio_confirm = None;
                self.work_kind(Kind::AudioDriver, Some(operation), ctx.clone());
            } else if dismissed || result.should_close() {
                self.audio_confirm = None;
            }
        }
    }
    fn dialogs(&mut self, ctx: &egui::Context, active: bool, home: bool) {
        self.audio_dialog(ctx, active);
        if home
            && crate::product::is_default_profile()
            && !self.offered
            && !active
            && self.pending.is_none()
            && self.confirm.is_none()
            && self.audio_confirm.is_none()
            && self.error.is_none()
            && self.status.is_some()
            && ctx.memory(|m| m.top_modal_layer().is_none())
        {
            self.offered = true;
            let remember = (|| -> anyhow::Result<bool> {
                let path = crate::product::local_data_dir()
                    .context("本地设置目录不可用")?
                    .join("resident-install-prompt.seen");
                let seen = path.exists();
                if !seen {
                    std::fs::create_dir_all(path.parent().unwrap())?;
                    std::fs::write(path, b"1")?;
                }
                Ok(seen)
            })();
            match remember {
                Ok(false) if self.status.as_ref().is_some_and(|s| !s.installed) => {
                    self.confirm = Some(Operation::Install)
                }
                Err(e) => self.error = Some(e.to_string()),
                _ => (),
            }
        }
        self.poll();
        crate::ui::controls::observe_notice(
            ctx,
            "component-error",
            "服务管理",
            crate::ui::controls::DialogIcon::Error,
            self.error.as_deref(),
        );
        if let Some(operation) = self.confirm {
            let mut accepted = false;
            let mut dismiss = false;
            let result = egui::Modal::new(egui::Id::new("component-confirm")).frame(crate::ui::controls::dialog_frame()).show(ctx, |ui| {
                ui.set_width(480.0);
                let verb = if operation == Operation::Uninstall { "卸载服务" } else if self.status.as_ref().is_some_and(|s| s.installed) { "更新服务" } else { "安装服务" };
                dismiss = crate::ui::controls::dialog_header(ui,verb,crate::ui::controls::DialogIcon::Warning,true);
                if operation == Operation::Install {
                    let drivers = crate::platform::windows::input::install::bundled() || crate::platform::windows::display::install::bundled();
                    if drivers { ui.label("安装被控服务、输入驱动及虚拟显示驱动，登录后自动启动托盘。允许被控时，重启后无需登录 Windows 即可连接。"); } else { ui.label("安装被控服务，登录后自动启动托盘。允许被控时，重启后无需登录 Windows 即可连接。此构建未包含驱动包，将使用系统输入注入和现有显示器。"); }
                    ui.label("程序安装到系统目录并创建桌面和开始菜单快捷方式，完成后重新打开控制中心。");
                    ui.label("本机身份、已登录账号的授权和被控设置将加密保存在本机，由当前 Windows 用户和系统服务使用；受信任设备与直连设置一并交给服务。不登录账号也可以使用：服务按访问码、受信任设备和局域网直连接受连接。");
                    ui.checkbox(&mut self.allow_sas,"允许服务发送 Ctrl+Alt+Del");
                    if drivers { ui.collapsing("驱动与证书", |ui| {
                        if let Some((subject, thumbprint, _)) = Kind::InputDriver.certificate() { ui.label(subject); ui.label(egui::RichText::new(thumbprint).monospace().small()); }
                        ui.label("两个驱动共用此证书，安装时加入本机受信任根证书及受信任发布者。");
                        ui.hyperlink_to("显示驱动上游与许可证", "https://github.com/SudoMaker/SudoVDA");
                    }); }
                } else {
                    ui.label("停止开机被控，卸载服务和输入驱动，移除自启动与后台账号授权。");
                    ui.checkbox(&mut self.removal.remove_display_driver, "同时卸载 Farwing 虚拟显示驱动" );
                    ui.checkbox(&mut self.removal.remove_audio_driver, "同时卸载 Farwing 虚拟声卡" );
                    ui.label("保留程序本体、当前账号和设置，仍可在普通桌面以便携模式使用。");
                }
                for (kind,status) in &self.components { ui.horizontal(|ui| { ui.label(kind.label()); ui.label(match status { Ok(s) => s.label.as_str(), Err(e) => e.as_str() }); }); }
                let (yes, close) = crate::ui::controls::dialog_actions(ui,Some(crate::ui::controls::DialogAction::new(verb).enabled(!active && self.pending.is_none())),Some("取消")); accepted = yes; dismiss |= close;
            });
            if accepted {
                self.confirm = None;
                self.work(Some(operation), ctx.clone());
            } else if dismiss || result.should_close() {
                self.confirm = None;
            }
        }
        if self.pending.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}
impl DeviceCenterApp {
    pub(in crate::application::app) fn component_dialogs(&mut self, ctx: &egui::Context) {
        self.center_ui.components.poll();
        if std::mem::take(&mut self.center_ui.components.handoff) {
            crate::application::schedule_installed_handoff();
            self.exit_requested = true;
            self.exit_ready = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if self.exit_requested {
            return;
        }
        let _ = std::mem::take(&mut self.center_ui.components.completed);
        if self.logout_pending {
            self.center_ui.components.confirm = None;
            self.center_ui.components.audio_confirm = None;
            return;
        }
        let active = self
            .host
            .as_ref()
            .is_some_and(|h| h.status().session_active);
        self.center_ui.components.dialogs(ctx, active, self.center_ui.page == Page::Devices);
    }
}
