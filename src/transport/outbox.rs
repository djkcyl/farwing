//! One writer per reliable data channel (`docs/protocol/v2-design.md` §5.3).
//! Every sender on SESSION, BULK and TUNNEL goes through the channel's
//! outbox: it owns the channel's single `on_buffered_amount_low` callback,
//! serializes the space check and the enqueue, and keeps at most the
//! channel's queue in SCTP. SCTP takes the channels' messages in turn; the
//! association admits at most about 2 MB of unsent data in all, so bulk and
//! tunnel together stay below it and session frames are always admitted.
//! The count includes bytes in flight (released by SACK): the queue also
//! bounds a channel's throughput to `queue / RTT`.

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::Notify;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

/// Bytes BULK and TUNNEL may hold in SCTP (about 125 Mbit/s at 50 ms).
pub(crate) const BULK_QUEUE: usize = 768 * 1024;
/// Bytes SESSION may hold in SCTP: requests, responses and their fragments.
const SESSION_QUEUE: usize = 1024 * 1024;
/// A frame that cannot be queued within this time fails its sender.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

struct Outbox {
    channel: Weak<RTCDataChannel>,
    queue: usize,
    /// Woken when the queue drained below half of `queue`.
    drained: Arc<Notify>,
    /// Serializes the space check and the enqueue of each frame.
    writer: tokio::sync::Mutex<()>,
    installed: tokio::sync::OnceCell<()>,
}

static OUTBOXES: Mutex<Vec<Arc<Outbox>>> = Mutex::new(Vec::new());

fn outbox(channel: &Arc<RTCDataChannel>) -> Arc<Outbox> {
    let mut outboxes = OUTBOXES.lock().unwrap_or_else(|e| e.into_inner());
    outboxes.retain(|outbox| outbox.channel.strong_count() > 0);
    let weak = Arc::downgrade(channel);
    if let Some(outbox) = outboxes.iter().find(|outbox| outbox.channel.ptr_eq(&weak)) {
        return outbox.clone();
    }
    let queue = if channel.label() == farwing_protocol::peer::channel::SESSION {
        SESSION_QUEUE
    } else {
        BULK_QUEUE
    };
    let outbox = Arc::new(Outbox {
        channel: weak,
        queue,
        drained: Arc::new(Notify::new()),
        writer: tokio::sync::Mutex::new(()),
        installed: tokio::sync::OnceCell::new(),
    });
    outboxes.push(outbox.clone());
    outbox
}

impl Outbox {
    async fn install(&self, channel: &RTCDataChannel) {
        self.installed
            .get_or_init(|| async {
                channel.set_buffered_amount_low_threshold(self.queue / 2).await;
                let drained = self.drained.clone();
                channel
                    .on_buffered_amount_low(Box::new(move || {
                        drained.notify_waiters();
                        Box::pin(async {})
                    }))
                    .await;
            })
            .await;
    }

    /// Wait for room for `len` bytes (a frame larger than the queue goes
    /// once the queue is empty), then enqueue. The caller holds the writer.
    async fn enqueue(&self, channel: &RTCDataChannel, frame: &Bytes, alive: &impl Fn() -> bool) -> Result<()> {
        loop {
            let wake = self.drained.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            ensure!(
                alive() && channel.ready_state() == RTCDataChannelState::Open,
                "{}",
                crate::i18n::t().session.data_channel_closed
            );
            let queued = channel.buffered_amount().await;
            if queued == 0 || queued + frame.len() <= self.queue {
                break;
            }
            // Above the low-water mark the callback wakes us; below it (a
            // large frame waiting for an empty queue) nothing crosses it.
            let fallback = if queued > self.queue / 2 {
                Duration::from_millis(100)
            } else {
                Duration::from_millis(5)
            };
            tokio::select! {
                _ = wake => {}
                _ = tokio::time::sleep(fallback) => {}
            }
        }
        channel.send(frame).await?;
        Ok(())
    }
}

/// Send one frame once the channel has room for it.
pub(crate) async fn send(channel: &Arc<RTCDataChannel>, frame: Bytes) -> Result<()> {
    send_all(channel, [frame], || true).await
}

/// [`send`], failing (the frame dropped) once `alive` turns false.
pub(crate) async fn send_while(
    channel: &Arc<RTCDataChannel>,
    frame: Bytes,
    alive: impl Fn() -> bool,
) -> Result<()> {
    send_all(channel, [frame], alive).await
}

/// Send `frames` back to back (the fragments of one message), no other
/// writer in between.
pub(crate) async fn send_all(
    channel: &Arc<RTCDataChannel>,
    frames: impl IntoIterator<Item = Bytes>,
    alive: impl Fn() -> bool,
) -> Result<()> {
    let outbox = outbox(channel);
    outbox.install(channel).await;
    tokio::time::timeout(SEND_TIMEOUT, async {
        let _writer = outbox.writer.lock().await;
        for frame in frames {
            outbox.enqueue(channel, &frame, &alive).await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context(crate::i18n::t().session.data_channel_send_timeout)?
}
