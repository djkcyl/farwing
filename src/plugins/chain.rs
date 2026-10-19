use super::graph::{Compiled, Document};
use super::process::lock;
use super::video::{ChainShared, Loader, Tap, VideoNode};
use super::*;
use std::sync::{Arc, atomic::Ordering};

struct Analysis {
    id: u64,
    name: String,
    source: u64,
    sample_frames: bool,
    controller: super::ui::AnalysisController,
}
struct Active {
    plan: Compiled,
    analyses: Vec<Analysis>,
    revision: u64,
}
struct Pending {
    plan: Compiled,
    analyses: Vec<Analysis>,
    loader: Option<Loader>,
    programs: Option<Vec<VideoNode>>,
    revision: Option<u64>,
}
pub(crate) struct Controller {
    pub shared: Arc<ChainShared>,
    context: egui::Context,
    saved: Vec<Document>,
    selected: Option<String>,
    watcher: Option<super::watch::Watcher>,
    active: Option<Active>,
    pending: Option<Pending>,
    latest: Option<Compiled>,
    last_view: Option<sdk::Viewport>,
    enabled: bool,
    error: Option<String>,
    input: Option<crate::features::remote_input::RemoteInput>,
    leases: std::collections::BTreeMap<u64, super::input::Lease>,
    input_owner: Option<u64>,
    input_allowed: bool,
    registration: Option<(u64, u64, bool)>,
}
impl Controller {
    pub fn new(context: egui::Context) -> Self {
        Self {
            shared: ChainShared::new(),
            context,
            saved: super::graph::list().unwrap_or_default(),
            selected: None,
            watcher: None,
            active: None,
            pending: None,
            latest: None,
            last_view: None,
            enabled: false,
            error: None,
            input: None,
            leases: std::collections::BTreeMap::new(),
            input_owner: None,
            input_allowed: false,
            registration: None,
        }
    }
    fn stop(&mut self) {
        self.disarm();
        self.watcher = None;
        self.pending = None;
        self.active = None;
        *lock(&self.shared.taps) = Vec::new();
        self.shared.publish(Vec::new(), 0);
        self.enabled = false;
    }
    fn watch(&mut self) {
        let Some(id) = &self.selected else {
            return;
        };
        match super::watch::Watcher::start(id.clone(), self.context.clone()) {
            Ok(watcher) => {
                self.watcher = Some(watcher);
                self.enabled = true;
                self.error = None;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
    }
    fn prepare(&mut self, plan: Compiled) {
        self.disarm();
        self.pending = None;
        self.latest = Some(plan.clone());
        self.error = None;
        let mut analyses = Vec::new();
        let mut gates = std::collections::BTreeMap::new();
        for spec in &plan.analyses {
            let name = plan
                .document
                .nodes
                .get((spec.instance.id - 1) as usize)
                .map_or("分析", |n| n.name.as_str())
                .to_owned();
            let mut controller =
                super::ui::AnalysisController::new(self.context.clone(), spec.clone());
            *lock(&controller.shared.view) = self.last_view;
            *lock(&controller.shared.physical) = self.input.clone();
            for control in &spec.controls {
                let conditions = control
                    .conditions
                    .iter()
                    .map(|condition| {
                        let gate = condition.hotkey.as_ref().map(|hotkey| {
                            gates
                                .entry(hotkey.id)
                                .or_insert_with(|| super::hotkeys::Gate::new(hotkey.clone()))
                                .clone()
                        });
                        super::hotkeys::Condition {
                            port: condition.port.clone(),
                            name: condition.name.clone(),
                            gate,
                        }
                    })
                    .collect();
                lock(&controller.shared.signals).insert(
                    control.instance.id,
                    super::hotkeys::ControlGate::new(conditions),
                );
            }
            controller.start();
            analyses.push(Analysis {
                id: spec.instance.id,
                name,
                source: spec.source,
                sample_frames: spec.sample_frames,
                controller,
            });
        }
        let loader = if plan.videos.is_empty() {
            None
        } else {
            match Loader::start(plan.videos.clone(), self.context.clone()) {
                Ok(loader) => Some(loader),
                Err(e) => {
                    self.error = Some(format!("准备失败，当前图保持运行：{e:#}"));
                    return;
                }
            }
        };
        let programs = if plan.videos.is_empty() {
            Some(Vec::new())
        } else {
            None
        };
        self.pending = Some(Pending {
            plan,
            analyses,
            loader,
            programs,
            revision: None,
        });
    }
    fn advance(&mut self) {
        let mut failure = None;
        let mut commit = false;
        if let Some(pending) = &mut self.pending {
            if let Some(result) = pending.loader.as_ref().and_then(Loader::poll) {
                pending.loader = None;
                match result {
                    Ok(programs) => pending.programs = Some(programs),
                    Err(e) => failure = Some(format!("视频节点准备失败：{e:#}")),
                }
            }
            for analysis in &pending.analyses {
                if !analysis.controller.shared.enabled.load(Ordering::Acquire) {
                    failure = Some(format!(
                        "{}：{}",
                        analysis.name,
                        lock(&analysis.controller.shared.display).status
                    ));
                    break;
                }
            }
            if let Some(revision) = pending.revision {
                if self.shared.failed_revision.load(Ordering::Acquire) == revision {
                    failure = Some(
                        lock(&self.shared.error)
                            .clone()
                            .unwrap_or_else(|| "GPU准备失败".into()),
                    );
                } else if self.shared.applied_revision.load(Ordering::Acquire) == revision {
                    commit = true;
                }
            } else if failure.is_none()
                && pending.programs.is_some()
                && pending
                    .analyses
                    .iter()
                    .all(|a| a.controller.shared.ready.load(Ordering::Acquire))
            {
                pending.revision = Some(self.shared.publish(
                    pending.programs.take().expect("prepared"),
                    pending.plan.output_source,
                ));
            }
        }
        if let Some(error) = failure {
            self.pending = None;
            self.error = Some(format!("应用失败，当前图未替换：{error}"));
        } else if commit {
            let pending = self.pending.take().expect("pending graph");
            let revision = pending.revision.expect("submitted graph");
            self.active = None;
            *lock(&self.shared.taps) = pending
                .analyses
                .iter()
                .filter(|a| a.sample_frames)
                .map(|a| Tap {
                    id: a.id,
                    revision,
                    source: a.source,
                    state: a.controller.shared.clone(),
                })
                .collect();
            self.shared.generated.store(0, Ordering::Release);
            self.shared.skipped.store(0, Ordering::Release);
            self.active = Some(Active {
                plan: pending.plan,
                analyses: pending.analyses,
                revision,
            });
            self.error = None;
        }
        if let Some(active) = &mut self.active
            && self.shared.video_failed.load(Ordering::Acquire) == active.revision
        {
            for analysis in &mut active.analyses {
                if analysis.source != 0
                    && analysis.controller.shared.enabled.load(Ordering::Acquire)
                {
                    analysis.controller.stop();
                }
            }
        }
    }
    fn update(&mut self) {
        self.leases.retain(|_, l| l.active());
        if let (Some(input), Some(owner), Some(active)) =
            (&self.input, self.input_owner, &self.active)
        {
            for spec in &active.plan.analyses {
                for control in spec.controls.iter().filter(|c| c.send_input) {
                    if self.leases.contains_key(&control.instance.id) {
                        continue;
                    }
                    if let Some(analysis) =
                        active.analyses.iter().find(|a| a.id == spec.instance.id)
                        && analysis.controller.shared.ready.load(Ordering::Acquire)
                        && let Some(gate) = lock(&analysis.controller.shared.signals)
                            .get(&control.instance.id)
                            .cloned()
                    {
                        match super::input::Lease::start(
                            input.clone(),
                            owner,
                            analysis.controller.shared.clone(),
                            gate,
                            control.instance.id,
                        ) {
                            Ok(lease) => {
                                self.leases.insert(control.instance.id, lease);
                            }
                            Err(e) => self.error = Some(e.to_string()),
                        }
                    }
                }
            }
        }
        self.advance();
        if let Some(update) = self.watcher.as_ref().and_then(super::watch::Watcher::poll) {
            let _publication_revision = update.revision;
            match update.result {
                Ok(plan) => self.prepare(plan),
                Err(error) => self.error = Some(error),
            }
        }
        self.advance();
        if self.pending.is_some() {
            self.context
                .request_repaint_after(std::time::Duration::from_millis(30));
        }
    }
    pub fn window(
        &mut self,
        ctx: &egui::Context,
        open: &mut bool,
        style: fn(&mut egui::Ui),
        anchor_offset: egui::Vec2,
    ) {
        self.update();
        if !*open {
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            *open = false;
            return;
        }
        let mut close = false;
        egui::Window::new("节点图")
            .id(egui::Id::new("viewer-plugin-menu"))
            .open(open)
            .title_bar(false)
            .resizable(false)
            .auto_sized()
            .anchor(egui::Align2::RIGHT_TOP, anchor_offset)
            .frame(
                egui::Frame::new()
                    .fill(crate::ui::theme::BG)
                    .stroke(egui::Stroke::new(1.0, crate::ui::theme::LINE))
                    .corner_radius(crate::ui::theme::PANEL_RADIUS)
                    .inner_margin(12),
            )
            .show(ctx, |ui| {
                style(ui);
                ui.set_width(320.0);
                ui.horizontal(|ui| {
                    ui.strong("节点图");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        close = crate::ui::controls::close_button(
                            ui,
                            "关闭",
                            crate::ui::controls::COMPACT_HEIGHT,
                        )
                        .clicked();
                    });
                });
                egui::ScrollArea::vertical()
                    .max_height((ctx.content_rect().height() - 120.0).clamp(100.0, 530.0))
                    .show(ui, |ui| self.menu(ui));
            });
        if close {
            *open = false;
        }
    }
    fn menu(&mut self, ui: &mut egui::Ui) {
        let before = self.selected.clone();
        egui::ComboBox::from_id_salt("graph-preset")
            .width(ui.available_width())
            .selected_text(
                self.saved
                    .iter()
                    .find(|d| Some(&d.graph_id) == self.selected.as_ref())
                    .map_or("选择节点图", |d| d.name.as_str()),
            )
            .show_ui(ui, |ui| {
                for doc in &self.saved {
                    ui.selectable_value(&mut self.selected, Some(doc.graph_id.clone()), &doc.name);
                }
            });
        if self.selected != before && self.enabled {
            self.watcher = None;
            self.watch();
        }
        let mut enabled = self.enabled;
        if ui
            .add_enabled(
                self.selected.is_some(),
                egui::Checkbox::new(&mut enabled, "启用节点图"),
            )
            .changed()
        {
            if enabled {
                self.watch();
            } else {
                self.stop();
            }
        }
        if let Some(active) = &mut self.active {
            ui.weak(format!("运行：{}", active.plan.document.name));
            for analysis in &mut active.analyses {
                ui.push_id(analysis.id, |ui| {
                    egui::CollapsingHeader::new(&analysis.name)
                        .default_open(true)
                        .show(ui, |ui| analysis.controller.menu(ui));
                });
            }
            crate::ui::controls::observe_notice(
                ui.ctx(),
                "plugin-video-bypass",
                "视频节点异常",
                crate::ui::controls::DialogIcon::Error,
                (self.shared.video_failed.load(Ordering::Acquire) == active.revision)
                    .then_some("视频节点已旁路"),
            );
            let generated = self.shared.generated.load(Ordering::Relaxed);
            if generated > 0 {
                ui.weak(format!(
                    "生成帧 {generated} · 跳过 {}",
                    self.shared.skipped.load(Ordering::Relaxed)
                ));
            }
        }
        crate::ui::controls::progress_notice(
            ui.ctx(),
            "plugin-prepare",
            "准备节点图",
            self.pending.is_some().then_some("正在准备新图…"),
        );
        crate::ui::controls::observe_notice(
            ui.ctx(),
            "plugin-runtime",
            "节点图运行失败",
            crate::ui::controls::DialogIcon::Error,
            self.error.as_deref(),
        );
        ui.horizontal(|ui| {
            if ui.button("刷新列表").clicked() {
                match super::graph::list() {
                    Ok(saved) => self.saved = saved,
                    Err(e) => self.error = Some(e.to_string()),
                }
            }
            if ui
                .add_enabled(
                    self.enabled && self.latest.is_some(),
                    egui::Button::new("重新应用"),
                )
                .clicked()
                && let Some(plan) = self.latest.clone()
            {
                self.prepare(plan);
            }
            if ui.button("图文件夹").clicked()
                && let Err(e) = super::graph::directory().and_then(|p| open_folder(&p))
            {
                self.error = Some(e.to_string());
            }
        });
        if self.saved.is_empty() {
            ui.weak("在控制中心的插件管理中创建并应用节点图。");
        }
    }
    pub fn paint(&mut self, ctx: &egui::Context, width: f32, height: f32, video: [f32; 4]) {
        self.last_view = Some(sdk::Viewport {
            width,
            height,
            scale: ctx.pixels_per_point(),
            video,
            revision: 1,
        });
        self.update();
        let mut output = Vec::new();
        if let Some(active) = &mut self.active {
            for analysis in &mut active.analyses {
                analysis
                    .controller
                    .paint(ctx, width, height, video, &mut output);
            }
        }
        self.status_overlay(ctx);
        output.sort_by_key(|o| o.order);
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Background,
            egui::Id::new("plugin-composite"),
        ));
        for item in output {
            painter
                .with_clip_rect(item.clip)
                .add(egui::Shape::mesh(item.mesh));
        }
    }
    pub fn disarm(&mut self) {
        self.registration = None;
        if let Some(owner) = self.input_owner {
            super::hotkeys::unregister(owner);
        }
        self.leases.clear();
        if let Some(active) = &self.active {
            for a in &active.analyses {
                for gate in lock(&a.controller.shared.signals).values() {
                    gate.reset();
                }
            }
        }
    }
    pub fn input_context(
        &mut self,
        input: crate::features::remote_input::RemoteInput,
        owner: u64,
        allowed: bool,
    ) {
        self.input_allowed = allowed
            && input.mode() != crate::features::remote_input::MouseMode::View
            && input.relative_mode()
            && self.pending.is_none();
        self.input = Some(input.clone());
        self.input_owner = Some(owner);
        if let Some(active) = &self.active {
            for spec in active
                .plan
                .analyses
                .iter()
                .filter(|s| s.controls.iter().any(|c| c.send_input))
            {
                self.input_allowed &= active
                    .analyses
                    .iter()
                    .find(|a| a.id == spec.instance.id)
                    .is_some_and(|a| a.controller.shared.ready.load(Ordering::Acquire));
            }
        }
        let registration = (
            owner,
            self.active.as_ref().map_or(0, |a| a.revision),
            self.input_allowed,
        );
        if self.registration == Some(registration) {
            return;
        }
        self.registration = Some(registration);
        let mut gates = std::collections::BTreeMap::new();
        let mut controls = Vec::new();
        if let Some(active) = &self.active {
            for a in &active.analyses {
                *lock(&a.controller.shared.physical) = Some(input.clone());
                for control in lock(&a.controller.shared.signals).values() {
                    for condition in &control.conditions {
                        if let Some(gate) = &condition.gate {
                            gates.insert(gate.spec.id, gate.clone());
                        }
                    }
                    controls.push(control.clone());
                }
            }
        }
        super::hotkeys::register(
            owner,
            gates.into_values().collect(),
            controls,
            self.input_allowed,
        );
    }
    fn status_overlay(&self, ctx: &egui::Context) {
        let Some(active) = &self.active else {
            return;
        };
        let mut rows = Vec::new();
        for spec in &active.plan.analyses {
            for c in spec.controls.iter().filter(|c| c.send_input) {
                let gate = active
                    .analyses
                    .iter()
                    .find(|a| a.id == spec.instance.id)
                    .and_then(|a| {
                        lock(&a.controller.shared.signals)
                            .get(&c.instance.id)
                            .cloned()
                    });
                let on = gate.as_ref().is_some_and(|g| g.snapshot().0) && self.input_allowed;
                let key = gate
                    .as_ref()
                    .map_or_else(|| "未绑定快捷键".into(), |g| g.binding_text());
                let status = if on {
                    "生效中"
                } else if self.input_allowed && gate.as_ref().is_some_and(|g| g.suspended()) {
                    "已暂停"
                } else if self.input_allowed && gate.as_ref().is_some_and(|g| g.master_enabled()) {
                    "已开启 · 待触发"
                } else {
                    "已关闭"
                };
                let name = active
                    .plan
                    .document
                    .nodes
                    .get((c.instance.id - 1) as usize)
                    .map_or("控制节点", |n| n.name.as_str());
                rows.push((name.to_owned(), key, on, status));
            }
        }
        if rows.is_empty() {
            return;
        }
        egui::Area::new(egui::Id::new("plugin-control-status"))
            .default_pos(egui::pos2(14.0, 60.0))
            .order(egui::Order::Foreground)
            .movable(true)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_black_alpha(190))
                    .corner_radius(5.0)
                    .inner_margin(8)
                    .show(ui, |ui| {
                        for (name, key, on, status) in rows {
                            ui.horizontal(|ui| {
                                ui.label(name);
                                ui.colored_label(
                                    if on {
                                        egui::Color32::LIGHT_GREEN
                                    } else {
                                        egui::Color32::GRAY
                                    },
                                    status,
                                );
                                ui.weak(key);
                            });
                            if !self.input_allowed {
                                ui.small("等待前台相对鼠标控制");
                            }
                        }
                    });
            });
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.stop();
    }
}
