use crate::diagnostics::logging::{self, Level, Settings};
use crate::i18n::t;
use crate::ui::{controls, theme};
use egui::{Align, RichText, vec2};
use std::time::Duration;

#[derive(Default)]
pub(crate) struct LogSettings {
    draft: Option<Settings>,
    baseline: Option<Settings>,
    search: String,
    message: Option<(bool, String)>,
}

impl LogSettings {
    pub(crate) fn folder_button(&mut self, ui: &mut egui::Ui) {
        let Some(snapshot) = logging::snapshot() else { return };
        if ui.button(t().settings.open_log_folder)
            .on_hover_text(snapshot.directory.display().to_string()).clicked()
            && let Err(error) = logging::open_directory()
        {
            self.message = Some((false, format!("{error:#}")));
        }
    }

    pub(crate) fn show(&mut self, ui: &mut egui::Ui) {
        let text = &t().settings;
        let Some(snapshot) = logging::snapshot() else {
            ui.label(text.logging_not_ready);
            return;
        };
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        if self.draft.is_none() || (self.draft == self.baseline && self.baseline.as_ref() != Some(&snapshot.settings)) {
            self.draft = Some(snapshot.settings.clone());
            self.baseline = Some(snapshot.settings.clone());
        }
        ui.horizontal(|ui| {
            if ui.add(controls::primary(t().common.save)).clicked() {
                match logging::apply(self.draft.as_ref().unwrap().clone()) {
                    Ok(()) => {
                        let saved = logging::snapshot().unwrap().settings;
                        self.baseline = Some(saved.clone());
                        self.draft = Some(saved);
                        self.message = Some((true, t().common.saved.into()));
                    }
                    Err(error) => self.message = Some((false, (text.save_failed)(&format!("{error:#}")))),
                }
            }
            if ui.button(text.restore_defaults).clicked() {
                self.draft = Some(Settings::default());
                self.message = None;
            }
            if ui.add_enabled(self.draft != self.baseline, egui::Button::new(text.discard_changes)).clicked() {
                self.draft = Some(snapshot.settings.clone());
                self.baseline = Some(snapshot.settings.clone());
                self.message = None;
            }
            if self.draft != self.baseline {
                ui.label(RichText::new(text.unsaved).small().color(theme::AMBER));
            }
        });
        if let Some((ok, message)) = self.message.take() {
            controls::notice(ui.ctx(), "log-settings-result", text.log_settings,
                if ok { controls::DialogIcon::Ready } else { controls::DialogIcon::Error }, message);
        }
        controls::observe_notice(ui.ctx(), "log-settings-error", text.log_settings,
            controls::DialogIcon::Error, snapshot.error.as_deref());
        if let Some(filter) = &snapshot.override_filter {
            ui.label(RichText::new(text.command_line_level).color(theme::AMBER)).on_hover_text(filter);
        }
        if snapshot.dropped != 0 {
            ui.label(RichText::new((text.logs_dropped)(snapshot.dropped)).color(theme::AMBER));
        }
        ui.separator();
        controls::page_scroll("log-settings-scroll").show(ui, |ui| {
            let draft = self.draft.as_mut().unwrap();
            ui.horizontal_wrapped(|ui| {
                ui.label(text.client_default);
                level_picker(ui, "application-level", &mut draft.application);
                ui.add_space(12.0);
                ui.label(text.third_party_default);
                level_picker(ui, "dependency-level", &mut draft.dependencies);
            });
            ui.add_space(12.0);
            ui.add(controls::singleline(&mut self.search, controls::HEIGHT)
                .hint_text(text.search_modules).desired_width(ui.available_width()));
            let search = self.search.trim().to_lowercase();
            for external in [false, true] {
                ui.add_space(12.0);
                ui.label(RichText::new(if external { text.third_party_components } else { text.client_modules }).strong());
                for module in logging::MODULES.iter().filter(|module| {
                    module.external == external && (search.is_empty() || format!("{} {}", module.name, module.targets.join(" ")).to_lowercase().contains(&search))
                }) {
                    ui.push_id(module.id, |ui| {
                        ui.horizontal(|ui| {
                            let width = (ui.available_width() - 166.0).max(180.0);
                            ui.allocate_ui_with_layout(vec2(width, 38.0), egui::Layout::top_down(Align::Min), |ui| {
                                ui.set_width(width);
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.label(module.name);
                                let targets = module.targets.iter().map(|target| target.strip_prefix("farwing::").unwrap_or(target)).collect::<Vec<_>>().join(" / ");
                                ui.add(egui::Label::new(RichText::new(targets).monospace().small().color(theme::MUTED)).truncate())
                                    .on_hover_text(module.targets.join("\n"));
                            });
                            let mut value = draft.modules.get(module.id).copied();
                            egui::ComboBox::from_id_salt("level").width(148.0).height(320.0)
                                .selected_text(value.map_or(text.inherit, Level::label)).show_ui(ui, |ui| {
                                    ui.selectable_value(&mut value, None, text.inherit);
                                    for level in Level::ALL { ui.selectable_value(&mut value, Some(level), level.label()); }
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
