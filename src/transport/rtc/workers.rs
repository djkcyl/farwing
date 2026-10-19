//! Connection-owned asynchronous worker cancellation and draining.
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub(super) struct SessionWorkers {
    pub(super) shutdown: CancellationToken,
    pub(super) tasks: StdMutex<Option<Vec<tokio::task::JoinHandle<()>>>>,
    pub(super) closing: Mutex<()>,
}

impl SessionWorkers {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            shutdown: CancellationToken::new(),
            tasks: StdMutex::new(Some(Vec::new())),
            closing: Mutex::new(()),
        })
    }

    pub(super) fn spawn(&self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        let _ = self.spawn_in(self.shutdown.clone(), future);
    }

    pub(super) fn spawn_in(
        &self,
        scope: CancellationToken,
        future: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Option<tokio::task::AbortHandle> {
        let mut tasks = std_mutex_lock(&self.tasks);
        if let Some(tasks) = tasks.as_mut() {
            let shutdown = self.shutdown.clone();
            let task = tokio::spawn(async move {
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => {},
                    _ = scope.cancelled() => {},
                    _ = future => {},
                }
            });
            let abort = task.abort_handle();
            tasks.push(task);
            Some(abort)
        } else {
            None
        }
    }

    pub(super) async fn close(&self) {
        self.shutdown.cancel();
        let _closing = self.closing.lock().await;
        let tasks = std_mutex_lock(&self.tasks).take().unwrap_or_default();
        let count = tasks.len();
        for task in tasks {
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                tracing::warn!(%error, "peer worker terminated unexpectedly");
            }
        }
        tracing::debug!(count, "all owned peer workers joined");
    }
}

impl Drop for SessionWorkers {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

pub(super) fn std_mutex_lock<T>(lock: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
