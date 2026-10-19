//! Retain the last submitted UI image description, independently of UI ticks.
use egui::epaint::{ClippedShape, Shape};
use egui_directx11::RendererOutput;

pub(super) fn merge(pending: &mut RendererOutput, next: RendererOutput) {
    // A hidden/busy window can receive repeated full image replacements. Keep
    // only the latest full image and its following patches, instead of retaining
    // every superseded bitmap until the window is visible again. Font atlas
    // patches and texture releases still preserve the renderer's normal order.
    pending.textures_delta.append(next.textures_delta);
    pending.shapes = next.shapes;
    pending.pixels_per_point = next.pixels_per_point;
}

pub(super) struct Content {
    shapes: Vec<ClippedShape>,
    scale: f32,
    zoom: f32,
    transparent: bool,
}
fn external(shape: &Shape) -> bool {
    match shape {
        Shape::Callback(_) => true,
        Shape::Vec(shapes) => shapes.iter().any(external),
        _ => matches!(shape.texture_id(), egui::TextureId::User(_)),
    }
}
impl Content {
    pub fn new(output: &RendererOutput, zoom: f32, transparent: bool) -> Self {
        Self {
            shapes: output.shapes.clone(),
            scale: output.pixels_per_point,
            zoom,
            transparent,
        }
    }
    pub fn matches(&self, output: &RendererOutput, zoom: f32, transparent: bool) -> bool {
        output.textures_delta.is_empty()
            && self.scale == output.pixels_per_point
            && self.zoom == zoom
            && self.transparent == transparent
            && !output.shapes.iter().any(|s| external(&s.shape))
            && self.shapes == output.shapes
    }
}
