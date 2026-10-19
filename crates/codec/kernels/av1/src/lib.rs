//! AV1 bitstream syntax and software decoder.
//! Component provenance and licenses are retained under licenses/.
#[cfg(test)]
#[macro_use]
extern crate pretty_assertions;

pub mod bitstream;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub mod decoder;
