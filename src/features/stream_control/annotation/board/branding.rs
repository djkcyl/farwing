//! Compact pen paths for the whiteboard's remote-visible brand and metadata.
//! Draw RPC accepts strokes rather than text/images, so the header uses simple
//! filled system-font lettering and the application's linked-screen logo.
use super::*;
use crate::ui::theme;

type Path = Vec<[f32; 2]>;

#[cfg(windows)]
fn text_paths(text: &str, size: f32, weight: i32) -> Result<Vec<Path>> {
    use windows::Win32::{
        Foundation::{COLORREF, SIZE},
        Graphics::Gdi::*,
    };
    use windows::core::w;
    struct Surface {
        dc: HDC,
        bitmap: HBITMAP,
        font: HFONT,
        old_bitmap: HGDIOBJ,
        old_font: HGDIOBJ,
    }
    impl Drop for Surface {
        fn drop(&mut self) {
            unsafe {
                if !self.old_font.is_invalid() {
                    SelectObject(self.dc, self.old_font);
                }
                if !self.old_bitmap.is_invalid() {
                    SelectObject(self.dc, self.old_bitmap);
                }
                if !self.font.is_invalid() {
                    let _ = DeleteObject(self.font.into());
                }
                if !self.bitmap.is_invalid() {
                    let _ = DeleteObject(self.bitmap.into());
                }
                if !self.dc.is_invalid() {
                    let _ = DeleteDC(self.dc);
                }
            }
        }
    }
    // Render only the requested label, not any desktop pixels. Each connected
    // filled glyph becomes one continuous pen walk, avoiding hundreds of scanline RPCs.
    let mut surface = Surface {
        dc: unsafe { CreateCompatibleDC(None) },
        bitmap: HBITMAP::default(),
        font: HFONT::default(),
        old_bitmap: HGDIOBJ::default(),
        old_font: HGDIOBJ::default(),
    };
    if surface.dc.is_invalid() {
        bail!("无法创建白板文字画布");
    }
    surface.font = unsafe {
        CreateFontW(
            -(size * 2.).round() as i32,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_TT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            DEFAULT_PITCH.0 as u32,
            w!("Segoe UI"),
        )
    };
    if surface.font.is_invalid() {
        bail!("无法加载白板字体");
    }
    surface.old_font = unsafe { SelectObject(surface.dc, surface.font.into()) };
    if surface.old_font.is_invalid() {
        bail!("无法选择白板字体");
    }
    let text = text.encode_utf16().collect::<Vec<_>>();
    let mut extent = SIZE::default();
    unsafe {
        GetTextExtentPoint32W(surface.dc, &text, &mut extent).ok()?;
    }
    let width = extent.cx.max(1) as usize + 4;
    let height = (size * 2.).ceil() as usize + 8;
    if width > 2048 || height > 128 {
        bail!("白板文字尺寸超出范围");
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits = std::ptr::null_mut();
    surface.bitmap =
        unsafe { CreateDIBSection(Some(surface.dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)? };
    if bits.is_null() {
        bail!("无法分配白板文字画布");
    }
    surface.old_bitmap = unsafe { SelectObject(surface.dc, surface.bitmap.into()) };
    if surface.old_bitmap.is_invalid() {
        bail!("无法选择白板文字画布");
    }
    unsafe {
        std::ptr::write_bytes(bits.cast::<u8>(), 0, width * height * 4);
        SetBkColor(surface.dc, COLORREF(0));
        SetTextColor(surface.dc, COLORREF(0xffffff));
        TextOutW(surface.dc, 2, 2, &text).ok()?;
        GdiFlush().ok()?;
    }
    let pixels = unsafe { std::slice::from_raw_parts(bits.cast::<u8>(), width * height * 4) };
    let mut remaining = pixels
        .chunks_exact(4)
        .map(|p| p[0] >= 128)
        .collect::<Vec<_>>();
    let point = |i: usize| {
        [
            (i % width) as f32 * 0.5 + 0.25,
            (i / width) as f32 * 0.5 + 0.25,
        ]
    };
    let mut paths = Vec::new();
    for seed in 0..remaining.len() {
        if !remaining[seed] {
            continue;
        }
        remaining[seed] = false;
        let mut path = vec![point(seed)];
        let mut stack = vec![(seed, 0u8)];
        while let Some((at, direction)) = stack.last_mut() {
            if *direction == 4 {
                stack.pop();
                if let Some((parent, _)) = stack.last() {
                    path.push(point(*parent));
                }
                continue;
            }
            let next = match *direction {
                0 if *at % width + 1 < width => Some(*at + 1),
                1 if *at / width + 1 < height => Some(*at + width),
                2 if *at % width > 0 => Some(*at - 1),
                3 if *at >= width => Some(*at - width),
                _ => None,
            };
            *direction += 1;
            if let Some(next) = next
                && remaining[next]
            {
                remaining[next] = false;
                path.push(point(next));
                stack.push((next, 0));
            }
        }
        let mut compact: Path = Vec::new();
        for p in path {
            if compact.len() >= 2 {
                let a = compact[compact.len() - 2];
                let b = compact[compact.len() - 1];
                let u = [b[0] - a[0], b[1] - a[1]];
                let v = [p[0] - b[0], p[1] - b[1]];
                if u[0] * v[1] == u[1] * v[0] && u[0] * v[0] + u[1] * v[1] > 0. {
                    compact.pop();
                }
            }
            compact.push(p);
        }
        paths.push(compact);
    }
    Ok(paths)
}

/// The lettering is traced from a GDI rasterization, which has no Linux
/// counterpart yet. The brand mark and its frame still draw; only the text is
/// left out, so the remote side sees a header without a caption.
#[cfg(not(windows))]
fn text_paths(text: &str, _size: f32, _weight: i32) -> Result<Vec<Path>> {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        tracing::info!("whiteboard brand lettering is unavailable on this platform");
    });
    let _ = text;
    Ok(Vec::new())
}

