//! Ordinary-user installation entry. Only component mutations request elevation.
use crate::platform::windows::{
    components::{self, Kind, Operation, application as deployment},
    host_service,
};
use anyhow::{Context, Result, ensure};
use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver},
    time::Duration,
};

pub(crate) fn route_gui() -> Result<bool> {
    cleanup_helpers();
    if !deployment::needs_handoff()? {
        return Ok(false);
    }
    let installed = deployment::image()?;
    if host_service::process::image_hash(&installed)?
        == host_service::process::image_hash(&std::env::current_exe()?)?
    {
        deployment::start_installed(std::env::args_os().skip(1))?;
    } else {
        run(false, std::env::args_os().skip(1).collect())?;
    }
    Ok(true)
}

#[derive(Debug)]
struct UpdateExit {
    code: i32,
    message: String,
}
impl std::fmt::Display for UpdateExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for UpdateExit {}
pub(crate) fn update_error_code(error: &anyhow::Error) -> i32 {
    error
        .downcast_ref::<UpdateExit>()
        .map(|e| e.code)
        .or_else(|| crate::application::component_error_code(error))
        .unwrap_or(1)
}

/// CLI update uses the same notification, quiescence and rollback transaction
/// as the GUI. No account restoration or UI startup happens on this entry path.
pub(crate) fn update(silent: bool, no_elevate: bool) -> Result<bool> {
    if !silent {
        // The update command is a maintenance operation, not the launch target.
        // Never pass it on to the replacement process after successful install.
        run(false, vec!["gui".into()])?;
        return Ok(false);
    }
    if no_elevate && !components::elevated()? {
        return Err(UpdateExit {
            code: 740,
            message:
                "静默更新需要管理员权限；请在管理员终端运行，或移除 --no-elevate 以请求 UAC 授权"
                    .into(),
        }
        .into());
    }
    let installed = deployment::image()?;
    if !installed.is_file() {
        return Err(UpdateExit {
            code: 1605,
            message: "尚未安装 OpenUUYC；update 仅更新已有安装".into(),
        }
        .into());
    }
    deployment::verify_directory(installed.parent().context("安装路径无效")?)?;
    ensure!(
        host_service::vault::owner()?.as_deref()
            == Some(host_service::vault::sid(std::process::id())?.as_str()),
        "请由安装此程序的 Windows 用户执行更新"
    );
    let _installer = super::instance::reserve_installer().map_err(|error| UpdateExit {
        code: 170,
        message: error.to_string(),
    })?;
    if host_service::process::image_hash(&installed)?
        == host_service::process::image_hash(&std::env::current_exe()?)?
    {
        return Ok(false);
    }
    ensure!(
        !is_downgrade(Some(&deployment::installed_version()?)),
        "已安装较新版本；请打开更新窗口明确确认降级"
    );
    update_running(true)
}
fn is_downgrade(installed: Option<&str>) -> bool {
    installed
        .and_then(|v| semver::Version::parse(v).ok())
        .zip(semver::Version::parse(env!("CARGO_PKG_VERSION")).ok())
        .is_some_and(|(installed, current)| installed > current)
}
pub(crate) fn uninstall(parent: Option<u32>) -> Result<()> {
    if let Some(parent) = parent {
        use windows::Win32::{Foundation::*, System::Threading::*};
        ensure!(parent != std::process::id(), "无效卸载交接");
        match unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                parent,
            )
        } {
            Ok(handle) => {
                let handle = host_service::pipe::Handle(handle);
                let identity = (|| -> Result<()> {
                    host_service::process::verify_image(parent)?;
                    ensure!(
                        host_service::vault::sid(parent)?
                            == host_service::vault::sid(std::process::id())?,
                        "卸载交接用户不匹配"
                    );
                    Ok(())
                })();
                if unsafe { WaitForSingleObject(handle.0, 0) } != WAIT_OBJECT_0 {
                    identity?;
                }
                ensure!(
                    unsafe { WaitForSingleObject(handle.0, 30_000) } == WAIT_OBJECT_0,
                    "等待原程序退出超时"
                );
            }
            Err(error) if error.code() == ERROR_INVALID_PARAMETER.to_hresult() => (),
            Err(error) => return Err(error.into()),
        }
    } else if crate::application::bootstrap::cached()
        || std::env::current_exe()?.parent() == Some(deployment::active_directory()?.as_path())
    {
        use std::os::windows::process::CommandExt;
        let directory = helpers()?;
        components::files::reject_reparse(directory.parent().context("卸载工作目录无效")?)?;
        components::files::reject_reparse(&directory)?;
        std::fs::create_dir_all(&directory)?;
        let helper = directory.join(format!("uninstall-{}.exe", uuid::Uuid::new_v4().simple()));
        std::fs::copy(std::env::current_exe()?, &helper)?;
        std::process::Command::new(&helper)
            .args(["uninstall", "--parent", &std::process::id().to_string()])
            .creation_flags(0x08000000)
            .spawn()
            .context("启动卸载程序失败")?;
        return Ok(());
    }
    run(true, Vec::new())?;
    Ok(())
}
pub(crate) fn report_error(error: &anyhow::Error) {
    use windows::{
        Win32::UI::WindowsAndMessaging::*,
        core::{PCWSTR, w},
    };
    let text: Vec<u16> = format!("{error:#}").encode_utf16().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            w!("OpenUUYC"),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}
