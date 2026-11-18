//! NVENC ABI for x86-64 (Windows and Linux) and aarch64 Linux. No CUDA or
//! NVDEC dependency or static driver linking: the driver library is loaded at
//! run time.
//!
//! The bindings were generated for Windows x64; the header's types are fixed
//! width except `GUID.Data1` and `RECT`, which use C `long` and are declared
//! here with the 32-bit width both platforms' headers mean. aarch64 Linux is
//! LP64 with the same natural alignment as x86-64, so the layout carries over.
#![allow(warnings)]
#[cfg(not(any(
    all(any(target_os = "windows", target_os = "linux"), target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64")
)))]
compile_error!("OpenUUYC NVENC bindings require Windows or Linux on x86-64, or Linux on aarch64");
pub mod sys {
    mod guid;
    mod version;
    mod windows_sys {
        pub mod nvEncodeAPI;
    }
    pub use windows_sys::nvEncodeAPI;
}
pub use sys::nvEncodeAPI::*;