fn arc(cx: f32, cy: f32, rx: f32, ry: f32, start: f32, end: f32) -> Path {
    (0..=20)
        .map(|i| {
            let a = (start + (end - start) * i as f32 / 20.).to_radians();
            [cx + a.cos() * rx, cy + a.sin() * ry]
        })
        .collect()
}
fn rounded_logo() -> Vec<(Path, f32, [u8; 3])> {
    let mut outline = vec![[6., 4.], [26., 4.]];
    outline.extend(arc(26., 6., 2., 2., -90., 0.));
    outline.push([28., 26.]);
    outline.extend(arc(26., 26., 2., 2., 0., 90.));
    outline.push([6., 28.]);
    outline.extend(arc(6., 26., 2., 2., 90., 180.));
    outline.push([4., 6.]);
    outline.extend(arc(6., 6., 2., 2., 180., 270.));
    let tile = vec![
        [10., 10.],
        [22., 10.],
        [22., 16.],
        [10., 16.],
        [10., 22.],
        [22., 22.],
    ];
    // Coordinates follow assets/icon-256.png; keep the blue and white interlock.
    let blue = vec![
        [94., 155.],
        [65., 155.],
        [55., 153.],
        [49., 146.],
        [48., 137.],
        [48., 96.],
        [50., 86.],
        [57., 79.],
        [67., 77.],
        [132., 77.],
        [142., 79.],
        [150., 87.],
        [152., 97.],
        [152., 122.],
        [150., 133.],
        [143., 141.],
        [135., 144.],
    ];
    let white = vec![
        [135., 119.],
        [125., 121.],
        [116., 129.],
        [110., 140.],
        [110., 162.],
        [112., 173.],
        [121., 180.],
        [131., 182.],
        [188., 182.],
        [199., 179.],
        [206., 171.],
        [207., 160.],
        [207., 131.],
        [204., 120.],
        [196., 113.],
        [186., 111.],
        [170., 111.],
    ];
    vec![
        (outline, 8., theme::ANNOTATION_BRAND_TILE),
        (tile, 12., theme::ANNOTATION_BRAND_TILE),
        (
            blue.into_iter().map(|[x, y]| [x / 8., y / 8.]).collect(),
            2.5,
            theme::ANNOTATION_BRAND_BLUE,
        ),
        (
            white.into_iter().map(|[x, y]| [x / 8., y / 8.]).collect(),
            2.5,
            theme::ANNOTATION_BRAND_WHITE,
        ),
    ]
}

