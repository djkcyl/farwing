fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let windows = target_os == "windows";
    assert!(
        windows || target_os == "linux" || target_os == "macos",
        "OpenUUYC supports Windows, Linux and macOS; {target_os} has no platform backend"
    );
    if windows {
        println!("cargo:rerun-if-changed=assets/windows.rc");
        println!("cargo:rerun-if-changed=assets/windows.manifest");
        println!("cargo:rerun-if-changed=assets/icon.ico");
        // rc.exe runs with the package root as CWD, so the crate-root-relative
        // paths inside windows.rc resolve; llvm-rc (cross builds from Linux)
        // instead uses the .rc's own directory. Stage the three files side by
        // side so both working directories work; the manifest bytes xtask
        // verifies are copied unchanged.
        #[cfg(not(windows))]
        let resource = {
            let out =
                std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("rc-stage");
            std::fs::create_dir_all(out.join("assets")).unwrap();
            std::fs::copy("assets/windows.rc", out.join("windows.rc")).unwrap();
            std::fs::copy("assets/icon.ico", out.join("assets/icon.ico")).unwrap();
            std::fs::copy("assets/windows.manifest", out.join("assets/windows.manifest"))
                .unwrap();
            out.join("windows.rc").to_str().unwrap().to_string()
        };
        #[cfg(windows)]
        let resource = "assets/windows.rc".to_string();
        embed_resource::compile_for(&resource, ["OpenUUYC"], embed_resource::NONE)
            .manifest_required()
            .expect("compile Windows application icon");
    }
    build_neteq(windows);
    build_fonts();
    // The virtual audio device is a Windows kernel driver package.
    if windows {
        bundle_audio_driver();
    }
}

fn bundle_audio_driver() {
    use std::path::PathBuf;
    println!("cargo:rerun-if-env-changed=OPENUUYC_AUDIO_DRIVER_DIR");
    println!("cargo:rerun-if-changed=drivers/audio/OpenUUYCAudio.inf");
    let output = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let directory = std::env::var_os("OPENUUYC_AUDIO_DRIVER_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/drivers/audio"));
    // Match build.ps1's localized UTF-16LE package, including its BOM.
    let inf_source = std::fs::read_to_string("drivers/audio/OpenUUYCAudio.inf").unwrap();
    let inf: Vec<u8> = std::iter::once(0xfeffu16)
        .chain(inf_source.encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect();
    let mut generated = String::from("pub(super) const FILES: &[(&str, &[u8])] = &[\n");
    for extension in ["inf", "sys", "cat"] {
        let name = format!("OpenUUYCAudio.{extension}");
        let path = directory.join(&name);
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = std::fs::read(&path).expect("read bundled audio driver package");
        assert!(
            !bytes.is_empty(),
            "audio driver package contains an empty file"
        );
        if extension == "inf" {
            assert_eq!(bytes, inf, "audio driver INF differs from source");
        }
        std::fs::write(output.join(&name), bytes).unwrap();
        generated.push_str(&format!(
            "(\"{name}\", include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{name}\"))),\n"
        ));
    }
    generated.push_str("];\n");
    std::fs::write(output.join("audio_driver.rs"), generated).unwrap();
}

fn build_fonts() {
    use std::io::Write;
    let directory = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    for (name, bytes) in [
        ("Hack", epaint_default_fonts::HACK_REGULAR),
        (
            "NotoEmoji-Regular",
            epaint_default_fonts::NOTO_EMOJI_REGULAR,
        ),
        ("Ubuntu-Light", epaint_default_fonts::UBUNTU_LIGHT),
        ("emoji-icon-font", epaint_default_fonts::EMOJI_ICON),
    ] {
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(bytes).expect("compress bundled font");
        std::fs::write(
            directory.join(format!("{name}.zlib")),
            encoder.finish().unwrap(),
        )
        .expect("write bundled font");
    }
}

fn build_neteq(windows: bool) {
    // Crossing into macOS from another host has no Apple SDK to compile the
    // C++ with, so a `cargo check --target *-apple-darwin` from Linux would
    // die in this script before reaching any Rust. Checking needs no native
    // library (nothing links); a real macOS build always has a macOS host.
    let host = std::env::var("HOST").unwrap_or_default();
    let target = std::env::var("TARGET").unwrap_or_default();
    let macos_cross = target.contains("apple-darwin") && !host.contains("apple-darwin");
    if macos_cross {
        println!(
            "cargo:warning=skipping the native NetEq library: no Apple SDK on a {host} host, \
             this build can check but not link"
        );
        return;
    }
    // TARGET is a full triple (aarch64-apple-darwin), not an OS name.
    let macos = target.contains("apple-darwin");
    let root = std::path::Path::new("vendor/webrtc_neteq");
    println!("cargo:rerun-if-changed=vendor/webrtc_neteq");
    println!("cargo:rerun-if-changed=src/media/audio/neteq_bridge.cc");
    let sources = std::fs::read_to_string(root.join("sources.txt")).expect("NetEq source manifest");
    let mut cpp = cc::Build::new();
    let mut c = cc::Build::new();
    for build in [&mut cpp, &mut c] {
        build
            .opt_level(3)
            .include(root)
            .define("NDEBUG", None)
            .define("RTC_DISABLE_LOGGING", None)
            .define("RTC_DISABLE_TRACE_EVENTS", None)
            .define("WEBRTC_APM_DEBUG_DUMP", "0")
            .define("WEBRTC_OPUS_SUPPORT_120MS_PTIME", "1")
            .warnings(false);
        if windows {
            build
                .define("WEBRTC_WIN", None)
                .define("NOMINMAX", None)
                .define("WIN32_LEAN_AND_MEAN", None);
        } else if macos {
            build.define("WEBRTC_POSIX", None).define("WEBRTC_MAC", None);
        } else {
            build
                .define("WEBRTC_POSIX", None)
                .define("WEBRTC_LINUX", None);
        }
    }
    cpp.cpp(true).std("c++17");
    if windows {
        cpp.flag_if_supported("/EHsc")
            .flag_if_supported("/Zc:__cplusplus");
    }
    cpp.file("src/media/audio/neteq_bridge.cc");
    for source in sources
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        if source.ends_with(".cc") {
            cpp.file(root.join(source));
        } else if source.ends_with(".c") {
            c.file(root.join(source));
        }
    }
    cpp.compile("openuuyc_neteq");
    c.compile("openuuyc_neteq_dsp");
    if windows {
        println!("cargo:rustc-link-lib=winmm");
        println!("cargo:rustc-link-lib=ws2_32");
    } else if macos {
        println!("cargo:rustc-link-lib=c++");
    } else {
        println!("cargo:rustc-link-lib=stdc++");
    }
}
