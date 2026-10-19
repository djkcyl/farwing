use super::process::{Runtime, Shared, lock};
use super::*;
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

pub(crate) struct AnalysisController {
    pub shared: Arc<Shared>,
    spec: graph::AnalysisSpec,
    runtime: Option<Runtime>,
    textures: BTreeMap<u64, egui::TextureHandle>,
    layers: BTreeMap<String, bool>,
    packet: Option<Reply>,
    sampled_at: std::time::Instant,
    view: Option<sdk::Viewport>,
}
impl AnalysisController {
    pub fn new(ctx: egui::Context, spec: graph::AnalysisSpec) -> Self {
        let shared = Shared::new(ctx);
        shared.sample_fps.store(
            spec.instance
                .config
                .get("sample_fps")
                .and_then(|v| v.as_u64())
                .unwrap_or(10)
                .max(1),
            Ordering::Release,
        );
        Self {
            shared,
            spec,
            runtime: None,
            textures: BTreeMap::new(),
            layers: BTreeMap::new(),
            packet: None,
            sampled_at: std::time::Instant::now(),
            view: None,
        }
    }
    pub fn stop(&mut self) {
        self.shared.enabled.store(false, Ordering::Release);
        self.shared.ready.store(false, Ordering::Release);
        self.runtime.take();
        lock(&self.shared.sample).take();
        let mut display = lock(&self.shared.display);
        display.reply = None;
        display.uploads.clear();
        drop(display);
        self.packet = None;
        self.textures.clear();
    }
    pub fn start(&mut self) {
        self.stop();
        self.layers.clear();
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        self.shared.enabled.store(true, Ordering::Release);
        lock(&self.shared.display).status = "正在加载…".into();
        match Runtime::start(self.spec.clone(), self.shared.clone()) {
            Ok(runtime) => self.runtime = Some(runtime),
            Err(error) => self.shared.fail(format!("{error:#}")),
        }
    }
    pub fn menu(&mut self, ui: &mut egui::Ui) {
        let enabled = self.shared.enabled.load(Ordering::Acquire);
        if let Some(packet) = &self.packet {
            for layer in &packet.rendered.layers {
                let enabled = self
                    .layers
                    .entry(layer.id.clone())
                    .or_insert(layer.default_enabled);
                ui.checkbox(enabled, &layer.name);
            }
        }
        let status = lock(&self.shared.display).status.clone();
        if !status.is_empty() {
            if enabled {
                ui.label(status);
            } else {
                ui.label("插件已停止").on_hover_text(status);
            }
        }
    }