fn helpers() -> Result<PathBuf> {
    Ok(std::env::temp_dir().join("OpenUUYC-maintenance"))
}
fn cleanup_helpers() {
    let Ok(directory) = helpers() else { return };
    if components::files::reject_reparse(&directory).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name
            .strip_prefix("uninstall-")
            .and_then(|s| s.strip_suffix(".exe"))
            .is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            && components::files::reject_reparse(&entry.path()).is_ok()
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
fn run(uninstall: bool, launch_arguments: Vec<std::ffi::OsString>) -> Result<()> {
    let _installer = super::instance::reserve_installer()?;
    if uninstall && deployment::active_directory()?.exists() {
        deployment::verify_directory(&deployment::active_directory()?)?;
    }
    let installed_version = deployment::installed_version().ok();
    crate::ui::run(
        crate::ui::WindowConfig {
            viewport: egui::ViewportBuilder::default()
                .with_title(if uninstall {
                    "卸载 OpenUUYC"
                } else {
                    "更新 OpenUUYC"
                })
                .with_icon(crate::ui::branding::icon(false))
                .with_inner_size(crate::ui::theme::MAINTENANCE_WINDOW_SIZE)
                .with_resizable(false),
            centered: true,
            notification: false,
            floating: false,
        },
        Box::new(move |ctx, _| {
            super::view::configure_visuals(ctx);
            Box::new(Maintenance {
                uninstall,
                launch_arguments,
                removal: Default::default(),
                pending: None,
                error: None,
                finished: false,
                installed_version,
            })
        }),
    )?;
    Ok(())
}
struct Maintenance {
    uninstall: bool,
    launch_arguments: Vec<std::ffi::OsString>,
    removal: components::RemovalOptions,
    pending: Option<Receiver<Result<bool, String>>>,
    error: Option<String>,
    finished: bool,
    installed_version: Option<String>,
}
impl crate::ui::App for Maintenance {
    fn uses_tray(&self) -> bool {
        false
    }
    fn on_close_requested(&mut self) -> bool {
        self.pending.is_none()
    }
    fn ui(&mut self, ui: &mut egui::Ui) {
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(result) => {
                    self.pending = None;
                    match result {
                        Ok(false) => {
                            self.finished = true;
                            if !self.uninstall {
                                self.open_installed(ui.ctx());
                            }
                        }
                        Ok(true) => self.error = Some("需要重启Windows后完成操作".into()),
                        Err(error) => self.error = Some(error),
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.error = Some("操作任务中断".into());
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }
        }
        use crate::ui::{controls, theme};
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(theme::DIALOG_MARGIN),
            )
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(if self.uninstall {
                        "卸载 OpenUUYC"
                    } else {
                        "更新 OpenUUYC"
                    })
                    .size(theme::DIALOG_TITLE),
                );
                ui.add_space(theme::DIALOG_HEADER_GAP);
                if !self.uninstall {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("当前版本").color(theme::MUTED));
                        ui.label(
                            self.installed_version
                                .as_ref()
                                .map(|v| format!("v{v}"))
                                .unwrap_or_else(|| "未知".into()),
                        );
                        ui.label(egui::RichText::new("→").color(theme::MUTED));
                        ui.label(egui::RichText::new("本次版本").color(theme::MUTED));
                        ui.label(
                            egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                                .color(theme::ACCENT),
                        );
                    });
                    ui.add_space(12.);
                }
                ui.label(if self.finished {
                    if self.removal.remove_data { "卸载完成，本机数据已清除。" } else { "卸载完成，账号和用户设置已保留。" }
                } else if self.uninstall {
                    "将断开当前连接并退出 OpenUUYC，然后移除程序、后台服务、输入驱动、快捷方式及自启动。"
                } else {
                    if is_downgrade(self.installed_version.as_deref()) {"本次版本低于已安装版本。确认后将结束当前连接并降级程序。"}else{"更新将结束当前连接并关闭运行中的 OpenUUYC，完成后自动打开新版本。"}
                });
                ui.add_space(12.);
                if !self.finished {
                    ui.add_enabled_ui(self.pending.is_none(), |ui| {
                        if self.uninstall {
                            ui.checkbox(&mut self.removal.remove_display_driver, "同时卸载 OpenUUYC 虚拟显示驱动");
                            ui.checkbox(&mut self.removal.remove_audio_driver, "同时卸载 OpenUUYC 虚拟声卡" );
                            ui.checkbox(&mut self.removal.remove_data, "同时删除本机 OpenUUYC 数据");
                            if self.removal.remove_data {
                                ui.label(egui::RichText::new("删除登录信息、设置、插件、缓存和日志，下次使用需重新登录。").color(theme::AMBER));
                            }
                        } else if ui.link("打开已安装版本").clicked() {
                            self.open_installed(ui.ctx());
                        }
                    });
                }
                ui.add_space(12.);
                let available = ui.available_height();
                let body_height = (available
                    - theme::CONTROL_HEIGHT
                    - theme::DIALOG_ACTION_GAP
                    - ui.spacing().item_spacing.y)
                    .max(0.);
                ui.allocate_ui(egui::vec2(ui.available_width(), body_height), |ui| {
                    ui.set_min_height(body_height);
                    if let Some(error) = &self.error {
                        egui::ScrollArea::vertical()
                            .max_height(body_height)
                            .show(ui, |ui| {
                                ui.colored_label(theme::RED, error);
                            });
                    } else if self.pending.is_some() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("正在处理…");
                        });
                    }
                });
                if self.finished {
                    if controls::dialog_actions(ui, Some(controls::DialogAction::new("关闭")), None)
                        .0
                    {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                } else {
                    let (yes, cancel) = ui
                        .add_enabled_ui(self.pending.is_none(), |ui| {
                            controls::dialog_actions(
                                ui,
                                Some(controls::DialogAction::new(if self.uninstall {
                                    "卸载程序"
                                } else if is_downgrade(self.installed_version.as_deref()) {
                                    "降级并打开"
                                } else {
                                    "更新并打开"
                                })),
                                Some("取消"),
                            )
                        })
                        .inner;
                    if yes {
                        self.start(ui.ctx().clone());
                    }
                    if cancel {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            });
        if self.pending.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }
}

