//! The complete egui fonts, compressed at build time and shared by all windows.
use egui::{FontData, FontDefinitions, FontFamily, FontTweak};
use std::{
    io::Read,
    path::PathBuf,
    sync::{Arc, LazyLock},
};

pub(crate) fn definitions() -> FontDefinitions {
    static FONTS: LazyLock<FontDefinitions> = LazyLock::new(|| {
        let mut fonts = FontDefinitions::empty();
        // Keep egui 0.36.1's exact font set, fallback order and emoji metrics.
        // Font sources and licenses come from epaint_default_fonts of that version.
        macro_rules! font {
            ($name:literal, $scale:expr) => {{
                let mut bytes = Vec::new();
                flate2::read::ZlibDecoder::new(
                    include_bytes!(concat!(env!("OUT_DIR"), "/", $name, ".zlib")).as_slice(),
                )
                .read_to_end(&mut bytes)
                .expect("invalid bundled font");
                fonts.font_data.insert(
                    $name.into(),
                    Arc::new(FontData::from_owned(bytes).tweak(FontTweak {
                        scale: $scale,
                        ..Default::default()
                    })),
                );
            }};
        }
        font!("Hack", 1.0);
        font!("Ubuntu-Light", 1.0);
        font!("NotoEmoji-Regular", 0.81);
        font!("emoji-icon-font", 0.90);
        fonts.families.insert(
            FontFamily::Monospace,
            [
                "Hack",
                "Ubuntu-Light",
                "NotoEmoji-Regular",
                "emoji-icon-font",
            ]
            .map(String::from)
            .into(),
        );
        fonts.families.insert(
            FontFamily::Proportional,
            ["Ubuntu-Light", "NotoEmoji-Regular", "emoji-icon-font"]
                .map(String::from)
                .into(),
        );
        fonts
    });
    FONTS.clone()
}

pub(crate) fn install(ctx: &egui::Context) {
    let mut fonts = definitions();
    let directory = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("Fonts");
    let system = ["msyh.ttc", "msyhbd.ttc", "simhei.ttf", "simsun.ttc"]
        .into_iter()
        .find_map(|name| {
            let path = directory.join(name);
            std::fs::read(&path).ok().map(|bytes| (path, bytes))
        });
    if let Some((path, bytes)) = system {
        let name = "system-cjk".to_owned();
        fonts
            .font_data
            .insert(name.clone(), Arc::new(FontData::from_owned(bytes)));
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push(name.clone());
        }
        tracing::debug!(path = %path.display(), "installed system CJK font");
    } else {
        tracing::warn!("no system CJK font found; non-Latin labels may be unavailable");
    }
    ctx.set_fonts(fonts);
    ctx.request_repaint();
}
