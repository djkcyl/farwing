//! Build only the x86-family decoder kernels used by the software backend.
use std::{env, fs, path::PathBuf};

pub(super) fn build() {
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    if arch != "x86" && arch != "x86_64" {
        return;
    }
    let x64 = arch == "x86_64";
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let apple = env::var("CARGO_CFG_TARGET_VENDOR").unwrap() == "apple";
    let features = env::var("CARGO_CFG_TARGET_FEATURE").unwrap();
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap()).join("decoder");
    fs::create_dir_all(&out_dir).unwrap();
    let mut config = String::from(
        "%define CONFIG_ASM 1\n%define CONFIG_LOG 1\n%define private_prefix dav1d\n%define PIC 1\n",
    );
    if apple || (os == "windows" && !x64) {
        config.push_str("%define PREFIX 1\n");
    }
    config.push_str(&format!(
        "%define ARCH_X86_32 {}\n%define ARCH_X86_64 {}\n%define STACK_ALIGNMENT {}\n%define FORCE_VEX_ENCODING {}\n",
        u8::from(!x64), u8::from(x64),
        if x64 || os == "linux" || apple { 16 } else { 4 },
        u8::from(features.split(',').any(|f| f == "avx")),
    ));
    fs::write(out_dir.join("config.asm"), config).unwrap();

    // Note that avx* is never (at runtime) supported on x86.
    let x86_generic = &["cdef_sse", "itx_sse", "msac", "pal", "refmvs"][..];
    let x86_64_generic = &["cdef_avx2", "itx_avx2", "itx_avx512"][..];
    let x86_bpc8 = &[
        "ipred_sse",
        "loopfilter_sse",
        "looprestoration_sse",
        "mc_sse",
    ][..];
    let x86_64_bpc8 = &[
        "cdef_avx512",
        "ipred_avx2",
        "ipred_avx512",
        "loopfilter_avx2",
        "loopfilter_avx512",
        "looprestoration_avx2",
        "looprestoration_avx512",
        "mc_avx2",
        "mc_avx512",
    ][..];
    let x86_bpc16 = &[
        "cdef16_sse",
        "ipred16_sse",
        "itx16_sse",
        "loopfilter16_sse",
        "looprestoration16_sse",
        "mc16_sse",
        // TODO(kkysen) avx2 shouldn't be in x86,
        // but a const used in sse is defined in avx2 (a bug).
        "ipred16_avx2",
    ][..];
    let x86_64_bpc16 = &[
        "cdef16_avx2",
        "cdef16_avx512",
        // TODO(kkysen) avx2 should only be in x86_64,
        // but a const used in sse is defined in avx2 (a bug).
        // "ipred16_avx2",
        "ipred16_avx512",
        "itx16_avx2",
        "itx16_avx512",
        "loopfilter16_avx2",
        "loopfilter16_avx512",
        "looprestoration16_avx2",
        "looprestoration16_avx512",
        "mc16_avx2",
        "mc16_avx512",
    ][..];

    let x86_all = &[
        x86_generic,
        #[cfg(feature = "bitdepth_8")]
        x86_bpc8,
        #[cfg(feature = "bitdepth_16")]
        x86_bpc16,
    ][..];
    let x86_64_all = &[
        x86_generic,
        x86_64_generic,
        #[cfg(feature = "bitdepth_8")]
        x86_bpc8,
        #[cfg(feature = "bitdepth_8")]
        x86_64_bpc8,
        #[cfg(feature = "bitdepth_16")]
        x86_bpc16,
        #[cfg(feature = "bitdepth_16")]
        x86_64_bpc16,
    ][..];
    let files = if x64 { x86_64_all } else { x86_all };
    let files = files
        .iter()
        .flat_map(|a| *a)
        .map(|name| PathBuf::from(format!("src/decoder/x86/{name}.asm")));
    let mut nasm = nasm_rs::Build::new();
    nasm.out_dir(&out_dir).min_version(2, 14, 0).files(files);
    if cfg!(debug_assertions) {
        nasm.flag("-g");
        if os != "windows" {
            nasm.flag("-Fdwarf");
        }
    }
    nasm.flag(&format!("-I{}/", out_dir.display()));
    nasm.flag("-Isrc/decoder/");
    let objects = nasm.compile_objects().unwrap_or_else(|e| {
        panic!("NASM build failed. Install NASM or disable the asm feature: {e}");
    });
    let mut archive = cc::Build::new();
    archive.out_dir(&out_dir);
    for object in objects {
        archive.object(object);
    }
    archive.compile("rav1dasm");
}
