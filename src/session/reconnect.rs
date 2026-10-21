//! Waits before the next attempt to reach the server, kept in one place so
//! the device centre can show when the next attempt is due and start it at
//! once ("立即重连").
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

static WAITS: Mutex<Vec<(u64, Instant)>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(0);
static NOW: Notify = Notify::const_new();
/// The next attempt of the background service, in service mode.
static REMOTE: Mutex<Option<Instant>> = Mutex::new(None);

fn waits() -> std::sync::MutexGuard<'static, Vec<(u64, Instant)>> {
    WAITS.lock().unwrap_or_else(|e| e.into_inner())
}

struct Pending(u64);
impl Drop for Pending {
    fn drop(&mut self) {
        waits().retain(|(id, _)| *id != self.0);
    }
}

/// Wait `delay` before trying the server again, or less if [`now`] is called.
pub(crate) async fn wait(delay: Duration) {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    // Registered before it is polled, so a `now()` from here on is not missed.
    let woken = NOW.notified();
    waits().push((id, Instant::now() + delay));
    let _pending = Pending(id);
    tokio::select! {
        _ = tokio::time::sleep(delay) => {}
        _ = woken => {}
    }
}

/// When the earliest pending attempt is due; `None` while none is waiting.
pub(crate) fn next() -> Option<Instant> {
    let local = waits().iter().map(|(_, at)| *at).min();
    let remote = *REMOTE.lock().unwrap_or_else(|e| e.into_inner());
    local.into_iter().chain(remote).min()
}

/// Start every waiting attempt now.
pub(crate) fn now() {
    NOW.notify_waiters();
}

/// The background service's next attempt, from its snapshot.
pub(crate) fn set_remote(at: Option<Instant>) {
    *REMOTE.lock().unwrap_or_else(|e| e.into_inner()) = at;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn now_ends_the_wait_and_clears_it() {
        let waiting = tokio::spawn(wait(Duration::from_secs(30)));
        while next().is_none() {
            tokio::task::yield_now().await;
        }
        now();
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("woken at once")
            .unwrap();
        assert!(next().is_none());
    }
}
