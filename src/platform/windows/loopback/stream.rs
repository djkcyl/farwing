//! The WASAPI reader owns its COM apartment; endpoint management cannot stall it.
use super::{Devices, Endpoint, Samples};
use anyhow::{Context, Result};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

struct State {
    stopped: AtomicBool,
    producing: AtomicBool,
    error: Mutex<Option<String>>,
}
pub(crate) struct Capture {
    state: Arc<State>,
    thread: Option<std::thread::JoinHandle<()>>,
}
pub(crate) fn open(endpoint: &Endpoint, samples: Samples) -> Result<Capture> {
    let id = endpoint.id.clone();
    let state = Arc::new(State {
        stopped: AtomicBool::new(false),
        producing: AtomicBool::new(false),
        error: Mutex::new(None),
    });
    let shared = state.clone();
    let (ready, initialized) = mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("desktop-audio-capture".into())
        .spawn(move || {
            let opened = (|| -> Result<_> {
                let devices = Devices::new()?;
                let endpoint = devices.endpoint(Some(&id))?.context("播放设备已断开")?;
                super::open_native(&endpoint, samples)
            })();
            let mut capture = match opened {
                Ok(capture) => {
                    if ready.send(Ok(())).is_err() {
                        return;
                    }
                    capture
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                    return;
                }
            };
            while !shared.stopped.load(Ordering::Acquire) {
                if let Err(error) = capture.poll() {
                    *shared.error.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(format!("{error:#}"));
                    break;
                }
                shared
                    .producing
                    .store(capture.producing(), Ordering::Release);
                std::thread::park_timeout(Duration::from_millis(5));
            }
            shared.producing.store(false, Ordering::Release);
        })
        .context("启动桌面音频取样线程")?;
    let owner = Capture {
        state,
        thread: Some(thread),
    };
    initialized.recv().context("桌面音频取样线程初始化中断")??;
    Ok(owner)
}
impl Capture {
    pub fn producing(&self) -> bool {
        self.state.producing.load(Ordering::Acquire)
    }
    pub fn poll(&mut self) -> Result<()> {
        if let Some(error) = self
            .state
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            anyhow::bail!("{error}");
        }
        if self.thread.as_ref().is_some_and(|t| t.is_finished()) {
            anyhow::bail!("桌面音频取样线程已停止");
        }
        Ok(())
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.state.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}
