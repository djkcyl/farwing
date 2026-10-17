//! POSIX backends shared by Linux and macOS.
//!
//! The two systems agree on the pty contract closely enough that the host
//! terminal is one implementation rather than two near-copies; the few places
//! they differ are `cfg`-gated at the call site and marked there.

pub(crate) mod terminal;
