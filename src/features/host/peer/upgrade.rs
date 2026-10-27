//! Local installer notification over the existing, reliable TEXT channel.
use super::ReportTarget;
use anyhow::{Context, Result, ensure};
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

pub(crate) struct UpdateNotice {
    routes: ReportTarget,
    cancel: CancellationToken,
    completed: tokio::sync::Mutex<bool>,
}
impl UpdateNotice {
    pub(super) fn new(routes: ReportTarget, cancel: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            routes,
            cancel,
            completed: tokio::sync::Mutex::new(false),
        })
    }
    pub(crate) async fn send(&self) -> Result<()> {
        let mut completed = self.completed.lock().await;
        if *completed {
            return Ok(());
        }
        let operation = async {
            ensure!(!self.cancel.is_cancelled(), "被控连接已结束");
            let channel = self
                .routes
                .borrow()
                .text
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
                .context("更新通知通道尚未就绪")?;
            ensure!(
                channel.ordered()
                    && channel.max_packet_lifetime().is_none()
                    && channel.max_retransmits().is_none(),
                "更新通知需要已就绪的可靠有序通道"
            );
            match channel.ready_state() {
                RTCDataChannelState::Open => {}
                RTCDataChannelState::Closing | RTCDataChannelState::Closed => {
                    // A TEXT close alone is not proof that the media session ended.
                    self.cancel.cancelled().await;
                    return Ok(false);
                }
                _ => anyhow::bail!("更新通知通道尚未就绪"),
            }
            let bytes = crate::features::stream_control::publisher::update_started();
            if let Err(error) = channel.send(&bytes::Bytes::from(bytes)).await {
                if matches!(
                    channel.ready_state(),
                    RTCDataChannelState::Closing | RTCDataChannelState::Closed
                ) {
                    self.cancel.cancelled().await;
                    return Ok(false);
                }
                return Err(error.into());
            }
            // In our SCTP implementation this counter is released by SACK, not
            // merely when send() queues the message. Keep the connection alive
            // until delivery drains; do not replay this notice over another carrier.
            loop {
                if channel.buffered_amount().await == 0 {
                    return Ok(true);
                }
                if matches!(
                    channel.ready_state(),
                    RTCDataChannelState::Closing | RTCDataChannelState::Closed
                ) {
                    self.cancel.cancelled().await;
                    return Ok(false);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::pin!(operation);
        let result = tokio::select! {
            biased;
            _=self.cancel.cancelled()=>Ok(false),
            result=&mut operation=>result,
            _=tokio::time::sleep(Duration::from_secs(3))=>Err(anyhow::anyhow!("更新通知未在限定时间内确认")),
        };
        // A timed-out enqueue may have submitted SCTP fragments. Retire this
        // connection before dropping that future; never resume/retry a partial
        // reliable message on the same stream. The installer still waits for
        // actual host/process shutdown before replacing any files.
        if result.is_err() {
            self.cancel.cancel();
        }
        *completed = true;
        let delivered = result?;
        if delivered {
            tracing::info!("local update start delivered to controller");
        } else {
            tracing::info!("controlled session ended while preparing local update");
        }
        Ok(())
    }
}
