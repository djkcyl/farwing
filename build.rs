fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").expect("Cargo target OS");
    let windows = target_os == "windows";
    assert!(
        windows || target_os == "linux",
        "Farwing has no build configuration for {target_os}"
    );
    if windows {
        println!("cargo:rerun-if-changed=assets/windows.rc");
        println!("cargo:rerun-if-changed=assets/windows.manifest");
        println!("cargo:rerun-if-changed=assets/icon.ico");
        println!("cargo:rerun-if-changed=assets/tray");
        embed_resource::compile_for("assets/windows.rc", ["Farwing"], embed_resource::NONE)
            .manifest_required()
            .expect("compile Windows application icon");
        bundle_drivers();
    }
    build_neteq(windows);
    build_fonts();
}

/// Signed driver packages are optional build inputs. Farwing does not ship
/// prebuilt driver binaries; a package is embedded only when all of its files
/// are present (see drivers/README.md), otherwise the installer reports that
/// this build has no driver package and the rest of the client is unaffected.
fn bundle_drivers() {
    use std::path::PathBuf;
    let output = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let mut bundled = false;
    for (kind, name, binary) in [
        ("display", "FarwingDisplay", "dll"),
        ("input", "FarwingInput", "dll"),
        ("audio", "FarwingAudio", "sys"),
    ] {
        let variable = format!("FARWING_{}_DRIVER_DIR", kind.to_ascii_uppercase());
        println!("cargo:rerun-if-env-changed={variable}");
        let source = format!("drivers/{kind}/{name}.inf");
        println!("cargo:rerun-if-changed={source}");
        let directory = std::env::var_os(&variable)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("assets/drivers").join(kind));
        let files: Vec<(String, PathBuf)> = ["inf", binary, "cat"]
            .iter()
            .map(|extension| {
                let file = format!("{name}.{extension}");
                let path = directory.join(&file);
                println!("cargo:rerun-if-changed={}", path.display());
                (file, path)
            })
            .collect();
        let present = files.iter().filter(|(_, path)| path.is_file()).count();
        assert!(
            present == 0 || present == files.len(),
            "incomplete {kind} driver package in {}: expected {name}.inf/.{binary}/.cat",
            directory.display()
        );
        let mut generated = String::from("pub(super) const FILES: &[(&str, &[u8])] = &[\n");
        if present == files.len() {
            bundled = true;
            let inf_source = std::fs::read_to_string(&source).expect("read driver INF source");
            // The audio package is localized and must be UTF-16LE with a BOM
            // (drivers/audio/build.ps1); the others copy the source unchanged.
            let inf: Vec<u8> = if kind == "audio" {
                std::iter::once(0xfeffu16)
                    .chain(inf_source.encode_utf16())
                    .flat_map(u16::to_le_bytes)
                    .collect()
            } else {
                inf_source.into_bytes()
            };
            for (file, path) in &files {
                let bytes = std::fs::read(path).expect("read bundled driver package");
                assert!(!bytes.is_empty(), "{kind} driver package contains an empty file");
                if file.ends_with(".inf") {
                    assert_eq!(
                        bytes, inf,
                        "{kind} driver INF differs from {source}; rebuild the package"
                    );
                }
                std::fs::write(output.join(file), bytes).unwrap();
                generated.push_str(&format!(
                    "(\"{file}\", include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{file}\"))),\n"
                ));
            }
        }
        generated.push_str("];\n");
        std::fs::write(output.join(format!("{kind}_driver.rs")), generated).unwrap();
    }
    // Public certificate of the key that signed the driver catalogs. Without
    // it the packages must chain to a certificate Windows already trusts.
    println!("cargo:rerun-if-env-changed=FARWING_DRIVER_CERTIFICATE");
    let certificate = std::env::var_os("FARWING_DRIVER_CERTIFICATE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets/drivers/FarwingDrivers.cer"));
    println!("cargo:rerun-if-changed={}", certificate.display());
    let generated = if certificate.is_file() {
        use sha1::Digest;
        let bytes = std::fs::read(&certificate).expect("read driver signing certificate");
        assert!(
            bytes.first() == Some(&0x30),
            "driver signing certificate must be DER encoded (Export-Certificate -Type CERT)"
        );
        let thumbprint: String = sha1::Sha1::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect();
        std::fs::write(output.join("FarwingDrivers.cer"), bytes).unwrap();
        format!(
            "pub(super) const CERTIFICATE: Option<(&str, &[u8])> = Some((\"{thumbprint}\", include_bytes!(concat!(env!(\"OUT_DIR\"), \"/FarwingDrivers.cer\"))));\n"
        )
    } else {
        if bundled {
            println!(
                "cargo:warning=driver packages are bundled without assets/drivers/FarwingDrivers.cer; their signer must already be trusted by Windows"
            );
        }
        "pub(super) const CERTIFICATE: Option<(&str, &[u8])> = None;\n".into()
    };
    std::fs::write(output.join("driver_certificate.rs"), generated).unwrap();
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
    cpp.compile("farwing_neteq");
    c.compile("farwing_neteq_dsp");
    if windows {
        println!("cargo:rustc-link-lib=winmm");
        println!("cargo:rustc-link-lib=ws2_32");
    }
}
