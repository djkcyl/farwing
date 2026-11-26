//! Shared application artwork, decoded once and uploaded once per egui context.
use std::sync::{Arc, OnceLock};

/// 方块轮廓与右上角红角标的几何，坐标是 `assets/icon-256.png` 的 256 尺度像素：
/// 圆角半径由图标轮廓拟合而来，直角边长度取到刚好盖住右上角那朵半透明的云、
/// 又不会切到白色相扣环。
const TILE: [f32; 4] = [13.5, 16.5, 241.5, 239.5];
const TILE_RADIUS: f32 = 42.0;
const BADGE_LEG: f32 = 120.0;
/// 正在被远程访问时角标的颜色。
const BADGE_COLOR: [u8; 3] = [232, 62, 74];

/// 应用图标。`controlled` 为真时右上角带一个红角标，托盘用它表示这台机器
/// 正被别人控制；两份图标各解码一次并缓存。
pub(crate) fn icon(controlled: bool) -> Arc<egui::IconData> {
    static IDLE: OnceLock<Arc<egui::IconData>> = OnceLock::new();
    static CONTROLLED: OnceLock<Arc<egui::IconData>> = OnceLock::new();
    let cached = if controlled { &CONTROLLED } else { &IDLE };
    Arc::clone(cached.get_or_init(|| {
        let mut pixels = image::load_from_memory_with_format(
            include_bytes!("../../assets/icon-256.png"),
            image::ImageFormat::Png,
        )
        .expect("embedded application icon")
        .into_rgba8();
        if controlled {
            let (width, height) = (pixels.width() as usize, pixels.height() as usize);
            draw_badge(pixels.as_mut(), width, height);
        }
        let (width, height) = pixels.dimensions();
        Arc::new(egui::IconData {
            width,
            height,
            rgba: pixels.into_raw(),
        })
    }))
}

/// 托盘要的尺寸：Windows 的 HICON 与 Linux 的 StatusNotifierItem pixmap 共用。
pub(crate) fn tray_pixmap(size: u32, controlled: bool) -> image::RgbaImage {
    let icon = icon(controlled);
    let source = image::RgbaImage::from_raw(icon.width, icon.height, icon.rgba.clone())
        .expect("embedded application icon is RGBA");
    image::imageops::resize(&source, size, size, image::imageops::FilterType::Triangle)
}

/// 直角在方块右上角、圆角跟着方块轮廓走的红三角。画的是实心色块而不是叠加，
/// 所以角标底下原来是底色还是那朵云都不影响结果。
fn draw_badge(pixels: &mut [u8], width: usize, height: usize) {
    const SAMPLES: usize = 4;
    for y in 0..height {
        for x in 0..width {
            let mut covered = 0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let fx = x as f32 + (sx as f32 + 0.5) / SAMPLES as f32;
                    let fy = y as f32 + (sy as f32 + 0.5) / SAMPLES as f32;
                    // 直角边贴着方块的上边与右边，斜边朝左下。
                    let inside_legs = (TILE[2] - fx) + (fy - TILE[1]) <= BADGE_LEG;
                    if inside_legs && inside_tile(fx, fy) {
                        covered += 1;
                    }
                }
            }
            if covered == 0 {
                continue;
            }
            let pixel = &mut pixels[(y * width + x) * 4..][..4];
            pixel[..3].copy_from_slice(&BADGE_COLOR);
            pixel[3] = (covered * 255 / (SAMPLES * SAMPLES)) as u8;
        }
    }
}

fn inside_tile(x: f32, y: f32) -> bool {
    if !(TILE[0]..=TILE[2]).contains(&x) || !(TILE[1]..=TILE[3]).contains(&y) {
        return false;
    }
    let cx = x.clamp(TILE[0] + TILE_RADIUS, TILE[2] - TILE_RADIUS);
    let cy = y.clamp(TILE[1] + TILE_RADIUS, TILE[3] - TILE_RADIUS);
    (x - cx).powi(2) + (y - cy).powi(2) <= TILE_RADIUS * TILE_RADIUS
}

pub(crate) fn load_texture(ctx: &egui::Context) -> egui::TextureHandle {
    let icon = icon(false);
    ctx.load_texture(
        "openuuyc-brand-icon",
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
    let icon = icon(false);
    winit::window::Icon::from_rgba(icon.rgba.clone(), icon.width, icon.height)
        .expect("embedded window icon dimensions")
}

pub(crate) fn set_taskbar_icon(window: &winit::window::Window) {
    #[cfg(windows)]
    {
        use winit::platform::windows::WindowExtWindows;
        window.set_taskbar_icon(Some(window_icon()));
    }
    // X11 takes the icon from the window itself; Wayland matches the .desktop
    // file by app id, which the event loop sets when the window is created.
    #[cfg(not(windows))]
    window.set_window_icon(Some(window_icon()));
}
