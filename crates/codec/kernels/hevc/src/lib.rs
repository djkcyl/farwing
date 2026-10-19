//! Codec syntax and reference metadata shared by the active decoding paths.
#![forbid(unsafe_code)]

pub mod bitreader;
pub mod dpb;
pub mod hrd;
pub mod nal;
pub mod poc;
pub mod pps;
pub mod scaling_list;
pub mod scan;
pub mod slice;
pub mod sps;
pub mod vps;
pub mod vui;
