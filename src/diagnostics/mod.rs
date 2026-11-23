//! Diagnostics ownership and module boundaries.

pub mod logging;
pub mod performance;

#[cfg(windows)]
pub(crate) mod bundle;
#[cfg(windows)]
pub(crate) mod remote;
