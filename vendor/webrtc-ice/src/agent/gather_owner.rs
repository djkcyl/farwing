//! Own gathering work and uncommitted sockets through final Agent teardown.
//! Stopping a generation is separate: it must not destroy established ports.
use std::future::Future;
use std::sync::{Arc, Mutex, Weak};

use tokio_util::{sync::CancellationToken, task::TaskTracker};
use util::Conn;

type Socket = dyn Conn + Send + Sync;

#[derive(Default)]
pub(super) struct GatherResources {
    sockets: Vec<Arc<Socket>>,
    relays: Vec<Arc<Socket>>,
    clients: Vec<Arc<turn::client::Client>>,
}

impl GatherResources {
    pub(super) async fn close(self) {
        for relay in self.relays {
            let _ = relay.close().await;
        }
        for client in self.clients {
            if let Err(error) = client.close().await {
                log::debug!("closing gathered TURN client: {error}");
            }
        }
        for socket in self.sockets {
            if let Err(error) = socket.close().await {
                log::debug!("closing gathered socket: {error}");
            }
        }
    }
}

#[derive(Default)]
struct Registry {
    sockets: Vec<Weak<Socket>>,
    relays: Vec<Weak<Socket>>,
    clients: Vec<Weak<turn::client::Client>>,
    closing: Option<GatherResources>,
}

#[derive(Default)]
pub(crate) struct GatherOwner {
    stop: CancellationToken,
    tasks: TaskTracker,
    registry: Mutex<Registry>,
}

impl Drop for GatherOwner {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl GatherOwner {
    pub(super) fn spawn<F>(&self, future: F)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let registry = self
            .registry
            .lock()
            .expect("gather registry mutex poisoned");
        if registry.closing.is_some() || self.stop.is_cancelled() {
            return;
        }
        let stop = self.stop.clone();
        self.tasks.spawn(async move {
            tokio::select! {
                biased;
                _ = stop.cancelled() => {},
                _ = future => {},
            }
        });
    }

    pub(super) fn socket(&self, socket: &Arc<Socket>) {
        let mut registry = self
            .registry
            .lock()
            .expect("gather registry mutex poisoned");
        if let Some(closing) = &mut registry.closing {
            closing.sockets.push(socket.clone());
        } else {
            registry.sockets.retain(|socket| socket.strong_count() != 0);
            let weak = Arc::downgrade(socket);
            if !registry.sockets.iter().any(|old| old.ptr_eq(&weak)) {
                registry.sockets.push(weak);
            }
        }
    }

    pub(super) fn client(&self, client: &Arc<turn::client::Client>) {
        let mut registry = self
            .registry
            .lock()
            .expect("gather registry mutex poisoned");
        if let Some(closing) = &mut registry.closing {
            closing.clients.push(client.clone());
        } else {
            registry.clients.retain(|client| client.strong_count() != 0);
            registry.clients.push(Arc::downgrade(client));
        }
    }

    pub(super) fn relay(&self, socket: &Arc<Socket>) {
        let mut registry = self
            .registry
            .lock()
            .expect("gather registry mutex poisoned");
        if let Some(closing) = &mut registry.closing {
            closing.relays.push(socket.clone());
        } else {
            registry.relays.retain(|socket| socket.strong_count() != 0);
            registry.relays.push(Arc::downgrade(socket));
        }
    }

    pub(super) fn begin_close(&self) {
        let mut registry = self
            .registry
            .lock()
            .expect("gather registry mutex poisoned");
        if registry.closing.is_none() {
            // Retain live clients before cancelling their construction futures,
            // so their readers can be explicitly joined rather than dropped.
            registry.closing = Some(GatherResources {
                sockets: std::mem::take(&mut registry.sockets)
                    .into_iter()
                    .filter_map(|s| s.upgrade())
                    .collect(),
                relays: std::mem::take(&mut registry.relays)
                    .into_iter()
                    .filter_map(|s| s.upgrade())
                    .collect(),
                clients: std::mem::take(&mut registry.clients)
                    .into_iter()
                    .filter_map(|c| c.upgrade())
                    .collect(),
            });
        }
        self.stop.cancel();
        self.tasks.close();
    }

    pub(super) async fn finish(&self) -> GatherResources {
        self.tasks.wait().await;
        log::debug!("all ICE gathering tasks joined");
        self.registry
            .lock()
            .expect("gather registry mutex poisoned")
            .closing
            .take()
            .unwrap_or_default()
    }
}
