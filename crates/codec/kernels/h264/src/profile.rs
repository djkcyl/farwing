// SPDX-License-Identifier: LGPL-2.1-or-later
//! Separate-build, whole-pipeline diagnostics; never mixed into timing runs.
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};
static NS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
pub struct Timer {
    slot: usize,
    start: Instant,
}
impl Timer {
    pub fn new(slot: usize) -> Self {
        Self {
            slot,
            start: Instant::now(),
        }
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        NS[self.slot].fetch_add(self.start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}
pub fn report() {
    eprintln!(
        "CORE_PHASE_MS total,reconstruct,inter_prediction,planes,filter,parse_headers,pack={:?}",
        NS.each_ref()
            .map(|v| v.swap(0, Ordering::Relaxed) as f64 / 1e6)
    );
}
