//! Login-page host lifetime, independent of QR/SMS attempt lifetimes.
use super::*;
use crate::session::{device_session::DeviceHandle, host_client::HostClient};

pub(super) struct Guest {
    pub client: Option<HostClient>,
    presence: Option<ActivePresence>,
    shown: Option<u64>,
    retry_at: Instant,
    blocked: bool,
}
impl Default for Guest {
    fn default() -> Self {
        Self {
            client: None,
            presence: None,
            shown: None,
            retry_at: Instant::now(),
            blocked: false,
        }
    }
}
impl Guest {
    pub async fn close(&mut self) {
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
                .is_some_and(|e| matches!(e, crate::transport::signal::SignalFailure::Kicked));
            if let Some(client) = &self.client {
                client.host.remote_failed(
                    if self.blocked {
                        "游客在线状态已被服务端结束，请退出并重新打开程序"
                    } else {
                        "协助连接已结束，正在重新连接"
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
            match HostClient::guest(device) {
                Ok(client) => {
                    self.presence = Some(ActivePresence::start(client.clone()));
                    self.client = Some(client);
                }
                Err(error) => {
                    let _ = events.send(GuiEvent::Warning(format!("游客协助准备失败：{error:#}")));
                    self.retry_at = Instant::now() + std::time::Duration::from_secs(3);
                }
            }
        }
        if let Some(client) = &self.client {
            client.host.assistance.touch_ui();
            if self.shown != Some(generation) {
                let _ = events.send(GuiEvent::Host(generation, client.host.clone(), None));
                self.shown = Some(generation);
            }
        }
        if let Some(presence) = &self.presence {
            while let Ok(event) = presence.events.try_recv() {
                if let PresenceEvent::Warning(error) = event {
                    if let Some(client) = &self.client {
                        client.host.remote_failed(error);
                    }
                }
            }
        }
    }
}
