//! Build `server/emoji/emoji.pack` (`docs/protocol/chat-design.md` §12.3):
//! every Fluent Emoji flat SVG drawn at 72 pixels as PNG, named by its code
//! points (`1f44d-1f3fb.png`, `fe0f` left out), and `index.json`: the
//! emoji in Unicode's order with their English and Chinese names and
//! keywords for the picker.
//!
//! `emoji-pack <fluentui-emoji/assets> <cldr zh annotations.json>
//! <cldr zh annotationsDerived.json> <emoji-test.txt> <out.pack>`

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const SIDE: u32 = 72;

/// Skin-tone folders in the order of `unicodeSkintones`.
const TONES: [&str; 6] = ["Default", "Light", "Medium-Light", "Medium", "Medium-Dark", "Dark"];

fn key(seq: &str) -> String {
    seq.split([' ', '-'])
        .filter(|cp| !cp.is_empty() && !cp.eq_ignore_ascii_case("fe0f"))
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join("-")
}

fn glyph_key(glyph: &str) -> String {
    glyph
        .chars()
        .filter(|c| *c != '\u{fe0f}')
        .map(|c| format!("{:x}", c as u32))
        .collect::<Vec<_>>()
        .join("-")
}

fn render(svg: &Path) -> Result<Vec<u8>> {
    let data = std::fs::read(svg)?;
    let tree = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default())?;
    let size = tree.size();
    let scale = SIDE as f32 / size.width().max(size.height());
    let mut pixmap = resvg::tiny_skia::Pixmap::new(SIDE, SIDE).context("pixmap")?;
    let dx = (SIDE as f32 - size.width() * scale) / 2.0;
    let dy = (SIDE as f32 - size.height() * scale) / 2.0;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap.encode_png()?)
}

fn first_svg(dir: &Path) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "svg"))
        .collect();
    found.sort();
    found.into_iter().next()
}

/// CLDR names and keywords by emoji key.
fn cldr(paths: &[&Path]) -> Result<HashMap<String, (String, Vec<String>)>> {
    let mut out = HashMap::new();
    for path in paths {
        let v: Value = serde_json::from_slice(&std::fs::read(path)?)?;
        let table = v["annotations"]["annotations"]
            .as_object()
            .or_else(|| v["annotationsDerived"]["annotations"].as_object())
            .context("annotations")?;
        for (glyph, entry) in table {
            let name = entry["tts"][0].as_str().unwrap_or("").to_owned();
            let words: Vec<String> = entry["default"]
                .as_array()
                .map(|a| a.iter().filter_map(|w| w.as_str().map(str::to_owned)).collect())
                .unwrap_or_default();
            out.insert(glyph_key(glyph), (name, words));
        }
    }
    Ok(out)
}

/// Fully qualified emoji in Unicode's order: key → position.
fn order(path: &Path) -> Result<HashMap<String, usize>> {
    let text = std::fs::read_to_string(path)?;
    let mut out = HashMap::new();
    for line in text.lines() {
        let Some((cps, rest)) = line.split_once(';') else { continue };
        if !rest.trim_start().starts_with("fully-qualified") {
            continue;
        }
        let k = key(cps.trim());
        let n = out.len();
        out.entry(k).or_insert(n);
    }
    Ok(out)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [assets, zh, zh_derived, test, out] = args.as_slice() else {
        bail!("usage: emoji-pack <assets> <zh annotations> <zh derived> <emoji-test.txt> <out.pack>");
    };
    let names = cldr(&[Path::new(zh), Path::new(zh_derived)])?;
    let order = order(Path::new(test))?;
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut index: Vec<(usize, Value)> = Vec::new();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(assets)?.flatten().map(|e| e.path()).collect();
    dirs.sort();
    for dir in dirs {
        let Ok(meta) = std::fs::read(dir.join("metadata.json")) else { continue };
        let meta: Value = serde_json::from_slice(&meta)?;
        let base = key(meta["unicode"].as_str().unwrap_or(""));
        if base.is_empty() {
            continue;
        }
        let mut drawn = Vec::new();
        if let Some(svg) = first_svg(&dir.join("Flat")) {
            drawn.push((base.clone(), svg));
        }
        if let Some(tones) = meta["unicodeSkintones"].as_array() {
            for (i, seq) in tones.iter().enumerate() {
                let (Some(seq), Some(folder)) = (seq.as_str(), TONES.get(i)) else { continue };
                if let Some(svg) = first_svg(&dir.join(folder).join("Flat")) {
                    drawn.push((key(seq), svg));
                }
            }
        }
        if drawn.is_empty() {
            continue;
        }
        for (k, svg) in &drawn {
            match render(svg) {
                Ok(png) => files.push((format!("{k}.png"), png)),
                Err(e) => eprintln!("{}: {e}", svg.display()),
            }
        }
        let (zh_name, zh_words) = names.get(&base).cloned().unwrap_or_default();
        let en_words: Vec<String> = meta["keywords"]
            .as_array()
            .map(|a| a.iter().filter_map(|w| w.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        let position = order.get(&base).copied().unwrap_or(usize::MAX);
        index.push((
            position,
            json!({
                "e": meta["glyph"],
                "c": base,
                "g": meta["group"],
                "n": meta["cldr"],
                "k": en_words,
                "zn": zh_name,
                "zk": zh_words,
                "t": meta["unicodeSkintones"].is_array(),
            }),
        ));
    }
    index.sort_by_key(|(p, _)| *p);
    let index: Vec<Value> = index.into_iter().map(|(_, v)| v).collect();
    files.push(("index.json".to_owned(), serde_json::to_vec(&index)?));
    // EMOJ, count, (name length, name, offset, length)…, files.
    let header: usize = 8 + files.iter().map(|(n, _)| 2 + n.len() + 8).sum::<usize>();
    let mut pack = Vec::new();
    pack.extend_from_slice(b"EMOJ");
    pack.extend_from_slice(&(files.len() as u32).to_le_bytes());
    let mut offset = header;
    for (name, bytes) in &files {
        pack.extend_from_slice(&(name.len() as u16).to_le_bytes());
        pack.extend_from_slice(name.as_bytes());
        pack.extend_from_slice(&(offset as u32).to_le_bytes());
        pack.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        offset += bytes.len();
    }
    for (_, bytes) in &files {
        pack.extend_from_slice(bytes);
    }
    std::fs::write(out, &pack)?;
    println!("{} files, {} emoji, {} bytes", files.len(), index.len(), pack.len());
    Ok(())
}
