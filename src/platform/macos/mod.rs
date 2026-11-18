//! macOS backends.
//!
//! Only the host terminal is here so far. The rest of the platform (capture,
//! encoders, input injection, virtual audio) is still Windows/Linux only, so
//! the crate does not build for macOS end to end yet; the terminal is the
//! first piece to be cross-platform because it needs nothing but a pty.

pub(crate) mod terminal;
