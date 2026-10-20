//! `FARWING_HOST_LINK`: an emulated network link between the host and each
//! viewer, for measurements. Rules as `dev video --loss` (`net:`, `rate:`,
//! `delay:`), applied to every video, RTX and FEC packet after the host has
//! stamped and recorded it for congestion control, so the viewer sees real
//! loss, queueing and delay, its feedback reports them, and the host's
//! congestion control reacts as on a real network. Packets lost here stay in
//! the retransmission history: they are repairable, as on a real network.
//! The host's RTCP (sender reports) waits in the same queue, so round trips
//! measured from them include the queueing, as on a real link.
//! Works for any viewer, including browsers.

use crate::transport::rtc::impair::{Bottleneck, Config, NetworkLoss};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use webrtc::interceptor::{Attributes, RTCPWriter, RTPWriter};
use webrtc::rtp::packet::Packet;
use webrtc::util::marshal::MarshalSize;

type Writer = Arc<dyn RTPWriter + Send + Sync>;
type ControlWriter = Arc<dyn RTCPWriter + Send + Sync>;
type Control = Vec<Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>>;

pub(super) struct Link {
    bottleneck: Mutex<Option<Bottleneck>>,
    loss: Mutex<Option<NetworkLoss>>,
    mtu: Option<usize>,
    delay: Duration,
    line: tokio::sync::mpsc::UnboundedSender<(Writer, Packet, Attributes, Instant)>,
    control: tokio::sync::mpsc::UnboundedSender<(ControlWriter, Control, Attributes, Instant)>,
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
        let (control, mut reports) =
            tokio::sync::mpsc::unbounded_channel::<(ControlWriter, Control, Attributes, Instant)>();
        runtime.spawn(async move {
            while let Some((writer, packets, attributes, due)) = reports.recv().await {
                tokio::time::sleep_until(due.into()).await;
                let _ = writer.write(&packets, &attributes).await;
            }
        });
        tracing::warn!(%spec, "host network link emulation is on");
        Some(Arc::new(Self {
            bottleneck: Mutex::new(config.link().map(Bottleneck::new)),
            loss: Mutex::new(config.network().map(NetworkLoss::new)),
            mtu: config.mtu(),
            delay: config.delay(),
            line,
            control,
        }))
    }

    /// Hand an RTCP batch to the link, behind the media already queued.
    pub(super) fn send_control(
        &self,
        writer: &ControlWriter,
        packets: &[Box<dyn webrtc::rtcp::packet::Packet + Send + Sync>],
        attributes: &Attributes,
    ) -> usize {
        let size = packets.iter().map(|packet| packet.marshal_size()).sum();
        if super::lock(&self.loss).as_mut().is_some_and(|loss| loss.drops()) {
            return size;
        }
        let left = match super::lock(&self.bottleneck).as_mut() {
            Some(bottleneck) => match bottleneck.admit(Instant::now(), size) {
                Some(departure) => departure,
                None => return size,
            },
            None => Instant::now(),
        };
        let _ = self.control.send((
            Arc::clone(writer),
            packets.iter().map(|packet| packet.cloned()).collect(),
            attributes.clone(),
            left + self.delay,
        ));
        size
    }

    /// Hand `packet` to the link; it leaves later or is lost. Returns its
    /// size, as a write would. `overhead`: the bytes SRTP, UDP and IP add.
    pub(super) fn send(
        &self,
        writer: &Writer,
        packet: &Packet,
        attributes: &Attributes,
        overhead: usize,
    ) -> usize {
        let size = packet.marshal_size();
        let now = Instant::now();
        if self.mtu.is_some_and(|mtu| size + overhead > mtu)
            || super::lock(&self.loss).as_mut().is_some_and(|loss| loss.drops())
        {
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
