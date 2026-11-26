//! Linux host terminal: the shared POSIX pty.
//!
//! The implementation lives in [`crate::platform::posix::terminal`] because
//! macOS runs the same one; this keeps the per-platform path stable and is
//! where a Linux-only divergence would go.

pub(crate) use crate::platform::posix::terminal::*;
