use sha2::{Digest, Sha256};
use std::{os::windows::process::CommandExt, path::PathBuf};
fn main() {
    assert_eq!(
        std::env::var("CARGO_CFG_TARGET_ARCH").as_deref(),
        Ok("x86_64")
    );
    println!("cargo:rerun-if-env-changed=FARWING_PAYLOAD");
    let image = PathBuf::from(
        std::env::var_os("FARWING_PAYLOAD")
            .expect("Set FARWING_PAYLOAD to the native release Farwing.exe"),
    );
    let image = std::fs::canonicalize(image).expect("native payload path");
    println!("cargo:rerun-if-changed={}", image.display());
    println!("cargo:rerun-if-changed=../assets/icon.ico");
    println!("cargo:rerun-if-changed=../assets/windows.manifest");
    let bytes = std::fs::read(&image).expect("read native payload");
    assert!(
        bytes.starts_with(b"MZ"),
        "payload must be a Windows executable"
    );
    let protocol = std::process::Command::new(&image)
        .arg("--bootstrap-version")
        .creation_flags(0x08000000)
        .output()
        .expect("read bootstrap protocol");
    assert!(
        protocol.status.success() && protocol.stdout == b"1\n",
        "Native payload must support bootstrap protocol 1; rebuild the main application first"
    );
    let version = std::process::Command::new(&image)
        .arg("--version")
        .creation_flags(0x08000000)
        .output()
        .expect("read payload version");
    assert!(version.status.success(), "native version command failed");
    let version = String::from_utf8(version.stdout).expect("UTF-8 version");
    let version = version
        .trim()
        .strip_prefix("Farwing ")
        .expect("Farwing payload");
    let manifest = farwing_bootstrap::Manifest {
        schema: 1,
        version: version.into(),
        architecture: "x86_64".into(),
        bytes: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    };
    manifest.validate().unwrap();
    let packed = zstd::stream::encode_all(bytes.as_slice(), 19).expect("compress payload");
    assert_eq!(zstd::stream::decode_all(packed.as_slice()).unwrap(), bytes);
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("payload.zst"), packed).unwrap();
    std::fs::write(
        out.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let icon = std::fs::canonicalize("../assets/icon.ico")
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/");
    let rc = out.join("launcher.rc");
    let application = std::fs::canonicalize("../assets/windows.manifest")
        .expect("shared Windows application manifest");
    let application = application.to_string_lossy().replace('\\', "/");
    std::fs::write(&rc, format!("1 ICON \"{icon}\"\n1 24 \"{application}\"\n")).unwrap();
    embed_resource::compile(&rc, embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
