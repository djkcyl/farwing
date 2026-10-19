//! Windows x64 NVENC ABI. No CUDA or NVDEC dependency or static driver linking.
#![allow(warnings)]
#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
compile_error!("Farwing NVENC bindings require Windows x64");
pub mod sys {
    mod guid;
    mod version;
    mod windows_sys {
        pub mod nvEncodeAPI;
    }
    pub use windows_sys::nvEncodeAPI;
}
pub use sys::nvEncodeAPI::*;
