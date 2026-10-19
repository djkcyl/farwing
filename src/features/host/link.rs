//! `FARWING_HOST_LINK`: an emulated network link between the host and each
//! viewer, for measurements. Rules as `dev video --loss` (`net:`, `rate:`,
//! `delay:`), applied to every video, RTX and FEC packet after the host has
//! stamped and recorded it for congestion control, so the viewer sees real
//! loss, queueing and delay, its feedback reports them, and the host's
//! congestion control reacts as on a real network. Packets lost here stay in
//! the retransmission history: they are repairable, as on a real network.
//! Works for any viewer, including browsers.

use crate::transport::rtc::impair::{Bottleneck, Config, NetworkLoss};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use webrtc::interceptor::{Attributes, RTPWriter};
use webrtc::rtp::packet::Packet;
use webrtc::util::marshal::MarshalSize;

type Writer = Arc<dyn RTPWriter + Send + Sync>;

pub(super) struct Link {
    bottleneck: Mutex<Option<Bottleneck>>,
    loss: Mutex<Option<NetworkLoss>>,
    delay: Duration,
    line: tokio::sync::mpsc::UnboundedSender<(Writer, Packet, Attributes, Instant)>,
}

impl Link {
    /// The link of one viewer connection, if `FARWING_HOST_LINK` is set.
    pub(super) fn from_environment() -> Option<Arc<Self>> {
        let spec = std::env::var("FARWING_HOST_LINK").ok()?;
        let config = match Config::parse(&spec) {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(%error, %spec, "ignoring FARWING_HOST_LINK");
                return None;
            }
        };
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        let (line, mut packets) =
            tokio::sync::mpsc::unbounded_channel::<(Writer, Packet, Attributes, Instant)>();
        // Departures are in order (a queue, then a fixed delay).
        runtime.spawn(async move {
            while let Some((writer, packet, attributes, due)) = packets.recv().await {
                tokio::time::sleep_until(due.into()).await;
                let _ = writer.write(&packet, &attributes).await;
            }
        });
        tracing::warn!(%spec, "host network link emulation is on");
        Some(Arc::new(Self {
            bottleneck: Mutex::new(config.link().map(Bottleneck::new)),
            loss: Mutex::new(config.network().map(NetworkLoss::new)),
            delay: config.delay(),
            line,
        }))
    }

    /// Hand `packet` to the link; it leaves later or is lost. Returns its
    /// size, as a write would.
    pub(super) fn send(&self, writer: &Writer, packet: &Packet, attributes: &Attributes) -> usize {
        let size = packet.marshal_size();
        let now = Instant::now();
        if super::lock(&self.loss).as_mut().is_some_and(|loss| loss.drops()) {
            return size;
        }
        let left = match super::lock(&self.bottleneck).as_mut() {
            Some(bottleneck) => match bottleneck.admit(now, size) {
                Some(departure) => departure,
                None => return size,
            },
            None => now,
        };
        let _ = self
            .line
            .send((Arc::clone(writer), packet.clone(), attributes.clone(), left + self.delay));
        size
    }
}