pub(super) fn build(
    base: u32,
    screen: i32,
    metrics: Metrics,
    background: [u8; 3],
) -> Result<Vec<Stroke>> {
    let lw = metrics.width as f32 * 100. / metrics.dpi as f32;
    let lh = metrics.height as f32 * 100. / metrics.dpi as f32;
    let light = u32::from(background[0]) * 299
        + u32::from(background[1]) * 587
        + u32::from(background[2]) * 114
        > 150_000;
    let colors = if light {
        theme::ANNOTATION_BRAND_ON_LIGHT
    } else {
        theme::ANNOTATION_BRAND_ON_DARK
    };
    let mut paths = Vec::new();
    let padding = theme::ANNOTATION_BRAND_PADDING;
    let logo_scale = theme::ANNOTATION_BRAND_LOGO_SIZE / 32.;
    for (path, width, color) in rounded_logo() {
        paths.push((
            path.into_iter()
                .flat_map(|[x, y]| [[padding + x * logo_scale, padding + y * logo_scale]; 3])
                .collect::<Path>(),
            width * logo_scale,
            color,
        ));
    }
    let info = format!(
        "v{}  ·  {} × {}  ·  {}% DPI",
        env!("CARGO_PKG_VERSION"),
        metrics.width,
        metrics.height,
        metrics.dpi
    );
    let x = padding + theme::ANNOTATION_BRAND_LOGO_SIZE + theme::ANNOTATION_BRAND_GAP;
    for (text, y, size, weight, color) in [
        (
            "OpenUUYC",
            padding,
            theme::ANNOTATION_BRAND_TITLE_SIZE,
            600,
            colors[0],
        ),
        (
            info.as_str(),
            padding + theme::ANNOTATION_BRAND_TITLE_SIZE + theme::ANNOTATION_BRAND_GAP * 0.5,
            theme::ANNOTATION_BRAND_INFO_SIZE,
            400,
            colors[1],
        ),
    ] {
        for path in text_paths(text, size, weight)? {
            paths.push((
                path.into_iter()
                    .map(|[px, py]| [x + px, y + py])
                    .collect::<Path>(),
                0.75,
                color,
            ));
        }
    }
    let extent =
        paths
            .iter()
            .flat_map(|(path, _, _)| path.iter())
            .fold([1_f32, 1_f32], |mut size, p| {
                size[0] = size[0].max(p[0] + padding);
                size[1] = size[1].max(p[1] + padding);
                size
            });
    let scale = (lw / extent[0]).min(lh / extent[1]).min(1.);
    if paths.len() >= BOARD_SLOT_IDS as usize {
        bail!("白板标识内容超出范围");
    }
    Ok(paths
        .into_iter()
        .enumerate()
        .map(|(i, (points, width, rgb))| Stroke {
            id: base + 1 + i as u32,
            screen,
            points: points
                .into_iter()
                .map(|[x, y]| Point {
                    x: (x * scale / lw).clamp(0., 1.),
                    y: (y * scale / lh).clamp(0., 1.),
                })
                .collect(),
            style: Style {
                argb: u32::from_be_bytes([255, rgb[0], rgb[1], rgb[2]]),
                width: width * scale,
            },
        })
        .collect())
}
