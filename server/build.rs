//! Embed the built web console (`web/dist`, produced by `npm run build`) in
//! the server binary. Without it the server still builds; the web console
//! then shows that it was not built (`docs/web-console.md` §3).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("web").join("dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    let mut list = Vec::new();
    files(&dist, &mut list);
    let mut code = String::from("pub static ASSETS: &[(&str, &[u8])] = &[\n");
    for path in &list {
        let relative = path
            .strip_prefix(&dist)
            .expect("inside dist")
            .to_string_lossy()
            .replace('\\', "/");
        let _ = writeln!(
            code,
            "    ({:?}, include_bytes!({:?})),",
            format!("/{relative}"),
            path.display().to_string()
        );
    }
    code.push_str("];\n");
    // Emoji images (`emoji/emoji.pack`, made by `emoji/tool`).
    let pack = Path::new(env!("CARGO_MANIFEST_DIR")).join("emoji").join("emoji.pack");
    println!("cargo:rerun-if-changed={}", pack.display());
    if pack.is_file() {
        let _ = writeln!(
            code,
            "pub static EMOJI: &[u8] = include_bytes!({:?});",
            pack.display().to_string()
        );
    } else {
        code.push_str("pub static EMOJI: &[u8] = &[];\n");
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("web_assets.rs");
    std::fs::write(out, code).expect("write web_assets.rs");
}
