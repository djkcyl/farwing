//! Counts owned work until native teardown has completed, including detached joins.
use std::sync::atomic::{AtomicUsize, Ordering};
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
pub(crate) struct Work;
impl Work {
    pub fn new() -> Self {
        ACTIVE.fetch_add(1, Ordering::AcqRel);
        Self
    }
}
impl Drop for Work {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::AcqRel);
    }
}
pub(crate) fn idle() -> bool {
    ACTIVE.load(Ordering::Acquire) == 0
}
