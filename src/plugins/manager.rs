use super::*;
use std::collections::BTreeMap;

struct Entry {
    path: PathBuf,
    manifest: Option<Manifest>,
    error: Option<String>,
    node_errors: BTreeMap<String, String>,
}
struct Edit {
    path: PathBuf,
    original: Option<Vec<u8>>,
    document: serde_json::Value,
    fields: super::parameters::Fields,
}

#[derive(Default)]
pub(crate) struct Manager {
    graphs: bool,
    editor: super::editor::Editor,
    loaded: bool,
    entries: Vec<Entry>,
    search: String,
    filter: String,
    details: Option<PathBuf>,
    edit: Option<Edit>,
    error: Option<String>,
    notice: Option<String>,
}

impl Manager {
    fn refresh(&mut self) {
        self.loaded = true;
        self.entries.clear();
        self.error = None;
        let result = (|| -> Result<()> {
            let root = root()?;
            if !root.exists() {
                return Ok(());
            }
            for path in super::metadata::candidates(&root)? {
                let result = read_manifest(&path);
                let error = result.as_ref().err().map(|e| format!("{e:#}"));
                self.entries.push(Entry {
                    path,
                    manifest: result.ok(),
                    error,
                    node_errors: BTreeMap::new(),
                });
            }
            self.entries.sort_by_key(|e| {
                e.manifest
                    .as_ref()
                    .map_or_else(|| e.path.display().to_string(), |m| m.id.clone())
            });
            let mut counts = BTreeMap::new();
            for entry in &self.entries {
                if let Some(m) = &entry.manifest {
                    *counts.entry(m.id.clone()).or_insert(0) += 1;
                }
            }
            for entry in &mut self.entries {
                if let Some(m) = &entry.manifest {
                    let result = (|| -> Result<()> {
                        ensure!(counts.get(&m.id) == Some(&1), "插件 ID 重复");
                        let dir = m.path.parent().context("缺少目录")?;
                        let library = std::fs::canonicalize(&m.path).context("缺少动态库")?;
                        ensure!(library.starts_with(dir), "动态库超出插件目录");
                        for dependency in &m.dependencies {
                            ensure!(
                                counts.get(dependency) == Some(&1),
                                "缺少或重复依赖：{dependency}"
                            );
                        }
                        let mut effective = m.clone();
                        settings::apply(&mut effective)?;
                        for key in ["model", "runtime"] {
                            if let Some(file) = effective.config.get(key).and_then(|v| v.as_str()) {
                                let resource = std::fs::canonicalize(dir.join(file))
                                    .with_context(|| format!("缺少文件：{file}"))?;
                                ensure!(resource.starts_with(dir), "资源超出插件目录");
                            }
                        }
                        Ok(())
                    })();
                    entry.error = result.err().map(|e| format!("{e:#}"));
                }
            }
            for _ in 0..self.entries.len() {
                let failed: BTreeMap<_, _> = self
                    .entries
                    .iter()
                    .filter_map(|e| {
                        e.manifest
                            .as_ref()
                            .filter(|_| e.error.is_some())
                            .map(|m| (m.id.clone(), m.name.clone()))
                    })
                    .collect();
                let mut changed = false;
                for entry in &mut self.entries {
                    if entry.error.is_none()
                        && let Some(manifest) = &entry.manifest
                        && let Some(dependency) =
                            manifest.dependencies.iter().find_map(|id| failed.get(id))
                    {
                        entry.error = Some(format!("依赖不可用：{dependency}"));
                        changed = true;
                    }
                }
                if !changed {
                    break;
                }
            }
            for entry in &mut self.entries {
                if let Some(m) = &entry.manifest {
                    for node in &m.nodes {
                        if let Err(error) = plan(&m.path, Some(&node.type_id)) {
                            entry
                                .node_errors
                                .insert(node.type_id.clone(), format!("{error:#}"));
                        }
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.error = Some(format!("{e:#}"));
        }
    }
    pub fn show(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if !self.graphs {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("插件文件夹").clicked()
                        && let Err(e) = root().and_then(|p| open_folder(&p))
                    {
                        self.error = Some(e.to_string());
                    }
                    if ui
                        .add_enabled(self.edit.is_none(), egui::Button::new("刷新"))
                        .clicked()
                    {
                        self.refresh();
                    }
                });
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.graphs, false, "已安装");
            ui.selectable_value(&mut self.graphs, true, "节点图");
        });
        ui.add_space(8.0);
        if self.graphs {
            self.editor.show(ui);
            return;
        }
        if !self.loaded {
            self.refresh();
        }
        if let Some(error) = self.error.take() {
            crate::ui::controls::notice(
                ui.ctx(),
                "plugin-manager-error",
                "插件管理",
                crate::ui::controls::DialogIcon::Error,
                error,
            );
        }
        if let Some(message) = self.notice.take() {
            crate::ui::controls::notice(
                ui.ctx(),
                "plugin-manager-notice",
                "插件管理",
                crate::ui::controls::DialogIcon::Info,
                message,
            );
        }
        if self.edit.is_some() {
            self.edit_ui(ui);
            return;
        }
        ui.horizontal(|ui| {
            let search_width = (ui.available_width() - 270.0).clamp(140.0, 340.0);
            ui.add_sized(
                [search_width, crate::ui::controls::HEIGHT],
                crate::ui::controls::singleline(&mut self.search, crate::ui::controls::HEIGHT)
                    .hint_text("搜索插件或节点"),
            );
            egui::ComboBox::from_id_salt("plugin-type")
                .width(120.0)
                .selected_text(if self.filter.is_empty() {
                    "全部类型"
                } else {
                    kind(&self.filter)
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.filter, String::new(), "全部类型");
                    for category in ["nodes", "inference"] {
                        ui.selectable_value(&mut self.filter, category.into(), kind(category));
                    }
                });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let count = self
                    .entries
                    .iter()
                    .filter(|entry| {
                        matches_filter(entry, &self.search.trim().to_lowercase(), &self.filter)
                    })
                    .count();
                ui.weak(if count == self.entries.len() {
                    format!("{count} 个插件")
                } else {
                    format!("{count} / {} 个插件", self.entries.len())
                });
            });
        });
        ui.add_space(12.0);
        let names: BTreeMap<_, _> = self
            .entries
            .iter()
            .filter_map(|e| {
                e.manifest
                    .as_ref()
                    .map(|m| (m.id.as_str(), m.name.as_str()))
            })
            .collect();
        let query = self.search.trim().to_lowercase();
        let visible: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| matches_filter(entry, &query, &self.filter))
            .collect();
        let mut configure = None;
        let mut folder = None;
        let mut details = self.details.clone();
        crate::ui::controls::page_scroll("installed-plugins").show(ui, |ui| {
            let wide = ui.available_width() >= 780.0;
            let columns = columns(ui.available_width(), wide);
            let (header, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 26.0), egui::Sense::hover());
            for (offset, label) in [
                (12.0, "插件"),
                (columns[1], "类型"),
                (columns[2], "版本"),
                (columns[3], "状态"),
                (columns[4], "操作"),
            ] {
                if !wide && matches!(label, "类型" | "版本") {
                    continue;
                }
                ui.painter().text(
                    header.left_top() + egui::vec2(offset, 8.0),
                    egui::Align2::LEFT_CENTER,
                    label,
                    egui::FontId::proportional(crate::ui::theme::SMALL),
                    crate::ui::theme::MUTED,
                );
            }
            if visible.is_empty() {
                ui.add_space(28.0);
                ui.vertical_centered(|ui| {
                    ui.weak(if self.entries.is_empty() {
                        "尚未安装插件"
                    } else {
                        "没有匹配的插件"
                    });
                });
            }
            for entry in &visible {
                ui.push_id(&entry.path, |ui| {
                    let expanded = details.as_ref() == Some(&entry.path);
                    let (row, response) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 64.0),
                        egui::Sense::hover(),
                    );
                    if expanded || response.hovered() {
                        ui.painter().rect_filled(row, 5.0, crate::ui::theme::HOVER);
                    }
                    let icon = egui::Rect::from_center_size(
                        egui::pos2(row.left() + 28.0, row.center().y),
                        egui::vec2(32.0, 32.0),
                    );
                    ui.painter()
                        .rect_filled(icon, 6.0, crate::ui::theme::SELECTED);
                    super::paint_plugin_icon(ui.painter(), icon, crate::ui::theme::ACCENT);
                    let title = entry.manifest.as_ref().map_or_else(
                        || {
                            entry
                                .path
                                .parent()
                                .and_then(Path::file_name)
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_else(|| "无效插件".into())
                        },
                        |m| m.name.clone(),
                    );
                    let subtitle = entry.manifest.as_ref().map_or_else(
                        || "无法读取插件清单".into(),
                        |m| {
                            if wide {
                                m.id.clone()
                            } else {
                                format!("{} · {} · v{}", m.id, kind(&m.capability), m.version)
                            }
                        },
                    );
                    cell(
                        ui,
                        row.shrink2(egui::vec2(0.0, 13.0)),
                        52.0,
                        columns[1] - 60.0,
                        |ui| {
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 3.0;
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&title).size(crate::ui::theme::BODY),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(&title);
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&subtitle)
                                            .size(crate::ui::theme::TINY)
                                            .color(crate::ui::theme::MUTED),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(&subtitle);
                            });
                        },
                    );
                    if wide {
                        cell(ui, row, columns[1], columns[2] - columns[1] - 8.0, |ui| {
                            ui.weak(
                                entry
                                    .manifest
                                    .as_ref()
                                    .map_or("未知", |m| kind(&m.capability)),
                            );
                        });
                        cell(ui, row, columns[2], columns[3] - columns[2] - 8.0, |ui| {
                            let version =
                                entry.manifest.as_ref().map_or("—", |m| m.version.as_str());
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(version).color(crate::ui::theme::MUTED),
                                )
                                .truncate(),
                            )
                            .on_hover_text(version);
                        });
                    }
                    cell(ui, row, columns[3], columns[4] - columns[3] - 4.0, |ui| {
                        let color = if entry.error.is_some() || !entry.node_errors.is_empty() {
                            crate::ui::theme::AMBER
                        } else {
                            crate::ui::theme::GREEN
                        };
                        let (dot, _) =
                            ui.allocate_exact_size(egui::vec2(6.0, 6.0), egui::Sense::hover());
                        ui.painter().circle_filled(dot.center(), 2.5, color);
                        ui.colored_label(
                            color,
                            if entry.error.is_some() {
                                "异常"
                            } else if !entry.node_errors.is_empty() {
                                "部分可用"
                            } else {
                                "正常"
                            },
                        )
                        .on_hover_text(
                            entry
                                .error
                                .as_deref()
                                .or_else(|| entry.node_errors.values().next().map(String::as_str))
                                .unwrap_or("安装检查通过"),
                        );
                    });
                    cell(ui, row, columns[4], row.width() - columns[4] - 8.0, |ui| {
                        if ui
                            .add_sized(
                                [48.0, crate::ui::controls::HEIGHT],
                                egui::Button::new(if expanded { "收起" } else { "详情" }),
                            )
                            .clicked()
                        {
                            details = if expanded {
                                None
                            } else {
                                Some(entry.path.clone())
                            };
                        }
                        if ui
                            .add_sized(
                                [58.0, crate::ui::controls::HEIGHT],
                                egui::Button::new("文件夹"),
                            )
                            .clicked()
                        {
                            folder = entry.path.parent().map(Path::to_owned);
                        }
                        if entry.manifest.as_ref().is_some_and(configurable)
                            && ui
                                .add_sized(
                                    [48.0, crate::ui::controls::HEIGHT],
                                    egui::Button::new("配置"),
                                )
                                .clicked()
                        {
                            configure = Some(entry.path.clone());
                        }
                    });
                    if details.as_ref() == Some(&entry.path) {
                        egui::Frame::new()
                            .inner_margin(egui::Margin {
                                left: 52,
                                right: 16,
                                top: 8,
                                bottom: 14,
                            })
                            .fill(crate::ui::theme::BG)
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                crate::ui::controls::observe_notice(
                                    ui.ctx(),
                                    ("plugin-entry-error", &entry.path),
                                    "插件检查失败",
                                    crate::ui::controls::DialogIcon::Error,
                                    entry.error.as_deref(),
                                );
                                if let Some(m) = &entry.manifest {
                                    if !m.dependencies.is_empty() {
                                        ui.label(format!(
                                            "依赖   {}",
                                            m.dependencies
                                                .iter()
                                                .map(|id| names
                                                    .get(id.as_str())
                                                    .copied()
                                                    .unwrap_or(id))
                                                .collect::<Vec<_>>()
                                                .join("、")
                                        ));
                                    }
                                    for node in &m.nodes {
                                        crate::ui::controls::observe_notice(
                                            ui.ctx(),
                                            ("plugin-node-error", &entry.path, &node.type_id),
                                            "插件节点检查失败",
                                            crate::ui::controls::DialogIcon::Error,
                                            entry
                                                .node_errors
                                                .get(&node.type_id)
                                                .map(String::as_str),
                                        );
                                        ui.horizontal_wrapped(|ui| {
                                            ui.label(&node.name);
                                            ui.weak(&node.description);
                                        });
                                    }
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(
                                                m.path
                                                    .file_name()
                                                    .unwrap_or_default()
                                                    .to_string_lossy(),
                                            )
                                            .size(crate::ui::theme::SMALL)
                                            .weak(),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(
                                        m.path.file_name().unwrap_or_default().to_string_lossy(),
                                    );
                                }
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(entry.path.display().to_string())
                                            .size(crate::ui::theme::TINY)
                                            .weak(),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(entry.path.display().to_string());
                            });
                    }
                    ui.painter().hline(
                        row.left() + 12.0..=row.right() - 12.0,
                        ui.cursor().top(),
                        egui::Stroke::new(1.0, crate::ui::theme::LINE),
                    );
                });
            }
        });
        self.details = details;
        if let Some(path) = folder
            && let Err(e) = open_folder(&path)
        {
            self.error = Some(e.to_string());
        }
        if let Some(path) = configure {
            let result = (|| -> Result<Edit> {
                let mut manifest = read_manifest(&path)?;
                settings::apply(&mut manifest)?;
                let path = super::settings::path(&manifest.id)?;
                let original = super::settings::read(&path)?;
                let document = serde_json::json!({"name":manifest.name,"id":manifest.id,"config":manifest.config});
                Ok(Edit {
                    fields: super::parameters::Fields::new(&manifest),
                    path,
                    original,
                    document,
                })
            })();
            match result {
                Ok(edit) => {
                    self.edit = Some(edit);
                    self.error = None;
                    self.notice = None;
                }
                Err(e) => self.error = Some(e.to_string()),
            }
        }
    }
    fn edit_ui(&mut self, ui: &mut egui::Ui) {
        let edit = self.edit.as_mut().expect("editing");
        let mut save = false;
        let mut cancel =
            ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(
                    edit.document
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("模块配置"),
                )
                .size(crate::ui::theme::DIALOG_TITLE)
                .strong(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                save = ui
                    .add_sized(
                        [72.0, crate::ui::controls::HEIGHT],
                        crate::ui::controls::primary("保存"),
                    )
                    .clicked();
                cancel |= ui
                    .add_sized(
                        [72.0, crate::ui::controls::HEIGHT],
                        egui::Button::new("取消"),
                    )
                    .clicked();
            });
        });
        ui.weak(
            edit.document
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
        );
        ui.add_space(18.0);
        crate::ui::controls::page_scroll("module-config").show(ui, |ui| {
            let width = (ui.available_width() - 32.0).max(0.0);
            egui::Frame::new()
                .fill(crate::ui::theme::SURFACE)
                .corner_radius(crate::ui::theme::PANEL_RADIUS)
                .inner_margin(16)
                .show(ui, |ui| {
                    ui.set_width(width);
                    ui.spacing_mut().item_spacing.y = 8.0;
                    if let Some(config) = edit.document.get_mut("config") {
                        *config = edit.fields.with_defaults(config);
                        for key in edit.fields.keys(config) {
                            edit.fields.row(ui, &key, config);
                        }
                    }
                });
        });
        if save {
            let result =
                super::settings::write(&edit.path, &edit.original, &edit.document["config"]);
            match result {
                Ok(()) => {
                    self.edit = None;
                    self.refresh();
                    self.notice = Some("配置已保存".into());
                }
                Err(e) => self.error = Some(e.to_string()),
            }
        } else if cancel {
            self.edit = None;
            self.error = None;
        }
    }
}

