use super::{graph::*, *};
use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};
use std::collections::{BTreeMap, BTreeSet};

mod wires;

#[derive(Default)]
pub(crate) struct Editor {
    catalog: Option<Catalog>,
    document: Option<Document>,
    baseline: Option<serde_json::Value>,
    pending_switch: Option<DocumentSwitch>,
    saved: Vec<Document>,
    selected: BTreeSet<String>,
    undo: Vec<Document>,
    redo: Vec<Document>,
    pan: Vec2,
    zoom: f32,
    wire: Option<(Endpoint, bool)>,
    selection_start: Option<Pos2>,
    clipboard: Option<(Vec<Node>, Vec<Edge>)>,
    message: String,
    fit_pending: bool,
    parameter_edit: Option<egui::Id>,
    wire_routes: wires::Cache,
    reroute_selection: Option<(String, usize)>,
}
enum DocumentSwitch {
    New,
    Open(Box<Document>),
}
struct LibraryDrag {
    graph_id: String,
    node: Node,
}
const DROP_ANCHOR: Vec2 = Vec2::new(24.0, 15.0);
const PORT_TOP: f32 = 88.0;
fn parameter_top(node: &Node, catalog: &Catalog) -> f32 {
    PORT_TOP
        + catalog
            .ports(node, false)
            .len()
            .max(catalog.ports(node, true).len()) as f32
            * 24.0
        + 4.0
}
fn node_size(node: &Node, catalog: &Catalog, edges: &[Edge]) -> Vec2 {
    let count = catalog
        .types
        .get(&node.type_id)
        .map_or(0, |t| t.fields.keys(&node.parameters).len());
    Vec2::new(
        320.0,
        parameter_top(node, catalog)
            + 8.0
            + count as f32 * super::parameters::ROW_HEIGHT
            + if node.type_id == OVERLAY {
                26.0 + edges.iter().filter(|e| e.to.node == node.id).count() as f32 * 32.0
            } else {
                0.0
            },
    )
}
fn node_name<'a>(node: &'a Node, catalog: &'a Catalog) -> &'a str {
    catalog
        .types
        .get(&node.type_id)
        .map_or(node.type_id.as_str(), |t| t.definition.name.as_str())
}
// Node purpose is distinct from the data types carried by its ports.
fn node_accent(type_id: &str, ty: Option<&NodeType>) -> Color32 {
    match type_id {
        RAW => crate::ui::theme::graph::SOURCE,
        VIDEO => crate::ui::theme::graph::VIDEO,
        OVERLAY => crate::ui::theme::graph::OVERLAY,
        INPUT => crate::ui::theme::graph::INPUT,
        _ => match ty.map(|t| &t.definition.implementation) {
            Some(sdk::NodeImplementation::VideoShader) => crate::ui::theme::graph::VIDEO,
            Some(sdk::NodeImplementation::FrameAnalysis) => crate::ui::theme::graph::ANALYSIS,
            Some(sdk::NodeImplementation::Overlay | sdk::NodeImplementation::SceneSource) => {
                crate::ui::theme::graph::OVERLAY
            }
            Some(sdk::NodeImplementation::DetectionControl) => crate::ui::theme::graph::INPUT,
            None => crate::ui::theme::MUTED,
        },
    }
}
fn color(kind: &sdk::PortType) -> Color32 {
    match kind {
        sdk::PortType::Frame => crate::ui::theme::graph::PORT_FRAME,
        sdk::PortType::DrawList => crate::ui::theme::graph::PORT_DRAW,
        sdk::PortType::Layer => crate::ui::theme::graph::PORT_LAYER,
        sdk::PortType::Detections => crate::ui::theme::graph::PORT_DETECTIONS,
        sdk::PortType::InputCommands => crate::ui::theme::graph::PORT_INPUT,
        sdk::PortType::Activation => crate::ui::theme::graph::PORT_ACTIVATION,
    }
}
impl Editor {
    fn checkpoint(&mut self) {
        if let Some(doc) = &self.document {
            self.undo.push(doc.clone());
            if self.undo.len() > 32 {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
    }
    fn refresh(&mut self) {
        match Catalog::load() {
            Ok(c) => self.catalog = Some(c),
            Err(e) => self.message = format!("{e:#}"),
        };
        match list() {
            Ok(d) => self.saved = d,
            Err(e) => self.message = e.to_string(),
        }
    }
    fn new_document(&mut self) {
        if let Some(catalog) = &self.catalog {
            match Document::new(catalog) {
                Ok(doc) => {
                    self.baseline = serde_json::to_value(&doc).ok();
                    self.document = Some(doc);
                    self.undo.clear();
                    self.redo.clear();
                    self.selected.clear();
                    self.wire = None;
                    self.reroute_selection = None;
                    self.pan = Vec2::ZERO;
                    self.zoom = 1.0;
                    self.fit_pending = true;
                    self.message.clear();
                }
                Err(e) => self.message = e.to_string(),
            }
        }
    }
    pub fn show(&mut self, ui: &mut egui::Ui) {
        super::parameters::clear_capture_frame(ui.ctx());
        if self.catalog.is_none() {
            self.refresh();
            self.new_document();
        }
        let mut load = None;
        let mut create = false;
        egui::Frame::new()
            .fill(crate::ui::theme::BG)
            .stroke(Stroke::new(1.0, crate::ui::theme::LINE))
            .corner_radius(crate::ui::theme::PANEL_RADIUS)
            .inner_margin(10)
            .show(ui, |ui| {
                crate::ui::controls::configure(ui.style_mut(), crate::ui::controls::HEIGHT);
                ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);
                ui.horizontal(|ui| {
                    ui.menu_button("文件", |ui| {
                        if ui.button("新建节点图").clicked() {
                            create = true;
                            ui.close();
                        }
                        ui.separator();
                        ui.weak("打开节点图");
                        if self.saved.is_empty() {
                            ui.weak("暂无已保存的节点图");
                        }
                        egui::ScrollArea::vertical()
                            .max_height(240.0)
                            .show(ui, |ui| {
                                for doc in &self.saved {
                                    let selected = self
                                        .document
                                        .as_ref()
                                        .is_some_and(|d| d.graph_id == doc.graph_id);
                                    if ui.selectable_label(selected, &doc.name).clicked() {
                                        load = Some(doc.clone());
                                        ui.close();
                                    }
                                }
                            });
                        ui.separator();
                        if ui.button("打开图文件夹").clicked() {
                            if let Err(e) = directory().and_then(|p| open_folder(&p)) {
                                self.message = e.to_string();
                            }
                            ui.close();
                        }
                    });
                    let name_width = (ui.available_width() - 254.0).clamp(140.0, 380.0);
                    if let Some(doc) = &mut self.document {
                        ui.add_sized(
                            [name_width, crate::ui::controls::HEIGHT],
                            crate::ui::controls::singleline(
                                &mut doc.name,
                                crate::ui::controls::HEIGHT,
                            )
                            .hint_text("节点图名称"),
                        )
                        .on_hover_text("节点图名称");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_sized(
                                [64.0, crate::ui::controls::HEIGHT],
                                crate::ui::controls::primary("应用"),
                            )
                            .on_hover_text("保存并应用到使用此图的观看窗口")
                            .clicked()
                        {
                            self.save(true);
                        }
                        if ui
                            .add_sized(
                                [64.0, crate::ui::controls::HEIGHT],
                                egui::Button::new("保存"),
                            )
                            .on_hover_text("仅保存草稿")
                            .clicked()
                        {
                            self.save(false);
                        }
                        if ui
                            .add_sized(
                                [64.0, crate::ui::controls::HEIGHT],
                                egui::Button::new("校验"),
                            )
                            .on_hover_text("检查节点和连线")
                            .clicked()
                        {
                            self.validate();
                        }
                    });
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !self.undo.is_empty(),
                            egui::Button::new("撤销")
                                .min_size(Vec2::new(56.0, crate::ui::controls::HEIGHT)),
                        )
                        .on_hover_text("撤销上一步")
                        .clicked()
                    {
                        self.undo();
                    }
                    if ui
                        .add_enabled(
                            !self.redo.is_empty(),
                            egui::Button::new("重做")
                                .min_size(Vec2::new(56.0, crate::ui::controls::HEIGHT)),
                        )
                        .on_hover_text("重做上一步")
                        .clicked()
                    {
                        self.redo();
                    }
                    ui.separator();
                    if ui
                        .add_sized(
                            [96.0, crate::ui::controls::HEIGHT],
                            egui::Button::new("刷新节点库"),
                        )
                        .clicked()
                    {
                        self.refresh();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.menu_button("帮助", |ui| {
                            for (action, gesture) in [
                                ("添加节点", "从节点库拖入"),
                                ("连线", "拖动端口"),
                                ("添加中继点", "双击连线"),
                                ("平移画布", "中键拖动"),
                                ("缩放画布", "滚轮"),
                                ("多选节点", "Ctrl + 单击"),
                            ] {
                                ui.horizontal(|ui| {
                                    ui.add_sized([105.0, 24.0], egui::Label::new(action));
                                    ui.weak(gesture);
                                });
                            }
                        });
                        ui.separator();
                        ui.add_sized(
                            [42.0, crate::ui::controls::HEIGHT],
                            egui::Label::new(
                                egui::RichText::new(format!("{:.0}%", self.zoom * 100.0)).weak(),
                            ),
                        );
                        if ui
                            .add_sized(
                                [72.0, crate::ui::controls::HEIGHT],
                                egui::Button::new("适应画布"),
                            )
                            .clicked()
                        {
                            self.fit_pending = true;
                        }
                        if ui
                            .add_sized(
                                [72.0, crate::ui::controls::HEIGHT],
                                egui::Button::new("整理节点"),
                            )
                            .on_hover_text("重新排列并清除中继点，可撤销")
                            .clicked()
                        {
                            self.arrange();
                        }
                    });
                });
            });
        if create || load.is_some() {
            let next = load.map_or(DocumentSwitch::New, |doc| {
                DocumentSwitch::Open(Box::new(doc))
            });
            let current = self
                .document
                .as_ref()
                .and_then(|doc| serde_json::to_value(doc).ok());
            if current != self.baseline {
                self.pending_switch = Some(next);
            } else {
                self.switch_document(next);
            }
        }
        if !self.message.is_empty() {
            crate::ui::controls::notice(
                ui.ctx(),
                "plugin-editor",
                "节点图",
                crate::ui::controls::DialogIcon::Info,
                std::mem::take(&mut self.message),
            );
        }
        if self.document.is_none() || self.catalog.is_none() {
            return;
        }
        ui.add_space(6.0);
        let height = ui.available_height().max(240.0);
        let width = ui.available_width();
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(155.0, height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| self.library(ui),
            );
            let size = Vec2::new((width - 155.0 - 8.0).max(260.0), height);
            let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
            self.canvas(ui, rect, response);
        });
        self.switch_confirmation(ui.ctx());
    }
    fn switch_document(&mut self, next: DocumentSwitch) {
        match next {
            DocumentSwitch::New => self.new_document(),
            DocumentSwitch::Open(doc) => {
                self.baseline = serde_json::to_value(&doc).ok();
                self.document = Some(*doc);
                self.selected.clear();
                self.wire = None;
                self.reroute_selection = None;
                self.undo.clear();
                self.redo.clear();
                self.pan = Vec2::ZERO;
                self.zoom = 1.0;
                self.fit_pending = true;
                self.message.clear();
            }
        }
        self.parameter_edit = None;
    }
    fn switch_confirmation(&mut self, ctx: &egui::Context) {
        let Some(next) = self.pending_switch.take() else {
            return;
        };
        let mut discard = false;
        let mut cancel = false;
        let response = egui::Modal::new(egui::Id::new("graph-unsaved-changes"))
            .frame(crate::ui::controls::dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(360.0);
                cancel = crate::ui::controls::dialog_header(
                    ui,
                    "节点图尚未保存",
                    crate::ui::controls::DialogIcon::Warning,
                    true,
                );
                ui.label("切换会丢弃当前修改。可以先继续编辑并保存，再切换节点图。");
                let (accept, dismiss) = crate::ui::controls::dialog_actions(
                    ui,
                    Some(crate::ui::controls::DialogAction::new("继续编辑")),
                    Some("放弃修改并切换"),
                );
                cancel |= accept;
                discard = dismiss;
            });
        if discard {
            self.switch_document(next);
        } else if !cancel && !response.should_close() {
            self.pending_switch = Some(next);
        }
    }
    fn save(&mut self, apply: bool) {
        let Some(doc) = &self.document else {
            return;
        };
        let result = if apply {
            publish(doc, self.catalog.as_ref().expect("catalog"))
        } else {
            save(doc)
        };
        match result {
            Ok(()) => {
                self.message = if apply {
                    "已应用"
                } else {
                    "已保存草稿"
                }
                .into();
                self.baseline = serde_json::to_value(doc).ok();
                self.saved = list().unwrap_or_default();
            }
            Err(e) => self.message = format!("{e:#}"),
        }
    }
    fn arrange(&mut self) {
        self.checkpoint();
        let Some(doc) = &mut self.document else {
            return;
        };
        let catalog = self.catalog.as_ref().expect("catalog");
        let mut levels: BTreeMap<String, usize> = BTreeMap::new();
        for _ in 0..doc.nodes.len() {
            let mut progressed = false;
            for node in &doc.nodes {
                if levels.contains_key(&node.id) {
                    continue;
                }
                let sources = doc
                    .edges
                    .iter()
                    .filter(|e| e.to.node == node.id)
                    .map(|e| &e.from.node)
                    .collect::<Vec<_>>();
                if sources.iter().all(|id| levels.contains_key(*id)) {
                    let level = sources.iter().map(|id| levels[*id] + 1).max().unwrap_or(0);
                    levels.insert(node.id.clone(), level);
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }
        let mut occupied: BTreeMap<usize, Vec<(f32, f32)>> = BTreeMap::new();
        for node in &mut doc.nodes {
            let level = levels.get(&node.id).copied().unwrap_or(0);
            let slots = occupied.entry(level).or_default();
            let y = slots.last().map_or(40., |(_, bottom)| *bottom + 40.);
            node.position = [40. + level as f32 * 380., y];
            slots.push((y, y + node_size(node, catalog, &doc.edges).y));
        }
        for edge in &mut doc.edges {
            edge.reroutes.clear();
        }
        self.reroute_selection = None;
        self.fit_pending = true;
    }
    fn validate(&mut self) {
        if let (Some(doc), Some(catalog)) = (&self.document, &self.catalog) {
            self.message = match compile(doc, catalog) {
                Ok(plan) => {
                    if plan.warnings.is_empty() {
                        "校验通过".into()
                    } else {
                        format!("校验通过\n{}", plan.warnings.join("\n"))
                    }
                }
                Err(errors) => errors
                    .into_iter()
                    .map(|d| d.message)
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
        }
    }
    fn undo(&mut self) {
        self.parameter_edit = None;
        if let Some(doc) = self.undo.pop() {
            if let Some(current) = self.document.replace(doc) {
                self.redo.push(current);
            }
            self.selected.clear();
            self.wire = None;
            self.reroute_selection = None;
        }
    }
    fn redo(&mut self) {
        self.parameter_edit = None;
        if let Some(doc) = self.redo.pop() {
            if let Some(current) = self.document.replace(doc) {
                self.undo.push(current);
            }
            self.selected.clear();
            self.wire = None;
            self.reroute_selection = None;
        }
    }
    fn library(&mut self, ui: &mut egui::Ui) {
        ui.strong("节点库");
        let types = self
            .catalog
            .as_ref()
            .expect("catalog")
            .types
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut add = None;
        egui::ScrollArea::vertical()
            .id_salt("node-library")
            .show(ui, |ui| {
                let mut groups: BTreeMap<String, Vec<_>> = BTreeMap::new();
                for ty in &types {
                    let category = if ty.definition.category.is_empty() {
                        "其他"
                    } else {
                        &ty.definition.category
                    };
                    groups.entry(category.to_owned()).or_default().push(ty);
                }
                for (category, types) in groups {
                    egui::CollapsingHeader::new(&category)
                        .id_salt((&category, "nodes"))
                        .default_open(true)
                        .show(ui, |ui| {
                            for ty in types {
                                let accent = node_accent(&ty.definition.type_id, Some(ty));
                                let response = ui
                                    .push_id(&ty.definition.type_id, |ui| {
                                        ui.add_sized(
                                            [ui.available_width(), 28.0],
                                            egui::Button::new(
                                                egui::RichText::new(&ty.definition.name)
                                                    .color(accent),
                                            )
                                            .sense(Sense::click_and_drag())
                                            .fill(
                                                crate::ui::theme::SURFACE
                                                    .lerp_to_gamma(accent, 0.10),
                                            )
                                            .stroke(
                                                Stroke::new(
                                                    1.0,
                                                    crate::ui::theme::LINE
                                                        .lerp_to_gamma(accent, 0.30),
                                                ),
                                            ),
                                        )
                                    })
                                    .inner
                                    .on_hover_text(format!(
                                        "{}\n\n按住拖入画布 · 点击添加",
                                        ty.definition.description
                                    ))
                                    .on_hover_cursor(egui::CursorIcon::Grab);
                                if response.clicked() {
                                    add = Some(ty.definition.type_id.clone());
                                }
                                if response.drag_started_by(egui::PointerButton::Primary)
                                    && ui.input(|i| i.focused)
                                {
                                    match self
                                        .catalog
                                        .as_ref()
                                        .expect("catalog")
                                        .instantiate(&ty.definition.type_id, [0.0, 0.0])
                                    {
                                        Ok(node) => {
                                            self.wire = None;
                                            self.selection_start = None;
                                            response.dnd_set_drag_payload(LibraryDrag {
                                                graph_id: self
                                                    .document
                                                    .as_ref()
                                                    .expect("document")
                                                    .graph_id
                                                    .clone(),
                                                node,
                                            });
                                        }
                                        Err(error) => self.message = error.to_string(),
                                    }
                                }
                            }
                        });
                }
            });
        if let Some(id) = add {
            let position = [
                (80.0 - self.pan.x) / self.zoom,
                (80.0 - self.pan.y) / self.zoom,
            ];
            match self
                .catalog
                .as_ref()
                .expect("catalog")
                .instantiate(&id, position)
            {
                Ok(node) => self.insert_node(node),
                Err(error) => self.message = error.to_string(),
            }
        }
    }
    fn insert_node(&mut self, node: Node) {
        if self
            .document
            .as_ref()
            .is_none_or(|doc| doc.nodes.len() >= 64)
        {
            self.message = "节点数量已达上限".into();
            return;
        }
        self.checkpoint();
        self.parameter_edit = None;
        self.reroute_selection = None;
        self.selection_start = None;
        self.wire = None;
        self.selected = BTreeSet::from([node.id.clone()]);
        self.document.as_mut().expect("document").nodes.push(node);
    }
    fn library_drop(
        &mut self,
        ui: &mut egui::Ui,
        canvas: Rect,
        transform: egui::emath::TSTransform,
    ) {
        let Some(payload) = egui::DragAndDrop::payload::<LibraryDrag>(ui.ctx()) else {
            return;
        };
        if !ui.input(|i| i.focused)
            || self
                .document
                .as_ref()
                .is_none_or(|d| d.graph_id != payload.graph_id)
        {
            egui::DragAndDrop::clear_payload(ui.ctx());
            return;
        }
        let Some(pointer) = ui.input(|i| i.pointer.interact_pos()) else {
            return;
        };
        let catalog = self.catalog.as_ref().expect("catalog");
        let ty = catalog.types.get(&payload.node.type_id);
        let inside = canvas.contains(pointer) && !ui.ctx().any_popup_open();
        let capacity = self.document.as_ref().is_some_and(|d| d.nodes.len() < 64);
        let valid = inside
            && capacity
            && ty.is_some_and(|t| t.definition.schema_version == payload.node.type_version);
        ui.ctx().set_cursor_icon(if valid {
            egui::CursorIcon::Copy
        } else {
            egui::CursorIcon::NotAllowed
        });
        let position = transform.inverse() * pointer - DROP_ANCHOR;
        if ui.input(|i| i.pointer.button_released(egui::PointerButton::Primary)) {
            egui::DragAndDrop::take_payload::<LibraryDrag>(ui.ctx());
            if valid {
                let mut node = payload.node.clone();
                node.position = [position.x, position.y];
                self.insert_node(node);
                ui.ctx().request_repaint();
            } else if inside && !capacity {
                self.message = "节点数量已达上限".into();
            }
            return;
        }
        let accent = node_accent(&payload.node.type_id, ty);
        let painter = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Tooltip,
            ui.id().with("library-preview"),
        ));
        if inside {
            let size = node_size(&payload.node, catalog, &[]) * transform.scaling;
            let rect = Rect::from_min_size(transform * position, size);
            let painter = painter.with_clip_rect(canvas);
            painter.rect_filled(rect, 5.0, crate::ui::theme::SURFACE.gamma_multiply(0.82));
            painter.rect_stroke(
                rect,
                5.0,
                Stroke::new(1.5, if valid { accent } else { crate::ui::theme::RED }),
                egui::StrokeKind::Inside,
            );
            let header = Rect::from_min_size(rect.min, Vec2::new(size.x, 30.0 * transform.scaling));
            painter.rect_filled(
                header,
                4.0,
                crate::ui::theme::SURFACE.lerp_to_gamma(accent, 0.42),
            );
            painter.text(
                header.left_center() + Vec2::new(9.0 * transform.scaling, 0.0),
                egui::Align2::LEFT_CENTER,
                node_name(&payload.node, catalog),
                egui::FontId::proportional(crate::ui::theme::COMPACT_TEXT * transform.scaling),
                Color32::WHITE,
            );
        } else {
            let text = node_name(&payload.node, catalog);
            let galley = painter.layout_no_wrap(
                text.into(),
                egui::FontId::proportional(crate::ui::theme::COMPACT_TEXT),
                accent,
            );
            let badge = Rect::from_min_size(
                pointer + Vec2::splat(14.0),
                galley.size() + Vec2::new(20.0, 12.0),
            );
            painter.rect_filled(badge, 5.0, crate::ui::theme::SURFACE);
            painter.galley(badge.min + Vec2::new(10.0, 6.0), galley, accent);
        }
    }
    fn delete_selected(&mut self) {
        if self.selected.is_empty() {
            return;
        }
        self.checkpoint();
        let doc = self.document.as_mut().expect("document");
        doc.nodes.retain(|n| !self.selected.contains(&n.id));
        doc.edges.retain(|e| {
            !self.selected.contains(&e.from.node) && !self.selected.contains(&e.to.node)
        });
        self.selected.clear();
        self.wire = None;
    }
    fn canvas(&mut self, ui: &mut egui::Ui, rect: Rect, _response: egui::Response) {
        if std::mem::take(&mut self.fit_pending)
            && let Some(doc) = &self.document
        {
            let mut bounds = Rect::NOTHING;
            for n in &doc.nodes {
                bounds = bounds.union(Rect::from_min_size(
                    Pos2::new(n.position[0], n.position[1]),
                    node_size(n, self.catalog.as_ref().expect("catalog"), &doc.edges),
                ));
            }
            for point in doc
                .edges
                .iter()
                .flat_map(|e| &e.reroutes)
                .filter(|p| p.iter().all(|x| x.is_finite()))
            {
                bounds.extend_with(Pos2::new(point[0], point[1]));
            }
            if bounds.is_positive() {
                self.zoom = ((rect.width() - 32.0) / bounds.width())
                    .min((rect.height() - 32.0) / bounds.height())
                    .clamp(0.35, 1.0);
                self.pan = (rect.size() - bounds.size() * self.zoom) * 0.5
                    - bounds.min.to_vec2() * self.zoom;
            }
        }
        let painter = ui.painter().with_clip_rect(rect);
        painter.rect_filled(rect, 4.0, crate::ui::theme::SIDEBAR);
        let pointer = ui.input(|i| i.pointer.hover_pos());
        if pointer.is_some_and(|p| rect.contains(p))
            && !ui.ctx().egui_wants_keyboard_input()
            && !ui.ctx().any_popup_open()
        {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                let before = self.zoom;
                self.zoom = (self.zoom * (scroll * 0.002).exp()).clamp(0.35, 1.75);
                if let Some(p) = pointer {
                    let relative = p - rect.min;
                    self.pan = relative - (relative - self.pan) * (self.zoom / before);
                }
            }
        }
        if ui.input(|i| i.pointer.button_down(egui::PointerButton::Middle))
            && pointer.is_some_and(|p| rect.contains(p))
        {
            self.pan += ui.input(|i| i.pointer.delta());
        }
        let transform = egui::emath::TSTransform {
            scaling: self.zoom,
            translation: rect.min.to_vec2() + self.pan,
        };
        let layer = egui::LayerId::new(ui.layer_id().order, ui.id().with("node-world"));
        ui.ctx().set_sublayer(ui.layer_id(), layer);
        ui.ctx().set_transform_layer(layer, transform);
        let world_rect = transform.inverse() * rect;
        let mut world = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("world")
                .layer_id(layer)
                .max_rect(world_rect),
        );
        world.set_clip_rect(world_rect);
        world.style_mut().override_font_id =
            Some(egui::FontId::proportional(crate::ui::theme::SMALL));
        world.spacing_mut().item_spacing = Vec2::ZERO;
        let response = world.interact(
            world_rect,
            world.id().with("background"),
            Sense::click_and_drag(),
        );
        self.world_canvas(&mut world, world_rect, response, transform.inverse());
        self.library_drop(ui, rect, transform);
    }
    fn world_canvas(
        &mut self,
        ui: &mut egui::Ui,
        rect: Rect,
        response: egui::Response,
        inverse: egui::emath::TSTransform,
    ) {
        let painter = ui.painter().with_clip_rect(rect);
        let pointer = ui.input(|i| i.pointer.hover_pos()).map(|p| inverse * p);
        let gap = 32.0;
        let mut x = rect.left() - rect.left().rem_euclid(gap);
        while x < rect.right() {
            painter.vline(
                x,
                rect.y_range(),
                Stroke::new(0.5, crate::ui::theme::LINE.gamma_multiply(0.55)),
            );
            x += gap;
        }
        let mut y = rect.top() - rect.top().rem_euclid(gap);
        while y < rect.bottom() {
            painter.hline(
                rect.x_range(),
                y,
                Stroke::new(0.5, crate::ui::theme::LINE.gamma_multiply(0.55)),
            );
            y += gap;
        }
        let doc = self.document.as_ref().expect("document").clone();
        let catalog = self.catalog.as_ref().expect("catalog").clone();
        let mut ports: Vec<(Endpoint, bool, Pos2, sdk::PortType)> = Vec::new();
        let mut rectangles = BTreeMap::new();
        for node in &doc.nodes {
            let inputs = catalog.ports(node, false);
            let outputs = catalog.ports(node, true);
            let r = Rect::from_min_size(
                Pos2::new(node.position[0], node.position[1]),
                node_size(node, &catalog, &doc.edges),
            );
            rectangles.insert(node.id.clone(), r);
            for (is_output, list) in [(false, inputs), (true, outputs)] {
                for (index, port) in list.iter().enumerate() {
                    ports.push((
                        Endpoint {
                            node: node.id.clone(),
                            port: port.id.clone(),
                        },
                        is_output,
                        Pos2::new(
                            if is_output { r.right() } else { r.left() },
                            r.top() + (PORT_TOP + index as f32 * 24.0),
                        ),
                        port.data_type.clone(),
                    ));
                }
            }
        }
        let mut connections = Vec::new();
        let mut wire_colors = Vec::new();
        for edge in &doc.edges {
            if let (Some(a), Some(b)) = (
                ports.iter().find(|(p, o, _, _)| *o && *p == edge.from),
                ports.iter().find(|(p, o, _, _)| !*o && *p == edge.to),
            ) {
                connections.push(wires::Connection {
                    id: edge.id.clone(),
                    from: a.2,
                    to: b.2,
                    via: edge.reroutes.clone(),
                });
                wire_colors.push(color(&a.3));
            }
        }
        self.wire_routes.update(&connections);
        let over_node = pointer.is_some_and(|p| rectangles.values().any(|r| r.contains(p)));
        let near_knot = pointer.and_then(|p| {
            doc.edges.iter().find(|e| {
                e.reroutes
                    .iter()
                    .any(|q| p.distance(Pos2::new(q[0], q[1])) < 8. * inverse.scaling)
            })
        });
        let hovered = pointer
            .filter(|_| !over_node && self.wire.is_none() && near_knot.is_none())
            .and_then(|p| self.wire_routes.hit(p, 6. * inverse.scaling));
        let focused = hovered
            .map(|h| h.wire)
            .or_else(|| near_knot.and_then(|e| connections.iter().position(|c| c.id == e.id)));
        for (index, wire_color) in wire_colors.iter().copied().enumerate() {
            if focused == Some(index) {
                continue;
            }
            self.wire_routes.paint(
                &painter,
                index,
                if focused.is_some() {
                    wire_color.gamma_multiply(0.18)
                } else {
                    wire_color
                },
                false,
            );
        }
        let mut wire_consumed = false;
        if let Some(hit) = hovered
            && let Some(edge) = doc.edges.iter().find(|e| e.id == connections[hit.wire].id)
        {
            response.clone().on_hover_text_at_pointer(format!(
                "{} → {}",
                endpoint_name(&doc, &catalog, &edge.from, true),
                endpoint_name(&doc, &catalog, &edge.to, false)
            ));
            if response.double_clicked() && edge.reroutes.len() < 32 {
                self.checkpoint();
                let edge = self
                    .document
                    .as_mut()
                    .expect("document")
                    .edges
                    .iter_mut()
                    .find(|e| e.id == edge.id)
                    .expect("edge");
                edge.reroutes
                    .insert(hit.segment, [hit.point.x, hit.point.y]);
                self.reroute_selection = Some((edge.id.clone(), hit.segment));
                self.selected.clear();
                wire_consumed = true;
            }
        }
        let mut drag = None;
        let mut checkpoint = false;
        let mut remove_nodes = BTreeSet::new();
        let mut remove_edges = BTreeSet::new();
        for node in &doc.nodes {
            let r = rectangles[&node.id];
            if !r.intersects(rect) {
                continue;
            }
            let ty = catalog.types.get(&node.type_id);
            let known = ty.is_some();
            let accent = node_accent(&node.type_id, ty);
            let accent = if node.enabled {
                accent
            } else {
                accent.lerp_to_gamma(crate::ui::theme::DISABLED, 0.85)
            };
            let base = crate::ui::theme::SURFACE;
            let body = ui.interact(
                r.intersect(rect),
                ui.id().with((&node.id, "body")),
                Sense::click(),
            );
            if body.clicked() {
                self.reroute_selection = None;
                self.selected = BTreeSet::from([node.id.clone()]);
            }
            painter.rect_filled(
                r,
                5.0,
                if node.enabled {
                    base.lerp_to_gamma(accent, 0.06)
                } else {
                    crate::ui::theme::BG
                },
            );
            painter.rect_stroke(
                r,
                5.0,
                Stroke::new(
                    if self.selected.contains(&node.id) {
                        2.0
                    } else {
                        1.0
                    },
                    if !known {
                        crate::ui::theme::RED
                    } else if self.selected.contains(&node.id) {
                        crate::ui::theme::TEXT
                    } else {
                        base.lerp_to_gamma(accent, 0.35)
                    },
                ),
                egui::StrokeKind::Inside,
            );
            let header = Rect::from_min_size(r.min, Vec2::new(r.width(), 30.0));
            painter.rect_filled(
                header,
                4.0,
                base.lerp_to_gamma(accent, if node.enabled { 0.42 } else { 0.16 }),
            );
            painter.hline(
                header.left() + 5.0..=header.right() - 5.0,
                header.top() + 1.0,
                Stroke::new(2.0, accent),
            );
            painter.with_clip_rect(header.intersect(rect)).text(
                header.left_center() + Vec2::new(9.0, 0.0),
                egui::Align2::LEFT_CENTER,
                node_name(node, &catalog),
                egui::FontId::proportional(crate::ui::theme::COMPACT_TEXT),
                if node.enabled {
                    Color32::WHITE
                } else {
                    crate::ui::theme::MUTED
                },
            );
            let nr = ui.interact(
                header.intersect(rect),
                ui.id().with((&node.id, "header")),
                Sense::click_and_drag(),
            );
            if nr.clicked() {
                if ui.input(|i| i.modifiers.ctrl) {
                    if !self.selected.insert(node.id.clone()) {
                        self.selected.remove(&node.id);
                    }
                } else {
                    self.selected = BTreeSet::from([node.id.clone()]);
                }
            }
            if nr.drag_started() {
                self.reroute_selection = None;
                if !self.selected.contains(&node.id) {
                    self.selected = BTreeSet::from([node.id.clone()]);
                }
                checkpoint = true;
            }
            if nr.dragged() {
                drag = Some(nr.drag_delta());
            }
            let mut edited = node.clone();
            let mut edit_id = None;
            nr.context_menu(|ui| {
                let enabled = ui.checkbox(&mut edited.enabled, "启用节点");
                if enabled.changed() {
                    edit_id = Some(enabled.id);
                }
                ui.separator();
                if ui.button("断开连线").clicked() {
                    remove_edges.extend(
                        doc.edges
                            .iter()
                            .filter(|e| e.from.node == node.id || e.to.node == node.id)
                            .map(|e| e.id.clone()),
                    );
                    ui.close();
                }
                if ui.button("删除节点").clicked() {
                    remove_nodes.insert(node.id.clone());
                    ui.close();
                }
            });
            let description = catalog
                .types
                .get(&node.type_id)
                .map(|t| t.definition.description.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("插件未提供此节点的用途说明。");
            let description_rect = Rect::from_min_max(
                r.min + Vec2::new(12.0, 37.0),
                Pos2::new(r.right() - 12.0, r.top() + 76.0),
            );
            let galley = painter.layout(
                description.into(),
                egui::FontId::proportional(crate::ui::theme::TINY),
                crate::ui::theme::MUTED,
                description_rect.width(),
            );
            painter
                .with_clip_rect(description_rect.intersect(rect))
                .galley(description_rect.min, galley, crate::ui::theme::MUTED);
            ui.interact(
                description_rect.intersect(rect),
                ui.id().with((&node.id, "description")),
                Sense::hover(),
            )
            .on_hover_text(description);
            if let Some(ty) = catalog.types.get(&node.type_id) {
                let keys = ty.fields.keys(&node.parameters);
                if !keys.is_empty() {
                    let top = r.top() + parameter_top(node, &catalog);
                    painter.hline(
                        r.left() + 12.0..=r.right() - 12.0,
                        top - 4.0,
                        Stroke::new(1.0, crate::ui::theme::LINE),
                    );
                    let mut fields = ui.new_child(
                        egui::UiBuilder::new()
                            .id_salt((&node.id, "parameters"))
                            .max_rect(Rect::from_min_max(
                                Pos2::new(r.left() + 12.0, top),
                                r.max - Vec2::new(12.0, 8.0),
                            ))
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                    );
                    fields.set_clip_rect(r.intersect(rect));
                    fields.spacing_mut().item_spacing = Vec2::ZERO;
                    crate::ui::controls::configure(
                        fields.style_mut(),
                        crate::ui::controls::COMPACT_HEIGHT,
                    );
                    edited.parameters = ty.fields.with_defaults(&edited.parameters);
                    for key in keys {
                        if let Some(id) = ty.fields.row(&mut fields, &key, &mut edited.parameters) {
                            edit_id = Some(id);
                            self.selected = BTreeSet::from([node.id.clone()]);
                        }
                    }
                }
            }
            if node.type_id == OVERLAY {
                let mut layers = doc
                    .edges
                    .iter()
                    .filter(|e| e.to.node == node.id)
                    .cloned()
                    .collect::<Vec<_>>();
                layers.sort_by_key(|e| (std::cmp::Reverse(e.order), e.id.clone()));
                let mut rows = ui.new_child(
                    egui::UiBuilder::new()
                        .id_salt((&node.id, "layer-order"))
                        .max_rect(Rect::from_min_max(
                            Pos2::new(r.left() + 12.0, r.top() + parameter_top(node, &catalog)),
                            r.max - Vec2::splat(10.0),
                        ))
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                crate::ui::controls::configure(
                    rows.style_mut(),
                    crate::ui::controls::COMPACT_HEIGHT,
                );
                rows.set_clip_rect(r.intersect(rect));
                rows.spacing_mut().item_spacing = Vec2::new(4.0, 4.0);
                rows.weak("图层顺序 · 上层在前");
                let mut swap = None;
                for (i, edge) in layers.iter().enumerate() {
                    rows.push_id(&edge.id, |ui| {
                        ui.horizontal(|ui| {
                            let name = doc
                                .nodes
                                .iter()
                                .find(|n| n.id == edge.from.node)
                                .map_or("未知节点", |n| node_name(n, &catalog));
                            ui.add_sized([190.0, 26.0], egui::Label::new(name).truncate());
                            if ui
                                .add_enabled(i > 0, egui::Button::new("↑"))
                                .on_hover_text("上移一层")
                                .clicked()
                            {
                                swap = Some((i, i - 1));
                            }
                            if ui
                                .add_enabled(i + 1 < layers.len(), egui::Button::new("↓"))
                                .on_hover_text("下移一层")
                                .clicked()
                            {
                                swap = Some((i, i + 1));
                            }
                            if crate::ui::controls::close_button(
                                ui,
                                "断开图层",
                                crate::ui::controls::COMPACT_HEIGHT,
                            )
                            .clicked()
                            {
                                remove_edges.insert(edge.id.clone());
                            }
                        })
                    });
                }
                if let Some((a, b)) = swap {
                    layers.swap(a, b);
                    self.checkpoint();
                    for (i, layer) in layers.iter().enumerate() {
                        if let Some(edge) = self
                            .document
                            .as_mut()
                            .expect("document")
                            .edges
                            .iter_mut()
                            .find(|e| e.id == layer.id)
                        {
                            edge.order = (layers.len() - 1 - i) as u32;
                        }
                    }
                }
            }
            if let Some(id) = edit_id {
                if self.parameter_edit != Some(id) {
                    self.checkpoint();
                    self.parameter_edit = Some(id);
                }
                if let Some(target) = self
                    .document
                    .as_mut()
                    .and_then(|d| d.nodes.iter_mut().find(|n| n.id == node.id))
                {
                    target.name = node_name(node, &catalog).to_owned();
                    target.enabled = edited.enabled;
                    target.parameters = edited.parameters;
                }
            }
            for (output, list) in [
                (false, catalog.ports(node, false)),
                (true, catalog.ports(node, true)),
            ] {
                for (index, port) in list.iter().enumerate() {
                    let pos = Pos2::new(
                        if output {
                            r.right() - 10.0
                        } else {
                            r.left() + 10.0
                        },
                        r.top() + (PORT_TOP + index as f32 * 24.0),
                    );
                    painter.text(
                        pos,
                        if output {
                            egui::Align2::RIGHT_CENTER
                        } else {
                            egui::Align2::LEFT_CENTER
                        },
                        &port.name,
                        egui::FontId::proportional(crate::ui::theme::SMALL),
                        crate::ui::theme::TEXT,
                    );
                }
            }
        }
        if checkpoint {
            self.checkpoint();
        }
        if let Some(delta) = drag {
            for node in &mut self.document.as_mut().expect("document").nodes {
                if self.selected.contains(&node.id) {
                    node.position[0] += delta.x;
                    node.position[1] += delta.y;
                }
            }
        }
        if let Some(delta) = drag {
            for edge in &mut self.document.as_mut().expect("document").edges {
                if self.selected.contains(&edge.from.node) && self.selected.contains(&edge.to.node)
                {
                    for p in &mut edge.reroutes {
                        p[0] += delta.x;
                        p[1] += delta.y;
                    }
                }
            }
        }
        if let Some(index) = focused {
            self.wire_routes
                .paint(&painter, index, wire_colors[index], true);
            if let Some(edge) = doc.edges.iter().find(|e| e.id == connections[index].id) {
                for id in [&edge.from.node, &edge.to.node] {
                    painter.rect_stroke(
                        rectangles[id].expand(2.),
                        5.,
                        Stroke::new(1.5, wire_colors[index]),
                        egui::StrokeKind::Outside,
                    );
                }
                for pos in [connections[index].from, connections[index].to] {
                    painter.circle_stroke(pos, 7., Stroke::new(1.5, wire_colors[index]));
                }
            }
        }
        let mut remove_knot = None;
        for edge in &doc.edges {
            let wire_color = connections
                .iter()
                .position(|c| c.id == edge.id)
                .map_or(Color32::GRAY, |i| wire_colors[i]);
            for (index, point) in edge.reroutes.iter().take(32).enumerate() {
                let pos = Pos2::new(point[0], point[1]);
                if !rect.contains(pos) {
                    continue;
                }
                let key = (edge.id.clone(), index);
                let response = ui.interact(
                    Rect::from_center_size(pos, Vec2::splat(16. * inverse.scaling)),
                    ui.id().with(("reroute", &edge.id, index)),
                    Sense::click_and_drag(),
                );
                painter.circle_filled(
                    pos,
                    4. * inverse.scaling,
                    if self.reroute_selection.as_ref() == Some(&key) {
                        Color32::WHITE
                    } else {
                        wire_color
                    },
                );
                if response.clicked() || response.drag_started() {
                    self.reroute_selection = Some(key.clone());
                    self.selected.clear();
                    wire_consumed = true;
                }
                if response.drag_started() {
                    self.checkpoint();
                }
                if response.dragged() {
                    let delta = response.drag_delta();
                    if let Some(p) = self
                        .document
                        .as_mut()
                        .expect("document")
                        .edges
                        .iter_mut()
                        .find(|e| e.id == edge.id)
                        .and_then(|e| e.reroutes.get_mut(index))
                    {
                        p[0] += delta.x;
                        p[1] += delta.y;
                    }
                    wire_consumed = true;
                }
                response
                    .on_hover_text("拖动调整连线；右键删除")
                    .context_menu(|ui| {
                        if ui.button("删除中继点").clicked() {
                            remove_knot = Some(key);
                            ui.close();
                        }
                    });
            }
        }
        if let Some((id, index)) = remove_knot {
            self.remove_reroute(&id, index);
            wire_consumed = true;
        }
        let mut connect = None;
        for (port, output, pos, kind) in &ports {
            if !rect.contains(*pos) {
                continue;
            }
            painter.circle_filled(*pos, 4.5, color(kind));
            let pr = ui.interact(
                Rect::from_center_size(*pos, Vec2::splat(16.0)),
                ui.id().with((&port.node, &port.port, *output)),
                Sense::click_and_drag(),
            );
            if pr.drag_started() {
                self.wire = Some((port.clone(), *output));
            }
            if pr.clicked() {
                match &self.wire {
                    Some((a, o)) if *o != *output => {
                        connect = Some(if *o {
                            (a.clone(), port.clone())
                        } else {
                            (port.clone(), a.clone())
                        });
                    }
                    _ => self.wire = Some((port.clone(), *output)),
                }
            }
            let describe =
                |endpoint: &Endpoint, output: bool| endpoint_name(&doc, &catalog, endpoint, output);
            let mut hint = vec![describe(port, *output)];
            for edge in &doc.edges {
                if (*output && edge.from == *port) || (!*output && edge.to == *port) {
                    hint.push(format!(
                        "{} → {}",
                        describe(&edge.from, true),
                        describe(&edge.to, false)
                    ));
                }
            }
            pr.clone().on_hover_text(hint.join("\n"));
            pr.context_menu(|ui| {
                if ui.button("断开连线").clicked() {
                    remove_edges.extend(
                        doc.edges
                            .iter()
                            .filter(|e| {
                                if *output {
                                    e.from == *port
                                } else {
                                    e.to == *port
                                }
                            })
                            .map(|e| e.id.clone()),
                    );
                    ui.close();
                }
            });
        }
        if !remove_nodes.is_empty() || !remove_edges.is_empty() {
            self.checkpoint();
            let document = self.document.as_mut().expect("document");
            document.nodes.retain(|n| !remove_nodes.contains(&n.id));
            document.edges.retain(|e| {
                !remove_edges.contains(&e.id)
                    && !remove_nodes.contains(&e.from.node)
                    && !remove_nodes.contains(&e.to.node)
            });
            self.selected.retain(|id| !remove_nodes.contains(id));
            self.wire = None;
            self.reroute_selection = None;
        }
        if let Some((start, output)) = &self.wire
            && let Some((_, _, pos, kind)) =
                ports.iter().find(|(p, o, _, _)| p == start && o == output)
            && let Some(pointer) = pointer
        {
            draw_wire(
                &painter,
                if *output { *pos } else { pointer },
                if *output { pointer } else { *pos },
                color(kind),
            );
            if ui.input(|i| i.pointer.any_released())
                && let Some((end, _, _, _)) = ports
                    .iter()
                    .find(|(_, o, p, _)| o != output && p.distance(pointer) < 12.0)
            {
                connect = Some(if *output {
                    (start.clone(), end.clone())
                } else {
                    (end.clone(), start.clone())
                });
            }
        }
        if let Some((from, to)) = connect {
            self.checkpoint();
            let result = self
                .document
                .as_mut()
                .expect("document")
                .connect(&catalog, from, to);
            self.message = result.err().map_or(String::new(), |e| e.to_string());
            self.wire = None;
            self.reroute_selection = None;
        }
        if response.drag_started_by(egui::PointerButton::Primary)
            && !over_node
            && !wire_consumed
            && near_knot.is_none()
        {
            self.selection_start = pointer;
            self.wire = None;
            self.reroute_selection = None;
        }
        if let (Some(start), Some(end)) = (self.selection_start, pointer) {
            let selection = Rect::from_two_pos(start, end);
            painter.rect_stroke(
                selection,
                0.0,
                Stroke::new(1.0, crate::ui::theme::ACCENT),
                egui::StrokeKind::Inside,
            );
            if ui.input(|i| i.pointer.any_released()) {
                self.selected = rectangles
                    .iter()
                    .filter(|(_, r)| selection.intersects(**r))
                    .map(|(id, _)| id.clone())
                    .collect();
                self.selection_start = None;
            }
        }
        if response.clicked()
            && !over_node
            && !wire_consumed
            && hovered.is_none()
            && near_knot.is_none()
        {
            self.selected.clear();
            self.wire = None;
            self.reroute_selection = None;
        }
        if rect.contains(pointer.unwrap_or(Pos2::new(-1.0, -1.0)))
            && !ui.ctx().egui_wants_keyboard_input()
            && !super::parameters::capturing(ui.ctx())
        {
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                self.wire = None;
                self.reroute_selection = None;
                self.selection_start = None;
            }
            if ui.input(|i| i.key_pressed(egui::Key::Delete)) {
                if let Some((id, index)) = self.reroute_selection.clone() {
                    self.remove_reroute(&id, index);
                } else {
                    self.delete_selected();
                }
            }
            if ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Z)) {
                if ui.input(|i| i.modifiers.shift) {
                    self.redo();
                } else {
                    self.undo();
                }
            }
            if ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::C)) {
                self.clipboard = Some((
                    doc.nodes
                        .iter()
                        .filter(|n| self.selected.contains(&n.id))
                        .cloned()
                        .collect(),
                    doc.edges
                        .iter()
                        .filter(|e| {
                            self.selected.contains(&e.from.node)
                                && self.selected.contains(&e.to.node)
                        })
                        .cloned()
                        .collect(),
                ));
            }
            if ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::V)) {
                self.paste();
            }
        }
        if !ui.ctx().egui_wants_keyboard_input() && !ui.input(|i| i.pointer.primary_down()) {
            self.parameter_edit = None;
        }
    }
    fn remove_reroute(&mut self, id: &str, index: usize) {
        if !self.document.as_ref().is_some_and(|d| {
            d.edges
                .iter()
                .any(|e| e.id == id && index < e.reroutes.len())
        }) {
            return;
        }
        self.checkpoint();
        if let Some(edge) = self
            .document
            .as_mut()
            .expect("document")
            .edges
            .iter_mut()
            .find(|e| e.id == id)
        {
            edge.reroutes.remove(index);
        }
        self.reroute_selection = None;
    }
    fn paste(&mut self) {
        let Some((mut nodes, mut edges)) = self.clipboard.clone() else {
            return;
        };
        if self
            .document
            .as_ref()
            .is_none_or(|d| d.nodes.len() + nodes.len() > 64 || d.edges.len() + edges.len() > 256)
        {
            self.message = "粘贴后会超出图规模限制".into();
            return;
        }
        self.checkpoint();
        let mut ids = BTreeMap::new();
        for node in &mut nodes {
            let next = uuid::Uuid::new_v4().to_string();
            ids.insert(node.id.clone(), next.clone());
            node.id = next;
            node.position[0] += 24.0;
            node.position[1] += 24.0;
        }
        for edge in &mut edges {
            edge.id = uuid::Uuid::new_v4().to_string();
            edge.from.node = ids[&edge.from.node].clone();
            edge.to.node = ids[&edge.to.node].clone();
            for p in &mut edge.reroutes {
                p[0] += 24.;
                p[1] += 24.;
            }
        }
        self.selected = nodes.iter().map(|n| n.id.clone()).collect();
        let doc = self.document.as_mut().expect("document");
        doc.nodes.extend(nodes);
        doc.edges.extend(edges);
    }
}
fn draw_wire(p: &egui::Painter, a: Pos2, b: Pos2, color: Color32) {
    let dx = ((b.x - a.x).abs() * 0.5).max(35.0);
    p.add(egui::epaint::CubicBezierShape::from_points_stroke(
        [a, a + Vec2::new(dx, 0.0), b - Vec2::new(dx, 0.0), b],
        false,
        Color32::TRANSPARENT,
        Stroke::new(2.0, color),
    ));
}

fn endpoint_name(doc: &Document, catalog: &Catalog, endpoint: &Endpoint, output: bool) -> String {
    doc.nodes
        .iter()
        .find(|n| n.id == endpoint.node)
        .map(|node| {
            let name = node_name(node, catalog);
            let port = catalog
                .ports(node, output)
                .iter()
                .find(|p| p.id == endpoint.port)
                .map_or(endpoint.port.as_str(), |p| p.name.as_str());
            format!("{name} · {port}")
        })
        .unwrap_or_default()
}