impl Maintenance {
    fn open_installed(&mut self, ctx: &egui::Context) {
        // Start before destroying the foreground installer, otherwise Windows
        // restores another app first and the replacement opens behind it.
        match deployment::start_installed(self.launch_arguments.iter().cloned()) {
            Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Err(error) => {
                self.finished = false;
                self.error = Some(format!("打开已安装版本失败：{error:#}"));
            }
        }
    }

    fn start(&mut self, ctx: egui::Context) {
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        self.error = None;
        let uninstall = self.uninstall;
        let removal = self.removal;
        std::thread::spawn(move || {
            let result = if !uninstall {
                update_running(false)
            } else {
                maintain_running(Some(removal), true)
            }
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }
}

fn update_running(preserve_pause: bool) -> Result<bool> {
    maintain_running(None, preserve_pause)
}
/// Opening/cancelling either dialog is non-destructive. Once confirmed, both
/// operations drain the GUI and host before taking the deployment reservation.
fn maintain_running(
    removal: Option<components::RemovalOptions>,
    preserve_pause: bool,
) -> Result<bool> {
    let gate = super::instance::reserve_update().map_err(|error| UpdateExit {
        code: 170,
        message: error.to_string(),
    })?;
    let running = super::instance::running_installed()?;
    let mut had_window = running.is_some();
    let background = host_service::resident::managed() && host_service::install::running()?;
    let resume = background && !host_service::resident::paused()?;
    let mut preparing_update = false;
    let result = (|| -> Result<bool> {
        // Announce before the old GUI's normal exit path pauses the resident.
        // The protected deployment receipt distinguishes pre-notification builds
        // without sending an unknown IPC request to an older running service.
        if removal.is_none() && resume && host_service::install::supports_update_notice()? {
            preparing_update = true;
            prepare_resident_update(host_service::resident::call)?;
        }
        if let Some(running) = running {
            running.close(removal.is_none())?;
        }
        // Headless and pre-handoff builds can leave their resident online after
        // the UI exits. Pause it and wait for its agents before deployment.
        if background {
            host_service::resident::call(host_service::resident::Request::Pause)?;
        }
        let started = std::time::Instant::now();
        let _reservation = loop {
            match super::instance::reserve_after_exit()? {
                Some(guard) => break guard,
                None if started.elapsed() < Duration::from_secs(5) => {
                    if let Some(running) = super::instance::running_installed()? {
                        had_window = true;
                        running.close(removal.is_none())?;
                    }
                    std::thread::sleep(Duration::from_millis(50))
                }
                None => anyhow::bail!("运行版本仍在退出，尚未开始替换文件"),
            }
        };
        match removal {
            Some(removal) => {
                components::request(Kind::Application, Operation::Uninstall, false, removal)
            }
            None => components::request_update(!preserve_pause || !background || resume),
        }
    })();
    drop(gate);
    if let Err(error) = result {
        if preparing_update {
            let _ = host_service::resident::call(host_service::resident::Request::CancelUpdate);
        }
        // Pause can succeed before a later GUI/deployment step fails. Restore
        // the original online intent even when an existing GUI is still alive;
        // merely activating that GUI does not resume a paused resident.
        let service_remains = removal.is_none()
            || host_service::install::status()
                .map(|s| s.installed)
                .unwrap_or(true);
        let resumed = if resume && service_remains {
            host_service::resident::call(host_service::resident::Request::Resume).map(|_| ())
        } else {
            Ok(())
        };
        let image_remains = removal.is_none() || deployment::image().is_ok_and(|p| p.is_file());
        let reopened = if had_window && image_remains {
            deployment::start_installed(std::iter::empty::<std::ffi::OsString>())
        } else {
            Ok(())
        };
        let restored = match (resumed, reopened) {
            (Err(a), Err(b)) => Err(anyhow::anyhow!(
                "恢复被控失败：{a:#}；打开原版本失败：{b:#}"
            )),
            (Err(e), _) | (_, Err(e)) => Err(e),
            _ => Ok(()),
        };
        return match restored {
            Ok(()) => Err(error),
            Err(recovery) => Err(anyhow::anyhow!(
                "{error:#}；恢复原运行状态失败：{recovery:#}"
            )),
        };
    }
    result
}

fn prepare_resident_update(
    mut request: impl FnMut(host_service::resident::Request) -> Result<host_service::resident::Reply>,
) -> Result<()> {
    use host_service::resident::{Reply, Request};
    let error = match request(Request::PrepareUpdate) {
        Ok(Reply::Done) => return Ok(()),
        Ok(_) => anyhow::bail!("更新准备返回了无效响应"),
        Err(error) => error,
    };
    // The running service can still use the old mandatory-notice policy.
    // Its protected Pause closes admission and only returns Done after the
    // resident/agent owner has exited. Do not infer retirement from a transient
    // Connected flag, replay the notice, or bypass deployment's exit checks.
    tracing::warn!(error=%format!("{error:#}"), "update preparation could not notify remote; stopping old host");
    match request(Request::Pause) {
        Ok(Reply::Done) => Ok(()),
        Ok(_) => anyhow::bail!("结束旧被控后台返回无效响应，尚未开始更新"),
        Err(shutdown) => Err(anyhow::anyhow!(
            "更新通知未完成：{error:#}；结束旧被控后台失败，尚未开始更新：{shutdown:#}"
        )),
    }
}