fn kind(capability: &str) -> &'static str {
    match capability {
        "nodes" => "节点插件",
        "inference" => "推理引擎",
        _ => "未知",
    }
}
fn configurable(m: &Manifest) -> bool {
    m.nodes.is_empty()
        && m.config.as_object().is_some_and(|c| {
            c.keys()
                .any(|key| m.config_schema.get(key).is_none_or(|f| f.kind != "hidden"))
        })
}
fn columns(width: f32, wide: bool) -> [f32; 5] {
    let actions = width - 208.0;
    let status = actions - 76.0;
    let version = if wide { status - 82.0 } else { status };
    let category = if wide { version - 96.0 } else { version };
    [0.0, category, version, status, actions]
}
fn cell(
    ui: &mut egui::Ui,
    row: egui::Rect,
    offset: f32,
    width: f32,
    add: impl FnOnce(&mut egui::Ui),
) {
    let rect = egui::Rect::from_min_size(
        row.min + egui::vec2(offset, 0.0),
        egui::vec2(width, row.height()),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(offset.to_bits())
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    crate::ui::controls::configure(child.style_mut(), crate::ui::controls::HEIGHT);
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    child.spacing_mut().item_spacing.x = 5.0;
    add(&mut child);
}

fn matches_filter(entry: &Entry, query: &str, filter: &str) -> bool {
    let category = entry
        .manifest
        .as_ref()
        .map_or("", |m| m.capability.as_str());
    if !filter.is_empty() && filter != category {
        return false;
    }
    query.is_empty()
        || entry.manifest.as_ref().map_or_else(
            || entry.path.to_string_lossy().to_lowercase().contains(query),
            |m| {
                format!(
                    "{} {} {} {}",
                    m.name,
                    m.id,
                    kind(category),
                    m.nodes
                        .iter()
                        .map(|n| n.name.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                )
                .to_lowercase()
                .contains(query)
            },
        )
}
