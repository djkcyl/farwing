//! Shared application artwork: the application icon, and the mark and
//! wordmark of the Farwing logo (white, tinted when drawn). Each is drawn
//! from a copy resampled to its size in pixels, so small sizes stay sharp.
use std::sync::{Arc, OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Art {
    /// The application icon: the white mark on a blue tile.
    App,
    /// The winged F alone.
    Mark,
    /// `farwing` in the logo's lettering.
    Wordmark,
}
impl Art {
    /// Width over height.
    pub fn aspect(self) -> f32 {
        let source = source(self);
        source.width() as f32 / source.height() as f32
    }
}

pub(crate) fn icon() -> Arc<egui::IconData> {
    static ICON: OnceLock<Arc<egui::IconData>> = OnceLock::new();
    Arc::clone(ICON.get_or_init(|| {
        let pixels = decode(include_bytes!("../../assets/icon-256.png"));
        Arc::new(egui::IconData {
            width: pixels.width(),
            height: pixels.height(),
            rgba: pixels.into_raw(),
        })
    }))
}

fn decode(bytes: &[u8]) -> image::RgbaImage {
    image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .expect("embedded artwork")
        .into_rgba8()
}

/// Premultiplied, so resampling does not darken the edges.
fn source(art: Art) -> &'static image::RgbaImage {
    static SOURCES: [OnceLock<image::RgbaImage>; 3] = [const { OnceLock::new() }; 3];
    SOURCES[art as usize].get_or_init(|| {
        let mut pixels = decode(match art {
            Art::App => include_bytes!("../../assets/icon-256.png"),
            Art::Mark => include_bytes!("../../assets/brand/mark.png"),
            Art::Wordmark => include_bytes!("../../assets/brand/wordmark.png"),
        });
        for pixel in pixels.pixels_mut() {
            let alpha = u16::from(pixel[3]);
            for channel in &mut pixel.0[..3] {
                *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
            }
        }
        pixels
    })
}

/// Draw `art` into `rect` (of the artwork's aspect ratio), aligned to whole
/// pixels. `tint` colours the white mark and wordmark; the icon takes white.
pub(crate) fn paint(painter: &egui::Painter, rect: egui::Rect, art: Art, tint: egui::Color32) {
    let ctx = painter.ctx();
    let scale = ctx.pixels_per_point();
    let size = [
        ((rect.width() * scale).round() as u32).max(1),
        ((rect.height() * scale).round() as u32).max(1),
    ];
    let id = egui::Id::new(("farwing-artwork", art, size));
    let texture = ctx
        .data(|data| data.get_temp::<egui::TextureHandle>(id))
        .unwrap_or_else(|| {
            let mut pixels = image::imageops::resize(
                source(art),
                size[0],
                size[1],
                image::imageops::FilterType::Lanczos3,
            );
            for pixel in pixels.pixels_mut() {
                let alpha = pixel[3];
                for channel in &mut pixel.0[..3] {
                    *channel = (*channel).min(alpha);
                }
            }
            let texture = ctx.load_texture(
                format!("farwing-{art:?}-{}x{}", size[0], size[1]),
                egui::ColorImage::from_rgba_premultiplied(
                    [size[0] as usize, size[1] as usize],
                    pixels.as_raw(),
                ),
                egui::TextureOptions::LINEAR,
            );
            ctx.data_mut(|data| data.insert_temp(id, texture.clone()));
            texture
        });
    let min = (rect.min.to_vec2() * scale).round() / scale;
    let rect = egui::Rect::from_min_size(
        min.to_pos2(),
        egui::vec2(size[0] as f32, size[1] as f32) / scale,
    );
    painter.image(
        texture.id(),
        rect,
        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
        tint,
    );
}

/// The mark and wordmark side by side, in the brand kit's proportions,
/// starting at `left` and vertically centred on `center_y`. Returns the
/// area drawn.
pub(crate) fn paint_logo(
    painter: &egui::Painter,
    left: f32,
    center_y: f32,
    mark_height: f32,
    wordmark: egui::Color32,
) -> egui::Rect {
    let size = logo_size(mark_height);
    let top = center_y - size.y * 0.5;
    let mark = egui::Rect::from_min_size(
        egui::pos2(left, top),
        egui::vec2(mark_height * Art::Mark.aspect(), mark_height),
    );
    paint(painter, mark, Art::Mark, crate::ui::theme::ACCENT);
    let height = mark_height * WORDMARK_HEIGHT;
    let word = egui::Rect::from_min_size(
        egui::pos2(
            mark.right() + mark_height * LOGO_GAP,
            top + mark_height * WORDMARK_TOP,
        ),
        egui::vec2(height * Art::Wordmark.aspect(), height),
    );
    paint(painter, word, Art::Wordmark, wordmark);
    egui::Rect::from_min_size(egui::pos2(left, top), size)
}

/// Size of [`paint_logo`] for a mark this tall.
pub(crate) fn logo_size(mark_height: f32) -> egui::Vec2 {
    let wordmark = mark_height * WORDMARK_HEIGHT;
    egui::vec2(
        mark_height * (Art::Mark.aspect() + LOGO_GAP) + wordmark * Art::Wordmark.aspect(),
        mark_height * (WORDMARK_TOP + WORDMARK_HEIGHT),
    )
}

// The horizontal lockup, relative to the mark's height.
const LOGO_GAP: f32 = 0.283;
const WORDMARK_HEIGHT: f32 = 0.985;
const WORDMARK_TOP: f32 = 0.12;

pub(crate) fn window_icon() -> winit::window::Icon {
    let icon = icon();
    winit::window::Icon::from_rgba(icon.rgba.clone(), icon.width, icon.height)
        .expect("embedded window icon dimensions")
}

pub(crate) fn set_taskbar_icon(window: &winit::window::Window) {
    use winit::platform::windows::WindowExtWindows;
    window.set_taskbar_icon(Some(window_icon()));
}
