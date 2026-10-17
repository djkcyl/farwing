//! Declarative parameter controls shared by the graph and plugin manager.
use super::*;
use std::collections::BTreeMap;

pub fn capturing(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(egui::Id::new("plugin-key-recording")))
        .unwrap_or(false)
}
pub fn clear_capture_frame(ctx: &egui::Context) {
    ctx.data_mut(|d| d.insert_temp(egui::Id::new("plugin-key-recording"), false));
}
pub const ROW_HEIGHT: f32 = 34.0;
pub(crate) use sdk::{ParameterChoice as Choice, ParameterField as Field};
#[derive(Clone, Default)]
pub(crate) struct Fields {
    schema: BTreeMap<String, Field>,
    labels: BTreeMap<String, String>,
    defaults: serde_json::Value,
}
impl Fields {
    pub fn new(manifest: &Manifest) -> Self {
        let mut schema = manifest.config_schema.clone();
        for field in schema.values_mut().filter(|f| f.kind == "file") {
            let declared = std::mem::take(&mut field.options);
            if let Some(dir) = manifest.path.parent()
                && let Ok(files) = std::fs::read_dir(dir)
            {
                for entry in files.take(256).flatten() {
                    let path = entry.path();
                    if !path.is_file()
                        || !path.extension().is_some_and(|e| {
                            e.to_string_lossy().eq_ignore_ascii_case(&field.extension)
                        })
                    {
                        continue;
                    }
                    let Some(name) = path.file_name().and_then(|v| v.to_str()) else {
                        continue;
                    };
                    field.options.push(
                        declared
                            .iter()
                            .find(|c| c.value.as_str() == Some(name))
                            .cloned()
                            .unwrap_or(Choice {
                                label: name.into(),
                                value: name.into(),
                                updates: BTreeMap::new(),
                            }),
                    );
                }
            }
            field.options.sort_by(|a, b| a.label.cmp(&b.label));
        }
        Self {
            schema,
            labels: manifest.config_labels.clone(),
            defaults: manifest.config.clone(),
        }
    }
    pub fn keys(&self, config: &serde_json::Value) -> Vec<String> {
        let mut keys = std::collections::BTreeSet::new();
        for value in [config, &self.defaults] {
            if let Some(v) = value.as_object() {
                keys.extend(v.keys().cloned());
            }
        }
        let mut keys = keys
            .into_iter()
            .filter(|k| self.schema.get(k).is_none_or(|s| s.kind != "hidden"))
            .collect::<Vec<_>>();
        keys.sort_by_key(|k| (self.schema.get(k).map_or(u32::MAX, |s| s.order), k.clone()));
        keys
    }
    pub fn with_defaults(&self, config: &serde_json::Value) -> serde_json::Value {
        let mut value = self.defaults.as_object().cloned().unwrap_or_default();
        if let Some(config) = config.as_object() {
            value.extend(config.clone());
        }
        serde_json::Value::Object(value)
    }
    pub fn shortcuts(
        &self,
        config: &serde_json::Value,
        instance: u64,
    ) -> Result<Vec<super::graph::ActivationSpec>> {
        let fields: Vec<_> = self
            .schema
            .iter()
            .filter(|(_, f)| f.kind == "hotkey")
            .collect();
        ensure!(fields.len() <= 4, "每个功能最多四个快捷键条件");
        let mut conditions = Vec::new();
        for (index, (key, _)) in fields.into_iter().enumerate() {
            let Some(value) = config.get(key).filter(|v| !v.is_null()) else {
                continue;
            };
            let shortcut: super::hotkeys::Shortcut = serde_json::from_value(value.clone())
                .with_context(|| format!("快捷键参数无效：{key}"))?;
            if let Some(binding) = &shortcut.binding {
                ensure!(
                    super::hotkeys::valid(binding),
                    "快捷键无效或与播放器保留快捷键冲突"
                );
            }
            conditions.push(super::graph::ActivationSpec {
                port: key.clone(),
                name: self.labels.get(key).unwrap_or(key).clone(),
                hotkey: (!shortcut.disabled).then_some(super::hotkeys::Spec {
                    id: instance * 4 + index as u64,
                    binding: shortcut.binding,
                    mode: shortcut.mode,
                }),
            });
        }
        ensure!(
            conditions
                .iter()
                .filter(|c| c
                    .hotkey
                    .as_ref()
                    .is_some_and(|s| s.mode == super::hotkeys::Mode::Trigger))
                .count()
                <= 1,
            "每个功能最多一个单次触发快捷键"
        );
        Ok(conditions)
    }
    fn choices(&self, field: &Field, config: &serde_json::Value) -> Vec<Choice> {
        if field.kind != "multi" {
            return field.options.clone();
        }
        let source = config
            .get(&field.source)
            .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()));
        let source = source.or_else(|| {
            self.schema
                .get(&field.source_choice)?
                .options
                .iter()
                .find(|c| Some(&c.value) == config.get(&field.source_choice))?
                .updates
                .get(&field.source)
        });
        source
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .split(',')
            .enumerate()
            .filter(|(_, n)| !n.trim().is_empty())
            .map(|(id, name)| Choice {
                label: name.trim().into(),
                value: serde_json::json!(id),
                updates: BTreeMap::new(),
            })
            .collect()
    }
    pub fn row(
        &self,
        ui: &mut egui::Ui,
        key: &str,
        config: &mut serde_json::Value,
    ) -> Option<egui::Id> {
        let field = self.schema.get(key).cloned().unwrap_or_default();
        let options = self.choices(&field, config);
        let mut value = config.get(key).cloned().unwrap_or_default();
        let before = value.clone();
        let mut updates = BTreeMap::new();
        let control_height = ui.spacing().interact_size.y.clamp(
            crate::ui::controls::COMPACT_HEIGHT,
            crate::ui::controls::HEIGHT,
        );
        let (row, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), control_height + 8.0),
            egui::Sense::hover(),
        );
        let label_width = 116.0_f32.min(row.width() * 0.43);
        let label_rect =
            egui::Rect::from_min_max(row.min, egui::pos2(row.left() + label_width, row.bottom()));
        let control_rect = egui::Rect::from_min_max(
            egui::pos2(label_rect.right() + 10.0, row.top() + 4.0),
            egui::pos2(row.right(), row.bottom() - 4.0),
        );
        let label = self.labels.get(key).map_or(key, String::as_str);
        let mut label_ui = ui.new_child(
            egui::UiBuilder::new()
                .id_salt((key, "label"))
                .max_rect(label_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        label_ui
            .add(egui::Label::new(label).truncate())
            .on_hover_text(label);
        let mut control = ui.new_child(
            egui::UiBuilder::new()
                .id_salt((key, "value"))
                .max_rect(control_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        crate::ui::controls::configure(control.style_mut(), control_height);
        control.spacing_mut().item_spacing.x = 4.0;
        let width = control_rect.width();
        let height = control_rect.height();
        let id = control.id().with("edit");
        control.push_id("edit", |ui| {
            if field.kind == "hotkey" {
                let recording = ui.id().with("recording");
                let mut waiting = ui.data(|d| d.get_temp::<bool>(recording)).unwrap_or(false);
                let was_waiting=waiting;
                let secondary_release = ui.id().with("recorded-secondary-release");
                let suppress_secondary = ui.data(|d| d.get_temp::<bool>(secondary_release)).unwrap_or(false);
                let mut shortcut = if value.is_null() {
                    super::hotkeys::Shortcut::default()
                } else {
                    serde_json::from_value::<super::hotkeys::Shortcut>(value.clone()).unwrap_or_default()
                };
                let mode_label = match shortcut.mode {
                    super::hotkeys::Mode::Toggle => "开关",
                    super::hotkeys::Mode::Hold => "长按",
                    super::hotkeys::Mode::Trigger => "单次",
                };
                let text = if waiting {
                    "请按快捷键…".into()
                } else if shortcut.disabled {
                    "已停用".into()
                } else if let Some(binding) = &shortcut.binding {
                    format!("{} · {mode_label}", super::hotkeys::label(binding))
                } else if value.is_null() {
                    "点击录入".into()
                } else {
                    format!("未绑定 · {mode_label}")
                };
                let response = ui.add_sized([(width - height - 4.0).max(20.0), height], egui::Button::new(text))
                    .on_hover_text("左键录入 · 右键切换模式");
                if response.clicked() { waiting = true; }
                // While recording, right mouse is a valid binding, not a menu command.
                if !was_waiting && !suppress_secondary {
                    response.context_menu(|ui| {
                        for (mode, label) in [
                            (super::hotkeys::Mode::Toggle, "开关"),
                            (super::hotkeys::Mode::Hold, "长按"),
                            (super::hotkeys::Mode::Trigger, "单次触发"),
                        ] {
                            if ui.selectable_label(shortcut.mode == mode, label).clicked() {
                                shortcut.mode = mode;
                                shortcut.disabled = false;
                                value = serde_json::to_value(&shortcut).expect("shortcut");
                                waiting = false;
                                ui.close();
                            }
                        }
                    });
                }
                if crate::ui::controls::close_button(ui, "清除", height).clicked() {
                    value = serde_json::Value::Null;
                    waiting = false;
                }
                if waiting {
                    ui.data_mut(|d| d.insert_temp(egui::Id::new("plugin-key-recording"), true));
                    let events = ui.input(|i| i.events.clone());
                    for event in events {
                        let recorded=match event {
                            egui::Event::Key{key:egui::Key::Escape,pressed:true,..}=>{waiting=false;break;}
                            egui::Event::Key{key,physical_key,pressed:true,repeat:false,modifiers}=>super::hotkeys::key_code(physical_key.unwrap_or(key)).map(|key|(key,modifiers)),
                            egui::Event::PointerButton{button,pressed:true,modifiers,..} if was_waiting=>Some((match button{egui::PointerButton::Primary=>1,egui::PointerButton::Secondary=>2,egui::PointerButton::Middle=>4,egui::PointerButton::Extra1=>5,egui::PointerButton::Extra2=>6},modifiers)),
                            _=>None,
                        };
                        if let Some((key,modifiers))=recorded {
                            #[cfg(windows)]
                            let win=unsafe{windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(91)<0 || windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(92)<0};
                            // egui does not carry the Super modifier on Linux.
                            #[cfg(not(windows))]
                            let win=false;
                            let binding=super::hotkeys::Binding{key,modifiers:u8::from(modifiers.ctrl)|(u8::from(modifiers.shift)<<1)|(u8::from(modifiers.alt)<<2)|(u8::from(win)<<3)};
                            if !super::hotkeys::valid(&binding){continue;}
                            shortcut.binding=Some(binding);shortcut.disabled=false;
                            value=serde_json::to_value(&shortcut).expect("shortcut");waiting=false;break;
                        }
                    }
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(50));
                }
                let suppress_secondary = (was_waiting || suppress_secondary)
                    && ui.input(|i| i.pointer.button_down(egui::PointerButton::Secondary));
                ui.data_mut(|d| {
                    d.insert_temp(recording, waiting);
                    d.insert_temp(secondary_release, suppress_secondary);
                });
            } else if field.kind == "file" || field.kind == "choice" {
                let selected = options
                    .iter()
                    .find(|c| c.value == value)
                    .map(|c| c.label.clone())
                    .unwrap_or_else(|| value.as_str().unwrap_or("未选择").to_owned());
                egui::ComboBox::from_id_salt("choice")
                    .width(width)
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        if options.is_empty() {
                            ui.weak("没有可用选项");
                        }
                        for option in &options {
                            if ui
                                .selectable_value(&mut value, option.value.clone(), &option.label)
                                .changed()
                            {
                                updates = field.reset.clone();
                                updates.extend(option.updates.clone());
                            }
                        }
                    });
            } else if field.kind == "multi" {
                let mut selected = value.as_array().cloned();
                let text = match &selected {
                    None => "全部类别".into(),
                    Some(v) if v.is_empty() => "未选择类别".into(),
                    Some(v) => format!("已选 {} 类", v.len()),
                };
                egui::ComboBox::from_id_salt("multi")
                    .width(width)
                    .height(320.0)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .selected_text(text)
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(selected.is_none(), "全部类别")
                            .clicked()
                        {
                            selected = None;
                        }
                        if ui
                            .selectable_label(
                                selected.as_ref().is_some_and(Vec::is_empty),
                                "清空选择",
                            )
                            .clicked()
                        {
                            selected = Some(Vec::new());
                        }
                        ui.separator();
                        if options.is_empty() {
                            ui.weak("模型未提供类别表");
                        }
                        for option in &options {
                            let mut checked =
                                selected.as_ref().is_none_or(|s| s.contains(&option.value));
                            if ui.checkbox(&mut checked, &option.label).changed() {
                                let list = selected.get_or_insert_with(|| {
                                    options.iter().map(|o| o.value.clone()).collect()
                                });
                                if checked {
                                    if !list.contains(&option.value) {
                                        list.push(option.value.clone());
                                    }
                                } else {
                                    list.retain(|v| v != &option.value);
                                }
                            }
                        }
                    });
                value = selected.map_or(serde_json::Value::Null, serde_json::Value::Array);
            } else {
                match &mut value {
                    serde_json::Value::Bool(v) => {
                        ui.add(egui::Checkbox::without_text(v));
                    }
                    serde_json::Value::String(v) => {
                        ui.add_sized([width, height], crate::ui::controls::singleline(v, height));
                    }
                    serde_json::Value::Number(n) => {
                        let range = field.min.unwrap_or(-f64::MAX)..=field.max.unwrap_or(f64::MAX);
                        if let Some(mut v) = n.as_i64() {
                            if crate::ui::controls::number_input(ui, egui::vec2(width, height), egui::DragValue::new(&mut v)
                                        .speed(field.step.unwrap_or(1.0))
                                        .range(range),
                                )
                                .changed()
                            {
                                *n = v.into();
                            }
                        } else if let Some(mut v) = n.as_u64() {
                            if crate::ui::controls::number_input(ui, egui::vec2(width, height), egui::DragValue::new(&mut v)
                                        .speed(field.step.unwrap_or(1.0))
                                        .range(range),
                                )
                                .changed()
                            {
                                *n = v.into();
                            }
                        } else if let Some(mut v) = n.as_f64()
                            && crate::ui::controls::number_input(ui, egui::vec2(width, height), egui::DragValue::new(&mut v)
                                        .speed(field.step.unwrap_or(0.01))
                                        .range(range),
                                )
                                .changed()
                            && let Some(nv) = serde_json::Number::from_f64(v)
                        {
                            *n = nv;
                        }
                    }
                    _ => {
                        ui.label("…").on_hover_text(value.to_string());
                    }
                }
            }
        });
        if value != before {
            config[key] = value;
            for (key, value) in updates {
                config[key] = value;
            }
            Some(id)
        } else {
            None
        }
    }
}