    /// Only imports validated textures/triangles. Shapes, text and HUD semantics
    /// are implemented by the external overlay provider.
    pub fn paint(
        &mut self,
        ctx: &egui::Context,
        width: f32,
        height: f32,
        video: [f32; 4],
        output: &mut Vec<Composite>,
    ) {
        if !self.shared.enabled.load(Ordering::Acquire) {
            self.packet = None;
            return;
        }
        let mut view = sdk::Viewport {
            width,
            height,
            scale: ctx.pixels_per_point(),
            video,
            revision: self.view.map_or(1, |v| v.revision),
        };
        if self.view.is_none_or(|v| v != view) {
            view.revision = view.revision.wrapping_add(1);
            self.view = Some(view);
            *lock(&self.shared.view) = Some(view);
        }
        let (reply, uploads, at) = {
            let mut display = lock(&self.shared.display);
            (
                display.reply.take(),
                std::mem::take(&mut display.uploads),
                display.at,
            )
        };
        if let Err(e) = self.upload(ctx, uploads) {
            self.shared.fail(e.to_string());
            return;
        }
        if let Some(reply) = reply {
            self.packet = Some(reply);
            self.sampled_at = at;
        }
        let Some(packet) = &self.packet else {
            return;
        };
        if packet.revision != view.revision
            || packet.generation != self.shared.generation.load(Ordering::Acquire)
        {
            return;
        }
        let video = egui::Rect::from_min_max(
            egui::pos2(video[0], video[1]),
            egui::pos2(video[2], video[3]),
        );
        let age = self.sampled_at.elapsed();
        for layer in &packet.rendered.layers {
            if !*self
                .layers
                .entry(layer.id.clone())
                .or_insert(layer.default_enabled)
                || (!layer.persistent && age > Duration::from_millis(400))
            {
                continue;
            }
            for source in &layer.meshes {
                let Some(texture) = self.textures.get(&source.texture) else {
                    continue;
                };
                let clip = egui::Rect::from_min_max(
                    egui::pos2(source.clip[0], source.clip[1]),
                    egui::pos2(source.clip[2], source.clip[3]),
                )
                .intersect(video);
                let mut mesh = egui::Mesh::with_texture(texture.id());
                mesh.indices = source.indices.clone();
                mesh.vertices = source
                    .vertices
                    .iter()
                    .map(|v| egui::epaint::Vertex {
                        pos: egui::pos2(v.position[0], v.position[1]),
                        uv: egui::pos2(v.uv[0], v.uv[1]),
                        color: egui::Color32::from_rgba_premultiplied(
                            v.color[0], v.color[1], v.color[2], v.color[3],
                        ),
                    })
                    .collect();
                output.push(Composite {
                    order: layer.composite_order,
                    clip,
                    mesh,
                });
            }
        }
        if age < Duration::from_millis(400) {
            ctx.request_repaint_after(Duration::from_millis(400).saturating_sub(age));
        }
    }
    fn upload(&mut self, ctx: &egui::Context, uploads: Vec<sdk::TextureUpload>) -> Result<()> {
        for upload in uploads {
            let image = egui::ColorImage::new(
                upload.size,
                upload
                    .pixels
                    .iter()
                    .map(|p| egui::Color32::from_rgba_premultiplied(p[0], p[1], p[2], p[3]))
                    .collect(),
            );
            if let Some(position) = upload.position {
                let texture = self.textures.get_mut(&upload.id).context("未知插件纹理")?;
                let size = texture.size();
                ensure!(
                    position[0]
                        .checked_add(upload.size[0])
                        .is_some_and(|v| v <= size[0])
                        && position[1]
                            .checked_add(upload.size[1])
                            .is_some_and(|v| v <= size[1]),
                    "纹理更新越界"
                );
                texture.set_partial(position, image, egui::TextureOptions::LINEAR);
            } else if let Some(texture) = self.textures.get_mut(&upload.id) {
                texture.set(image, egui::TextureOptions::LINEAR);
            } else {
                ensure!(self.textures.len() < 64, "插件纹理数量超限");
                self.textures.insert(
                    upload.id,
                    ctx.load_texture(
                        format!("plugin-{}", upload.id),
                        image,
                        egui::TextureOptions::LINEAR,
                    ),
                );
            }
        }
        ensure!(
            self.textures
                .values()
                .map(|t| t.size()[0] * t.size()[1])
                .sum::<usize>()
                <= 4 * 1024 * 1024,
            "插件纹理内存超限"
        );
        Ok(())
    }
}
pub(crate) fn paint_plugin_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let center = rect.center();
    let stroke = egui::Stroke::new(crate::ui::theme::ICON_STROKE, color);
    painter.rect_stroke(
        egui::Rect::from_center_size(center + egui::vec2(0.0, -1.0), egui::vec2(12.0, 9.0)),
        2.5,
        stroke,
        egui::StrokeKind::Inside,
    );
    for x in [-3.0, 3.0] {
        painter.line_segment(
            [center + egui::vec2(x, -5.0), center + egui::vec2(x, -9.0)],
            stroke,
        );
    }
    painter.line(
        vec![
            center + egui::vec2(0.0, 3.5),
            center + egui::vec2(0.0, 7.0),
            center + egui::vec2(4.0, 7.0),
        ],
        stroke,
    );
}
impl Drop for AnalysisController {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(crate) struct Composite {
    pub order: u32,
    pub clip: egui::Rect,
    pub mesh: egui::Mesh,
}
