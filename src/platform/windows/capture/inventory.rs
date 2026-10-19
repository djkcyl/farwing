//! Read display inventory away from the capture deadline. One outstanding read;
//! the capture owner consumes changes and owns both cancellation and joining.
use super::Screen;
use anyhow::{Context, Result};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};

pub(super) struct Refresh {
    requests: Option<SyncSender<Screen>>,
    results: Receiver<Result<Screen>>,
    thread: Option<std::thread::JoinHandle<()>>,
    pending: bool,
}
impl Refresh {
    pub fn new() -> Result<Self> {
        let (requests, incoming) = mpsc::sync_channel::<Screen>(1);
        let (completed, results) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("capture-inventory".into())
            .spawn(move || {
                while let Ok(screen) = incoming.recv() {
                    let result = super::refresh(&screen);
                    if completed.try_send(result).is_err() {
                        break;
                    }
                }
            })
            .context("启动显示器状态读取线程")?;
        Ok(Self {
            requests: Some(requests),
            results,
            thread: Some(thread),
            pending: false,
        })
    }
    pub fn request(&mut self, screen: &Screen) -> Result<()> {
        if !self.pending {
            self.requests
                .as_ref()
                .context("显示器状态读取已停止")?
                .try_send(screen.clone())
                .context("提交显示器状态读取")?;
            self.pending = true;
        }
        Ok(())
    }
    pub fn poll(&mut self) -> Result<Option<Result<Screen>>> {
        match self.results.try_recv() {
            Ok(result) => {
                self.pending = false;
                Ok(Some(result))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => anyhow::bail!("显示器状态读取线程已结束"),
        }
    }
}
impl Drop for Refresh {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
