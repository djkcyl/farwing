//! The complete egui fonts, compressed at build time and shared by all windows.
//!
//! All font bytes are `'static`: egui copies owned font data every time a
//! context builds its fonts, which for the system CJK font (about 20 MB)
//! meant one private copy per window plus ours.
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
                // Decompressed once per process and kept for its lifetime.
                let bytes: &'static [u8] = Vec::leak(bytes);
                fonts.font_data.insert(
                    $name.into(),
                    Arc::new(FontData::from_static(bytes).tweak(FontTweak {
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

/// The system CJK font, mapped once per process: its pages belong to the
/// file (shared, read on demand), not to the process's private memory.
fn system_cjk() -> Option<&'static (PathBuf, memmap2::Mmap)> {
    static FONT: LazyLock<Option<(PathBuf, memmap2::Mmap)>> = LazyLock::new(|| {
        let directory = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("Fonts");
        ["msyh.ttc", "msyhbd.ttc", "simhei.ttf", "simsun.ttc"]
            .into_iter()
            .find_map(|name| {
                let path = directory.join(name);
                let file = std::fs::File::open(&path).ok()?;
                // SAFETY: Windows keeps an installed font file in use; the
                // mapping is read-only and lives as long as the process.
                let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
                Some((path, map))
            })
    });
    FONT.as_ref()
}

pub(crate) fn install(ctx: &egui::Context) {
    let mut fonts = definitions();
    if let Some((path, bytes)) = system_cjk() {
        let name = "system-cjk".to_owned();
        fonts
            .font_data
            .insert(name.clone(), Arc::new(FontData::from_static(&bytes[..])));
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
