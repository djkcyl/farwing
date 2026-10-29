//! Default-device COM calls and recovery journal IO never run in the PCM feeder.
use super::{Shared, lock};
use crate::platform::windows::virtual_audio::Routing;
use crate::i18n::t;
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
                let mut defaults = Defaults::new();
                let mut last = None;
                let mut retry = Instant::now();
                loop {
                    let permitted = lock(&participants)
                        .iter()
                        .find(|shared| shared.permitted())
                        .cloned();
                    match permitted {
                        None => {
                            defaults.reset();
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
                                    Some((t().features.audio_defaults_failed)(&format!("{error:#}")))
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
            .context(t().features.audio_defaults_thread_failed)?;
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

/// The default-device routing: in this process, or, for the background
/// host, in the service (which keeps it current and gives the devices back
/// when this process goes).
enum Defaults {
    Local(Routing),
    Service(Option<(bool, bool)>),
}
impl Defaults {
    fn new() -> Self {
        if crate::platform::windows::host_service::resident::is_owner() {
            Self::Service(None)
        } else {
            Self::Local(Routing::default())
        }
    }
    fn apply(&mut self, speakers: bool, microphone: bool) -> Result<()> {
        match self {
            Self::Local(routing) => routing.apply(speakers, microphone),
            Self::Service(current) => {
                crate::platform::windows::host_service::requests::audio_routing(Some((speakers, microphone)))?;
                *current = Some((speakers, microphone));
                Ok(())
            }
        }
    }
    fn maintain(&mut self) -> Result<()> {
        match self {
            Self::Local(routing) => routing.maintain(),
            Self::Service(_) => Ok(()),
        }
    }
    fn reset(&mut self) {
        match self {
            Self::Local(routing) => *routing = Routing::default(),
            Self::Service(current) => {
                if current.take().is_some()
                    && let Err(error) = crate::platform::windows::host_service::requests::audio_routing(None)
                {
                    tracing::warn!(%error, "default audio devices not given back");
                }
            }
        }
    }
}
impl Drop for Defaults {
    fn drop(&mut self) {
        self.reset();
    }
}
