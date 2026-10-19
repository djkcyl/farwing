//! Codec syntax and reference metadata shared by the active decoding paths.
#![forbid(unsafe_code)]

pub mod bitstream;
pub mod forward;
pub mod nal;
pub mod poc;
pub mod pps;
pub mod ref_list;
pub mod scaling_list;
pub mod slice_header;
pub mod sps;
pub mod transform;
pub mod vui;
