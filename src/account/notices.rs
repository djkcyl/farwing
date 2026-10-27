//! Notifications pushed to this device (`docs/notifications.md`): only
//! that some arrived; the device centre reads them from the server.

use std::sync::atomic::{AtomicU64, Ordering};

static PUSHES: AtomicU64 = AtomicU64::new(0);

/// A `notice` push arrived (or the background service received one).
pub(crate) fn pushed() {
    PUSHES.fetch_add(1, Ordering::Relaxed);
}

/// Pushes so far: a change means there is something new to read.
pub(crate) fn pushes() -> u64 {
    PUSHES.load(Ordering::Relaxed)
}
