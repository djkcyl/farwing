//! Shared AV1 sequence/frame syntax for the software core and DXVA submission.
//! The only input envelope is bounded low-overhead OBUs; no file/container IO.
mod bitreader;
mod helpers;
mod parser;
mod reader;
pub use parser::*;

mod warp;
