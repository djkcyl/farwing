//! One writer gate per `farwing.bulk.v2` channel (`docs/protocol/v2-design.md`
//! §5.3, §8). FILE, CLIPBOARD, DRAG and DIAGNOSTICS all send `TRANSFER` and
//! `FLOW` frames on the same channel; they share its single
//! `on_buffered_amount_low` callback here instead of replacing each other's,
//! and together keep at most [`QUEUE`] bytes queued in SCTP so session and
//! control frames are never stuck behind bulk data.

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::Notify;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

/// Bytes the bulk channel may hold in SCTP.
pub(crate) const QUEUE: usize = 256 * 1024;
/// A frame that cannot be queued within this time fails its sender.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

struct Gate {
    channel: Weak<RTCDataChannel>,
    /// Woken when the queue drained below half of [`QUEUE`].
    drained: Arc<Notify>,
    /// Serializes the space check and the enqueue of each frame.
    writer: tokio::sync::Mutex<()>,
    installed: tokio::sync::OnceCell<()>,
}

static GATES: Mutex<Vec<Arc<Gate>>> = Mutex::new(Vec::new());

fn gate(channel: &Arc<RTCDataChannel>) -> Arc<Gate> {
    let mut gates = GATES.lock().unwrap_or_else(|e| e.into_inner());
    gates.retain(|gate| gate.channel.strong_count() > 0);
    let weak = Arc::downgrade(channel);
    if let Some(gate) = gates.iter().find(|gate| gate.channel.ptr_eq(&weak)) {
        return gate.clone();
    }
    let gate = Arc::new(Gate {
        channel: weak,
        drained: Arc::new(Notify::new()),
        writer: tokio::sync::Mutex::new(()),
        installed: tokio::sync::OnceCell::new(),
    });
    gates.push(gate.clone());
    gate
}

/// Send one frame on the bulk channel once SCTP holds at most [`QUEUE`]
/// bytes. `alive` is checked while waiting; when it turns false the frame is
/// dropped and the call fails.
pub(crate) async fn send(
    channel: &Arc<RTCDataChannel>,
    frame: Bytes,
    alive: impl Fn() -> bool,
) -> Result<()> {
    let gate = gate(channel);
    gate.installed
        .get_or_init(|| async {
            channel
                .set_buffered_amount_low_threshold(QUEUE / 2)
                .await;
            let drained = gate.drained.clone();
            channel
                .on_buffered_amount_low(Box::new(move || {
                    drained.notify_waiters();
                    Box::pin(async {})
                }))
                .await;
        })
        .await;
    tokio::time::timeout(SEND_TIMEOUT, async {
        let _writer = gate.writer.lock().await;
        loop {
            let wake = gate.drained.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            ensure!(
                alive() && channel.ready_state() == RTCDataChannelState::Open,
                "{}",
                crate::i18n::t().session.data_channel_closed
            );
            if channel.buffered_amount().await + frame.len() <= QUEUE {
                break;
            }
            tokio::select! {
                _ = wake => {},
                _ = tokio::time::sleep(Duration::from_millis(20)) => {},
            }
        }
        channel.send(&frame).await?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context(crate::i18n::t().session.data_channel_send_timeout)?
}
