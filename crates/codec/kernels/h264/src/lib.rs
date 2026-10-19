// SPDX-License-Identifier: LGPL-2.1-or-later
//! Platform-independent H.264 decoding and low-delay desktop encoding.
//! Internal kernel of farwing-codec; AVC syntax is also used by GPU decoding.
#![deny(unsafe_code)]

pub mod bits;
pub mod cabac;
mod cavlc;
mod dpb;
pub mod dsp;
pub mod encoder;
mod entropy;
mod fault;
pub mod headers;
pub mod syntax;
pub use fault::Fault;
mod order;
pub mod picture;
#[cfg(feature = "profile")]
pub mod profile;
pub mod reconstruct;
mod residual;
mod scan;
pub mod stream;
mod tables;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Truncated,
    Invalid(Fault),
    Unsupported(Fault),
    Allocation,
    NeedKeyframe,
    Cancelled,
    Closed,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(fault) => write!(f, "invalid input: {}", fault.message()),
            Self::Unsupported(fault) => write!(f, "unsupported: {}", fault.message()),
            _ => write!(f, "{self:?}"),
        }
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
