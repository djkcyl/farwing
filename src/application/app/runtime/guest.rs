//! Login-page host lifetime, independent of QR/SMS attempt lifetimes.
use super::*;
use crate::session::{device_session::DeviceHandle, host_client::HostClient};

pub(super) struct Guest {
    pub client: Option<HostClient>,
    presence: Option<ActivePresence>,
    /// Encoder probing, started with the guest host (see `poll`).
    media: Option<(tokio_util::sync::CancellationToken, tokio::task::JoinHandle<()>)>,
    shown: Option<u64>,
    retry_at: Instant,
    blocked: bool,
}
impl Default for Guest {
    fn default() -> Self {
        Self {
            client: None,
            presence: None,
            media: None,
            shown: None,
            retry_at: Instant::now(),
            blocked: false,
        }
    }
}
impl Guest {
    pub async fn close(&mut self) {
        if let Some((cancel, task)) = self.media.take() {
            cancel.cancel();
            let _ = task.await;
        }
        if let Some(client) = self.client.take() {
            client.retire();
            stop_active_signal(&mut self.presence).await;
            client.close().await;
        }
        self.shown = None;
    }
    pub async fn poll(&mut self, device: DeviceHandle, generation: u64, events: &Sender<GuiEvent>) {
        if self.presence.as_ref().is_some_and(|p| p.task.is_finished()) {
            let result = self.presence.take().unwrap().task.await;
            self.blocked = result
                .as_ref()
                .ok()
                .and_then(|r| r.as_ref().err())
                .and_then(|e| e.downcast_ref::<crate::transport::signal::SignalFailure>())
                .is_some_and(|e| matches!(e, crate::transport::signal::SignalFailure::Kicked(_)));
            if let Some(client) = &self.client {
                client.host.remote_failed(
                    if self.blocked {
                        "已被服务器断开"
                    } else {
                        "正在重新连接服务器"
                    }
                    .into(),
                );
            }
            if let Ok(Err(error)) = result {
                tracing::warn!(%error,"guest presence ended");
            }
            self.close().await;
            self.retry_at = Instant::now() + std::time::Duration::from_secs(3);
        }
        if self.client.is_none() && !self.blocked && Instant::now() >= self.retry_at {
            match HostClient::signed_out(device) {
                Ok(client) => {
                    tracing::info!(serverless = client.is_local(), "guest host starting");
                    // Probing the encoders takes seconds; do it now, as the
                    // signed-in device centre does at startup, so the first
                    // assistance does not run into the connection deadline.
                    let cancel = tokio_util::sync::CancellationToken::new();
                    let host = client.host.clone();
                    let task_cancel = cancel.clone();
                    self.media = Some((
                        cancel,
                        tokio::spawn(async move {
                            if let Err(error) = host.prepare_startup(task_cancel).await {
                                tracing::warn!(error = %format!("{error:#}"), "guest host media preparation failed");
                            }
                        }),
                    ));
                    self.presence = Some(ActivePresence::start(client.clone()));
                    self.client = Some(client);
                }
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "guest host could not start");
                    let _ = events.send(GuiEvent::Warning(format!("本机被控准备失败：{error:#}")));
                    self.retry_at = Instant::now() + std::time::Duration::from_secs(3);
                }
            }
        }
        if let Some(client) = &self.client {
            client.host.assistance.touch_ui();
            if self.shown != Some(generation) {
                let _ = events.send(GuiEvent::Host(generation, client.host.clone()));
                self.shown = Some(generation);
            }
        }
        if let Some(presence) = &self.presence {
            while let Ok(event) = presence.events.try_recv() {
                if let PresenceEvent::Warning(error) = event {
                    tracing::warn!(%error, "guest presence");
                    if let Some(client) = &self.client {
                        client.host.remote_failed(error);
                    }
                }
            }
        }
    }
}
