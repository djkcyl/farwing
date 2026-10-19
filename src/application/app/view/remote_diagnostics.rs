use super::*;
impl DeviceCenterApp {
    pub(super) fn fetch_diagnostics(&mut self, device: crate::account::api::DeviceInfo) {
        if self.logout_pending || self.mutation_pending {
            return;
        }
        if self
            .worker
            .commands
            .send(GuiCommand::Diagnostics {
                generation: self.login_generation,
                device,
                options: self.media,
            })
            .is_err()
        {
            self.status = StatusMessage::error("设备后台服务已停止");
        }
    }
}
pub(super) fn button_label(snapshot: Option<&crate::diagnostics::remote::Snapshot>) -> String {
    use crate::diagnostics::remote::Phase;
    let Some(state) = snapshot else {
        return "获取诊断包".into();
    };
    if state.busy {
        let phase = match state.phase {
            Phase::Connecting => return "正在连接…".into(),
            Phase::Preparing => "打包",
            Phase::Downloading => "下载",
            Phase::Saving => return "正在保存…".into(),
        };
        return if state.progress.total > 0 {
            format!("{} {:.0}%", phase, state.progress.fraction() * 100.0)
        } else {
            format!("正在{}…", phase)
        };
    }
    if state.path.is_some() {
        "诊断包已保存".into()
    } else if state.error.is_some() {
        "获取失败".into()
    } else {
        "获取诊断包".into()
    }
}

pub(super) fn popup(
    response: &egui::Response,
    controller: &str,
    target: &str,
    snapshot: Option<&crate::diagnostics::remote::Snapshot>,
    can_start: bool,
) -> bool {
    let mut start = response.clicked() && can_start && snapshot.is_none_or(|s| s.cancelled);
    egui::Popup::from_toggle_button_response(response)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_width(theme::DIAGNOSTIC_POPUP_WIDTH);
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 10.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("远端诊断包")
                        .size(theme::SECTION)
                        .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if crate::ui::controls::close_button(
                        ui,
                        "收起面板",
                        crate::ui::controls::COMPACT_HEIGHT,
                    )
                    .clicked()
                    {
                        ui.close();
                    }
                });
            });
            ui.separator();
            match snapshot {
                Some(state) if state.busy => {
                    let p = &state.progress;
                    if p.total > 0 {
                        ui.label(&p.stage);
                        ui.add(egui::ProgressBar::new(p.fraction()).show_percentage());
                    } else {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(16.0));
                            ui.label(&p.stage);
                        });
                    }
                    if ui.button("取消获取").clicked() {
                        crate::diagnostics::remote::service::cancel(controller, target);
                    }
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                }
                Some(state) if state.path.is_some() => {
                    ui.label(
                        egui::RichText::new(if state.warnings > 0 {
                            "已保存，部分日志未包含"
                        } else {
                            "诊断包已保存到本机"
                        })
                        .color(if state.warnings > 0 {
                            theme::AMBER
                        } else {
                            theme::GREEN
                        }),
                    );
                    let path = state.path.as_ref().unwrap();
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(
                                path.file_name().unwrap_or_default().to_string_lossy(),
                            )
                            .size(theme::SMALL)
                            .color(theme::MUTED),
                        )
                        .truncate(),
                    )
                    .on_hover_text(path.display().to_string());
                    ui.horizontal(|ui| {
                        if ui.button("打开文件夹").clicked() {
                            if let Err(error) = crate::diagnostics::bundle::open_folder(path) {
                                tracing::warn!(%error,"open diagnostic folder failed");
                                crate::ui::controls::notice(
                                    ui.ctx(),
                                    ("diagnostic-folder", target),
                                    "无法打开文件夹",
                                    crate::ui::controls::DialogIcon::Error,
                                    "诊断包已保存，但文件夹未能打开，请从日志目录查看。",
                                );
                            }
                        }
                        start |= ui
                            .add_enabled(can_start, egui::Button::new("重新获取"))
                            .clicked();
                    });
                }
                Some(state) if state.error.is_some() => {
                    ui.label(
                        egui::RichText::new(state.error.as_deref().unwrap()).color(theme::AMBER),
                    );
                    start |= ui
                        .add_enabled(can_start, egui::Button::new("重试"))
                        .clicked();
                }
                Some(_) => {
                    ui.label(egui::RichText::new("已取消本次获取").color(theme::MUTED));
                    start |= ui
                        .add_enabled(can_start, egui::Button::new("重新获取"))
                        .clicked();
                }
                None => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(16.0));
                        ui.label("正在准备连接…");
                    });
                }
            }
        });
    start
}
