use std::{path::PathBuf, process::Command};
fn main() {
    println!("cargo:rerun-if-env-changed=FXC");
    let compiler = std::env::var_os("FXC")
        .map(PathBuf::from)
        .or_else(|| {
            let root =
                PathBuf::from(std::env::var_os("ProgramFiles(x86)")?).join("Windows Kits/10/bin");
            let mut candidates = std::fs::read_dir(root)
                .ok()?
                .flatten()
                .map(|e| e.path().join("x64/fxc.exe"))
                .filter(|p| p.is_file())
                .collect::<Vec<_>>();
            candidates.sort();
            candidates.pop()
        })
        .expect("Windows SDK fxc.exe is required; set FXC to its path");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    for shader in ["color", "sharpen", "pixelate"] {
        let input = format!("src/{shader}.hlsl");
        println!("cargo:rerun-if-changed={input}");
        let result = Command::new(&compiler)
            .args(["/nologo", "/T", "ps_5_0", "/E", "main", "/O3", "/Fo"])
            .arg(out.join(format!("{shader}.cso")))
            .arg(&input)
            .output()
            .expect("run fxc");
        assert!(
            result.status.success(),
            "{input}: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
