//! macOS host terminal: the shared POSIX pty.
//!
//! macOS and Linux agree on the pty contract, so the implementation is shared
//! rather than duplicated. Two things differ, and both live in the shared
//! module: the fallback login shell (`/bin/zsh` here, `/bin/bash` on Linux)
//! and `ptsname_r`, which macOS does not have.

pub(crate) use crate::platform::posix::terminal::*;
