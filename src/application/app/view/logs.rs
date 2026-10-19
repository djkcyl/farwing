use super::*;
use crate::diagnostics::logging::{self, Level, Settings};

#[derive(Default)]
pub(super) struct LogUi {
    pub(super) export: super::diagnostic_export::ExportUi,
    draft: Option<Settings>,
    baseline: Option<Settings>,
    search: String,
    message: Option<(bool, String)>,
}

fn level_picker(ui: &mut egui::Ui, id: &str, level: &mut Level) {
    egui::ComboBox::from_id_salt(id)
        .width(148.0)
        .height(320.0)
        .selected_text(level.label())
        .show_ui(ui, |ui| {
            for choice in Level::ALL {
                ui.selectable_value(level, choice, choice.label());
            }
        });
}

impl DeviceCenterApp {
    pub(super) fn logs_page(&mut self, ui: &mut egui::Ui) {
        let Some(snapshot) = logging::snapshot() else {
            ui.label("日志系统尚未初始化");
            return;
        };
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        let state = &mut self.center_ui.logs;
        let dirty = state.draft != state.baseline;
        if state.draft.is_none() || (!dirty && state.baseline.as_ref() != Some(&snapshot.settings))
        {
            state.draft = Some(snapshot.settings.clone());
            state.baseline = Some(snapshot.settings.clone());
        }
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .button("打开日志文件夹")
                    .on_hover_text(snapshot.directory.display().to_string())
                    .clicked()
                    && let Err(e) = logging::open_directory()
                {
                    state.message = Some((false, format!("{e:#}")));
                }
            });
        });
        ui.add_space(8.0);
        self.diagnostic_export(ui);
        ui.add_space(8.0);
        let state = &mut self.center_ui.logs;
        ui.horizontal(|ui| {
            if ui.add(primary("保存")).clicked() {
                match logging::apply(state.draft.as_ref().unwrap().clone()) {
                    Ok(()) => {
                        let saved = logging::snapshot().unwrap().settings;
                        state.baseline = Some(saved.clone());
                        state.draft = Some(saved);
                        state.message = Some((true, "已保存".into()));
                    }
                    Err(e) => state.message = Some((false, format!("保存失败：{e:#}"))),
                }
            }
            if ui.button("恢复默认").clicked() {
                state.draft = Some(Settings::default());
                state.message = None;
            }
            if ui
                .add_enabled(state.draft != state.baseline, egui::Button::new("撤销修改"))
                .clicked()
            {
                state.draft = Some(snapshot.settings.clone());
                state.baseline = Some(snapshot.settings.clone());
                state.message = None;
            }
            if state.draft != state.baseline {
                ui.label(RichText::new("未保存").small().color(AMBER));
            }
        });
        if let Some((ok, message)) = state.message.take() {
            crate::ui::controls::notice(
                ui.ctx(),
                "log-settings-result",
                "日志设置",
                if ok {
                    crate::ui::controls::DialogIcon::Ready
                } else {
                    crate::ui::controls::DialogIcon::Error
                },
                message,
            );
        }
        crate::ui::controls::observe_notice(
            ui.ctx(),
            "log-settings-error",
            "日志设置",
            crate::ui::controls::DialogIcon::Error,
            snapshot.error.as_deref(),
        );
        if let Some(filter) = &snapshot.override_filter {
            ui.label(RichText::new("命令行级别生效中").color(AMBER))
                .on_hover_text(filter);
        }
        if snapshot.dropped != 0 {
            ui.label(RichText::new(format!("日志丢弃：{} 条", snapshot.dropped)).color(AMBER));
        }
        ui.add_space(8.0);
        ui.separator();
        crate::ui::controls::page_scroll("log-settings-scroll").show(ui, |ui| {
            let draft = state.draft.as_mut().unwrap();
            ui.horizontal(|ui| {
                ui.label("客户端默认");
                level_picker(ui, "application-level", &mut draft.application);
                ui.add_space(12.0);
                ui.label("第三方默认");
                level_picker(ui, "dependency-level", &mut draft.dependencies);
            });
            ui.add_space(12.0);
            ui.add(
                singleline_input(&mut state.search)
                    .hint_text("搜索模块")
                    .desired_width(ui.available_width()),
            );
            let search = state.search.trim().to_lowercase();
            for external in [false, true] {
                ui.add_space(12.0);
                ui.label(
                    RichText::new(if external {
                        "第三方组件"
                    } else {
                        "客户端模块"
                    })
                    .strong(),
                );
                for module in logging::MODULES.iter().filter(|m| {
                    m.external == external
                        && (search.is_empty()
                            || format!("{} {}", m.name, m.targets.join(" "))
                                .to_lowercase()
                                .contains(&search))
                }) {
                    ui.push_id(module.id, |ui| {
                        ui.horizontal(|ui| {
                            let width = (ui.available_width() - 166.0).max(180.0);
                            ui.allocate_ui_with_layout(
                                vec2(width, 38.0),
                                egui::Layout::top_down(Align::Min),
                                |ui| {
                                    ui.set_min_width(width);
                                    ui.set_max_width(width);
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    ui.label(module.name);
                                    let targets = module
                                        .targets
                                        .iter()
                                        .map(|target| {
                                            target.strip_prefix("farwing::").unwrap_or(target)
                                        })
                                        .collect::<Vec<_>>()
                                        .join(" / ");
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(targets).monospace().small().color(MUTED),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(module.targets.join("\n"));
                                },
                            );
                            let mut value = draft.modules.get(module.id).copied();
                            egui::ComboBox::from_id_salt("level")
                                .width(148.0)
                                .height(320.0)
                                .selected_text(value.map_or("继承上级", Level::label))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut value, None, "继承上级");
                                    for level in Level::ALL {
                                        ui.selectable_value(&mut value, Some(level), level.label());
                                    }
                                });
                            if let Some(level) = value {
                                draft.modules.insert(module.id.into(), level);
                            } else {
                                draft.modules.remove(module.id);
                            }
                        });
                        ui.separator();
                    });
                }
            }
        });
    }
}
