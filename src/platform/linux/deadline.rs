//! On-demand wakeup at a deadline, for mouse-motion throttling. Linux has no
//! thread-pool timer to borrow, so each enabled throttle owns one parked thread
//! that waits on a condition variable until the armed deadline.
use anyhow::{Context as _, Result};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;
use tokio::sync::Notify;

#[derive(Default)]
struct State {
    deadline: Option<Instant>,
    stop: bool,
}
pub(crate) struct Timer {
    shared: Arc<(Mutex<State>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn lock(shared: &(Mutex<State>, Condvar)) -> std::sync::MutexGuard<'_, State> {
    shared.0.lock().unwrap_or_else(|p| p.into_inner())
}

impl Timer {
    pub fn new(wake: Arc<Notify>) -> Result<Self> {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let state = shared.clone();
        let thread = std::thread::Builder::new()
            .name("mouse-deadline".into())
            .spawn(move || {
                let mut s = lock(&state);
                loop {
                    if s.stop {
                        break;
                    }
                    match s.deadline {
                        None => s = state.1.wait(s).unwrap_or_else(|p| p.into_inner()),
                        Some(deadline) => {
                            let now = Instant::now();
                            if now >= deadline {
                                s.deadline = None;
                                // A stale wakeup only makes the consumer recheck
                                // its current deadline and input epoch.
                                wake.notify_one();
                            } else {
                                s = state
                                    .1
                                    .wait_timeout(s, deadline - now)
                                    .unwrap_or_else(|p| p.into_inner())
                                    .0;
                            }
                        }
                    }
                }
            })
            .context("创建鼠标发送定时线程失败")?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }
    pub fn arm(&mut self, deadline: Instant) -> Result<()> {
        let mut s = lock(&self.shared);
        if s.deadline != Some(deadline) {
            s.deadline = Some(deadline);
            self.shared.1.notify_one();
        }
        Ok(())
    }
    pub fn disarm(&mut self) {
        lock(&self.shared).deadline = None;
        self.shared.1.notify_one();
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        lock(&self.shared).stop = true;
        self.shared.1.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn wakes_at_the_deadline_and_not_after_disarm() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let wake = Arc::new(Notify::new());
            let mut timer = Timer::new(wake.clone()).unwrap();
            let armed = Instant::now();
            timer.arm(armed + Duration::from_millis(5)).unwrap();
            tokio::time::timeout(Duration::from_secs(1), wake.notified())
                .await
                .expect("armed deadline wakes");
            let late = armed.elapsed();
            assert!(late >= Duration::from_millis(5), "woke early: {late:?}");
            assert!(late < Duration::from_millis(50), "woke late: {late:?}");
            timer
                .arm(Instant::now() + Duration::from_millis(5))
                .unwrap();
            timer.disarm();
            assert!(
                tokio::time::timeout(Duration::from_millis(30), wake.notified())
                    .await
                    .is_err(),
                "disarmed deadline must not wake"
            );
        });
    }
}
