//! AV1 packet metadata and hardware decode syntax shared by streaming backends.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

mod bits;
pub mod obu;
mod sequence;
pub mod syntax;

pub use sequence::{Chroma, Sequence};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Truncated,
    Invalid,
    Limit,
    Unsupported(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Truncated => "truncated AV1 bitstream",
            Self::Invalid => "invalid AV1 syntax",
            Self::Limit => "AV1 bitstream limit exceeded",
            Self::Unsupported(reason) => reason,
        })
    }
}
impl std::error::Error for Error {}
type Result<T> = std::result::Result<T, Error>;
