//! What the hosting process tells the server for connection records and
//! account events (`docs/connection-records.md`): its summaries of what
//! each participant did, and changes of this device's access settings.
//! They wait here until the host's account client uploads them; a host
//! without an account (a guest, or direct connections only) keeps none.

use crate::account::api::ApiFailure;
use crate::account::client::AuthenticatedClient;
use farwing_protocol::api::{DeviceEvent, RecordReport};
use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Items kept while the server cannot be reached.
const MAX_WAITING: usize = 200;
/// A failed upload is tried again after this long.
const RETRY: Duration = Duration::from_secs(60);

pub(crate) enum Item {
    /// The host's summary; the uploader fills in this device's ID.
    Report(RecordReport),
    Event(DeviceEvent),
}

static WAITING: LazyLock<Mutex<VecDeque<Item>>> = LazyLock::new(Default::default);
static WAKE: LazyLock<Notify> = LazyLock::new(Notify::new);

fn waiting() -> std::sync::MutexGuard<'static, VecDeque<Item>> {
    WAITING.lock().unwrap_or_else(|p| p.into_inner())
}

/// Queue an item for the server.
pub(crate) fn submit(item: Item) {
    let mut queue = waiting();
    if queue.len() >= MAX_WAITING {
        queue.pop_front();
    }
    queue.push_back(item);
    drop(queue);
    WAKE.notify_one();
}

/// A change of this device's access settings (`device_event`).
pub(crate) fn device_event(kind: &str, device_id: &str, device_name: &str) {
    submit(Item::Event(DeviceEvent {
        kind: kind.to_owned(),
        device_id: device_id.to_owned(),
        device_name: device_name.to_owned(),
    }));
}

async fn send(client: &AuthenticatedClient, item: &Item) -> anyhow::Result<()> {
    match item {
        Item::Report(report) => {
            let mut report = report.clone();
            if report.host_device_id.is_empty() {
                report.host_device_id = client.device_id().to_owned();
            }
            client
                .request(|api| async move { api.record_report(&report).await })
                .await
        }
        Item::Event(event) => {
            client
                .request(|api| async move { api.device_event(event).await })
                .await
        }
    }
}

/// Upload what is waiting until `cancel`: at once when something arrives,
/// and again after a failure.
pub(crate) async fn upload(client: &AuthenticatedClient, cancel: &CancellationToken) {
    loop {
        loop {
            let Some(item) = waiting().pop_front() else {
                break;
            };
            match send(client, &item).await {
                Ok(()) => {}
                Err(error)
                    if error
                        .downcast_ref::<ApiFailure>()
                        .is_none_or(|failure| failure.code.retryable()) =>
                {
                    tracing::debug!(%error, "connection record waits for the server");
                    waiting().push_front(item);
                    break;
                }
                Err(error) => tracing::info!(%error, "connection record refused"),
            }
        }
        tokio::select! {
            _ = WAKE.notified() => {}
            _ = tokio::time::sleep(RETRY) => {}
            _ = cancel.cancelled() => return,
        }
    }
}
