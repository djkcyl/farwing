//! Shared, lifetime-owned settings IO. Rendering and keyboard hooks use memory.
use anyhow::{Context, Result};
use std::sync::{Arc, Mutex, OnceLock, Weak, mpsc};
use std::time::Duration;

pub(crate) struct Watcher {
    _owner: Arc<Owner>,
}
struct Owner {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Watcher {
    pub fn new() -> Result<Self> {
        static LIVE: OnceLock<Mutex<Weak<Owner>>> = OnceLock::new();
        let mut live = LIVE
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(owner) = live.upgrade() {
            return Ok(Self { _owner: owner });
        }
        // First load happens during window construction, before input is enabled.
        super::refresh();
        let (stop, stopped) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("shortcut-settings".into())
            .spawn(move || {
                while matches!(
                    stopped.recv_timeout(Duration::from_millis(500)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    super::refresh();
                }
            })
            .context("启动快捷键设置读取线程")?;
        let owner = Arc::new(Owner {
            stop,
            thread: Some(thread),
        });
        *live = Arc::downgrade(&owner);
        Ok(Self { _owner: owner })
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
