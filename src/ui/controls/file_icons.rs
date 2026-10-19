//! Shared file kinds for local listings, peer metadata and transfer rows.
use super::{paint_file_icon, theme};
use egui::{Align2, FontId, Painter, Rect, Shape, Stroke, StrokeKind, vec2};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Folder,
    Disk,
    Desktop,
    Download,
    Document,
    Image,
    Video,
    Audio,
    Archive,
    Sheet,
    Slides,
    Pdf,
    Code,
    Application,
}

fn kind(name: &str, entry_type: i32, marker: &str) -> Kind {
    if entry_type == 3 {
        return Kind::Disk;
    }
    if entry_type < 4 {
        return match marker {
            "desktop" => Kind::Desktop,
            "download" => Kind::Download,
            "document" => Kind::Document,
            "picture" => Kind::Image,
            "video" => Kind::Video,
            "music" => Kind::Audio,
            _ => Kind::Folder,
        };
    }
    let extension = if let Some(ext) = marker.strip_prefix('.') {
        ext
    } else {
        // Entries from peers without a marker still have a useful filename.
        name.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("")
    }
    .to_ascii_lowercase();
    match extension.as_str() {
        "doc" | "docx" | "odt" | "rtf" | "txt" | "md" => Kind::Document,
        "xls" | "xlsx" | "ods" | "csv" => Kind::Sheet,
        "ppt" | "pptx" | "odp" => Kind::Slides,
        "pdf" => Kind::Pdf,
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tif" | "tiff" | "svg" | "ico"
        | "heic" => Kind::Image,
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "wmv" | "flv" | "mpeg" | "m4v" => Kind::Video,
        "mp3" | "wav" | "flac" | "ogg" | "opus" | "aac" | "m4a" | "wma" => Kind::Audio,
        "zip" | "7z" | "rar" | "tar" | "gz" | "bz2" | "xz" | "cab" => Kind::Archive,
        "exe" | "msi" | "msix" | "appx" | "dll" => Kind::Application,
        "rs" | "c" | "cpp" | "h" | "py" | "js" | "ts" | "json" | "xml" | "html" | "css"
        | "toml" | "yaml" | "yml" | "ps1" | "bat" | "cmd" => Kind::Code,
        _ => Kind::File,
    }
}

pub(crate) fn paint(p: &Painter, rect: Rect, name: &str, entry_type: i32, marker: &str) {
    let kind = kind(name, entry_type, marker);
    let c = rect.center();
    let color = match kind {
        Kind::Folder | Kind::Archive | Kind::Slides => theme::AMBER,
        Kind::Image | Kind::Audio | Kind::Sheet => theme::GREEN,
        Kind::Pdf => theme::RED,
        Kind::File => theme::MUTED,
        _ => theme::ACCENT,
    };
    let s = Stroke::new(theme::ICON_STROKE, color);
    let line = |a, b| {
        p.line_segment([c + a, c + b], s);
    };
    match kind {
        Kind::Disk => {
            p.rect(
                Rect::from_center_size(c + vec2(0., 2.), vec2(17., 10.)),
                2,
                color.gamma_multiply(0.12),
                s,
                StrokeKind::Inside,
            );
            line(vec2(-7., -3.), vec2(-5., -7.));
            line(vec2(-5., -7.), vec2(5., -7.));
            line(vec2(5., -7.), vec2(7., -3.));
            p.circle_filled(c + vec2(5., 2.), 1., color);
        }
        Kind::Desktop => super::files::paint_icon(p, rect, super::files::Icon::Computer, color),
        Kind::Folder => paint_file_icon(p, rect, color, true),
        _ => {
            paint_file_icon(p, rect, color, false);
            match kind {
                Kind::Download => {
                    line(vec2(0., -3.), vec2(0., 4.));
                    line(vec2(-3., 1.), vec2(0., 4.));
                    line(vec2(0., 4.), vec2(3., 1.));
                }
                Kind::Image => {
                    p.circle_filled(c + vec2(-2., -2.), 1.2, color);
                    p.add(Shape::line(
                        vec![
                            c + vec2(-4., 5.),
                            c + vec2(-1., 1.),
                            c + vec2(1., 3.),
                            c + vec2(3., 0.),
                            c + vec2(4., 5.),
                        ],
                        s,
                    ));
                }
                Kind::Video => {
                    p.add(Shape::convex_polygon(
                        vec![c + vec2(-2., -3.), c + vec2(4., 1.), c + vec2(-2., 5.)],
                        color,
                        Stroke::NONE,
                    ));
                }
                Kind::Audio => {
                    line(vec2(2., -3.), vec2(2., 3.));
                    line(vec2(2., -3.), vec2(4., -2.));
                    p.circle_filled(c + vec2(0., 4.), 2., color);
                }
                Kind::Archive => {
                    for y in [-4., -1., 2., 5.] {
                        line(vec2(-1., y), vec2(1., y));
                    }
                }
                Kind::Sheet => {
                    for y in [-1., 2., 5.] {
                        line(vec2(-3., y), vec2(3., y));
                    }
                    line(vec2(0., -2.), vec2(0., 5.));
                }
                Kind::Slides | Kind::Pdf | Kind::Code | Kind::Application => {
                    let label = match kind {
                        Kind::Slides => "P",
                        Kind::Pdf => "P",
                        Kind::Code => "<>",
                        _ => "+",
                    };
                    p.text(
                        c + vec2(0., 1.),
                        Align2::CENTER_CENTER,
                        label,
                        FontId::monospace(9.),
                        color,
                    );
                }
                Kind::Document => {
                    for y in [-1., 2., 5.] {
                        line(vec2(-3., y), vec2(3., y));
                    }
                }
                _ => {}
            }
        }
    }
    if matches!(entry_type, 2 | 5) {
        let origin = c + vec2(-6., 5.);
        p.rect_filled(
            Rect::from_center_size(origin, vec2(7., 7.)),
            1,
            theme::SIDEBAR,
        );
        p.add(Shape::line(
            vec![
                origin + vec2(-2., 2.),
                origin + vec2(-2., -1.),
                origin + vec2(2., -1.),
                origin + vec2(0., -3.),
            ],
            Stroke::new(1., theme::TEXT),
        ));
    }
}
