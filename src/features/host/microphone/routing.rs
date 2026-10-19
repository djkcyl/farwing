//! Default-device COM calls and recovery journal IO never run in the PCM feeder.
use super::{Shared, lock};
use crate::platform::windows::virtual_audio::Routing;
use anyhow::{Context, Result};
use std::{
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

/// Routes the default audio devices while any participant is permitted to
/// use them; one worker serves every participant of the host.
pub(super) struct Worker {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Worker {
    pub fn new(participants: Arc<Mutex<Vec<Arc<Shared>>>>) -> Result<Self> {
        let (stop, stopped) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("audio-default-devices".into())
            .spawn(move || {
                let mut defaults = Routing::default();
                let mut last = None;
                let mut retry = Instant::now();
                loop {
                    let permitted = lock(&participants)
                        .iter()
                        .find(|shared| shared.permitted())
                        .cloned();
                    match permitted {
                        None => {
                            defaults = Routing::default();
                            last = None;
                        }
                        Some(shared) if Instant::now() >= retry => {
                            let selected = shared.lease.audio_defaults();
                            let result = if last != Some(selected) {
                                defaults.apply(selected.speakers(), selected.microphone())
                            } else {
                                defaults.maintain()
                            };
                            let error = match result {
                                Ok(()) => {
                                    last = Some(selected);
                                    None
                                }
                                Err(error) => {
                                    retry = Instant::now() + Duration::from_secs(1);
                                    Some(format!("默认音频设备调整失败：{error:#}"))
                                }
                            };
                            for shared in lock(&participants).iter() {
                                let mut current = lock(&shared.routing_error);
                                if *current != error {
                                    current.clone_from(&error);
                                    drop(current);
                                    shared.republish();
                                }
                            }
                        }
                        Some(_) => {}
                    }
                    if !matches!(
                        stopped.recv_timeout(Duration::from_millis(20)),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        break;
                    }
                }
                drop(defaults);
            })
            .context("启动默认音频设备管理线程")?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
