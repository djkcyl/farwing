//! Bringing a host online, the same in the device centre, the background
//! host and the command line. The host's capture and encoders are probed
//! first (a cold probe takes about 10 s), so a first session does not race
//! the probe against the controller's connection deadline: the host goes
//! online once the probe ends, or after [`ONLINE_AFTER`] at the latest.
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// The longest the probe holds a host back from going online.
pub(crate) const ONLINE_AFTER: Duration = Duration::from_secs(30);

pub(crate) struct Probe {
    cancel: CancellationToken,
    task: Option<JoinHandle<anyhow::Result<()>>>,
    due: Instant,
}

impl Probe {
    pub(crate) fn start(host: &crate::features::host::Handle) -> Self {
        let cancel = CancellationToken::new();
        let (host, token) = (host.clone(), cancel.clone());
        Self {
            cancel,
            task: Some(tokio::spawn(async move { host.prepare_startup(token).await })),
            due: Instant::now() + ONLINE_AFTER,
        }
    }
    /// The probe's outcome, once, when it has ended.
    pub(crate) async fn ended(&mut self) -> Option<anyhow::Result<()>> {
        if !self.task.as_ref().is_some_and(JoinHandle::is_finished) {
            return None;
        }
        let task = self.task.take()?;
        Some(task.await.unwrap_or_else(|error| Err(error.into())))
    }
    /// Still probing.
    pub(crate) fn running(&self) -> bool {
        self.task.is_some()
    }
    /// The host may go online: the probe ended, or took too long.
    pub(crate) fn online(&self) -> bool {
        self.task.is_none() || Instant::now() >= self.due
    }
    /// Stop probing. The probe releases its GPU and COM objects on its own
    /// thread; this waits for that rather than abandoning a blocking task.
    pub(crate) async fn cancel(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
