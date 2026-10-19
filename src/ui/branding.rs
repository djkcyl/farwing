//! Shared application artwork, decoded once and uploaded once per egui context.
use std::sync::{Arc, OnceLock};

pub(crate) fn icon() -> Arc<egui::IconData> {
    static ICON: OnceLock<Arc<egui::IconData>> = OnceLock::new();
    Arc::clone(ICON.get_or_init(|| {
        let pixels = image::load_from_memory_with_format(
            include_bytes!("../../assets/icon-256.png"),
            image::ImageFormat::Png,
        )
        .expect("embedded application icon")
        .into_rgba8();
        Arc::new(egui::IconData {
            width: pixels.width(),
            height: pixels.height(),
            rgba: pixels.into_raw(),
        })
    }))
}

pub(crate) fn load_texture(ctx: &egui::Context) -> egui::TextureHandle {
    let icon = icon();
    ctx.load_texture(
        "farwing-brand-icon",
        egui::ColorImage::from_rgba_unmultiplied(
            [icon.width as usize, icon.height as usize],
            &icon.rgba,
        ),
        egui::TextureOptions::LINEAR,
    )
}

pub(crate) fn paint(painter: &egui::Painter, rect: egui::Rect, texture: &egui::TextureHandle) {
    painter.image(
        texture.id(),
        rect,
        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
}

pub(crate) fn window_icon() -> winit::window::Icon {
    let icon = icon();
    winit::window::Icon::from_rgba(icon.rgba.clone(), icon.width, icon.height)
        .expect("embedded window icon dimensions")
}

pub(crate) fn set_taskbar_icon(window: &winit::window::Window) {
    use winit::platform::windows::WindowExtWindows;
    window.set_taskbar_icon(Some(window_icon()));
}
