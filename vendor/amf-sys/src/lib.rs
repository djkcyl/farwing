//! AMD AMF 1.5.2 Windows x64 public C ABI, generated from the retained headers.
#![allow(warnings)]
#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
compile_error!("Farwing AMF bindings require Windows x64");
include!("bindings.rs");
include!("av1.rs");
