use super::workers::SessionWorkers;
use crate::diagnostics::performance::PerformanceMonitor;
use crate::transport::rsfec::RsFecReceiver;
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use webrtc::rtp::packet::Packet as RtpPacket;
use webrtc::util::marshal::Unmarshal;
use webrtc_srtp::session::IncomingRtpPacket;

// Ordered RTP ingress, repair decoding and interceptor ownership.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum VideoIngressOrigin {
    Primary,
    Rtx,
    RsFec,
}

pub(super) struct VideoIngressPacket {
    pub(super) ordinal: u64,
    pub(super) received_at: Instant,
    pub(super) wire_bytes: usize,
    pub(super) origin: VideoIngressOrigin,
    pub(super) packet: RtpPacket,
    pub(super) raw: Bytes,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MediaRecoveryFlags {
    pub(super) recovered_from_rtx: bool,
    pub(super) recovered_by_fec: bool,
}

pub(super) struct PendingMediaPacket {
    pub(super) packet: RtpPacket,
    pub(super) flags: MediaRecoveryFlags,
    pub(super) received_at: Instant,
    pub(super) rsfec_source: Option<Bytes>,
}

pub(super) struct OrderedVideoIngress {
    pub(super) receiver: broadcast::Receiver<IncomingRtpPacket>,
    pub(super) media_ssrc: u32,
    pub(super) rtx_ssrc: u32,
    pub(super) fec_ssrc: u32,
    /// `dev video --loss delay:MS`: every packet arrives this much later.
    delay: std::time::Duration,
    /// `dev video --loss rate:MBPS`: a bottleneck in front of the receiver.
    bottleneck: Option<super::impair::Bottleneck>,
    /// `dev video --loss net:P,R`: random loss in front of the receiver.
    network: Option<super::impair::NetworkLoss>,
    /// `dev video --loss outage:START+DURATION`: the network drops out.
    outage: Option<super::impair::OutageWindow>,
    /// A packet taken from the broadcast and when it is due, so that a
    /// cancelled `recv` does not lose it.
    held: Option<(VideoIngressPacket, Instant)>,
}

impl OrderedVideoIngress {
    pub(super) async fn open(track: &webrtc::track::track_remote::TrackRemote) -> Result<Self> {
        let receiver = track
            .subscribe_incoming_rtp()
            .await
            .context("subscribe to ordered decrypted RTP ingress")?;
        let (media_ssrc, rtx_ssrc, fec_ssrc) = track
            .associated_ssrcs()
            .await
            .context("resolve associated media/RTX/FEC SSRCs")?;
        Ok(Self {
            receiver,
            media_ssrc,
            rtx_ssrc,
            fec_ssrc,
            delay: super::impair::config().map_or(std::time::Duration::ZERO, |c| c.delay()),
            bottleneck: super::impair::config()
                .and_then(|c| c.link())
                .map(super::impair::Bottleneck::new),
            network: super::impair::config()
                .and_then(|c| c.network())
                .map(super::impair::NetworkLoss::new),
            outage: super::impair::config()
                .and_then(|c| c.outage())
                .map(super::impair::OutageWindow::new),
            held: None,
        })
    }

    /// Cancel-safe.
    pub(super) async fn recv(&mut self) -> Result<VideoIngressPacket> {
        if self.delay.is_zero() && self.bottleneck.is_none() && self.network.is_none() && self.outage.is_none() {
            return self.next().await;
        }
        while self.held.is_none() {
            let packet = self.next().await?;
            if self.network.as_mut().is_some_and(|n| n.drops()) {
                continue;
            }
            let media = matches!(packet.origin, VideoIngressOrigin::Primary).then_some(packet.packet.header.timestamp);
            if self.outage.as_mut().is_some_and(|o| o.drops(packet.received_at, media)) {
                continue;
            }
            let left = match self.bottleneck.as_mut() {
                Some(link) => match link.admit(packet.received_at, packet.wire_bytes) {
                    Some(departure) => departure,
                    // Lost in the queue.
                    None => continue,
                },
                None => packet.received_at,
            };
            self.held = Some((packet, left + self.delay));
        }
        let due = self.held.as_ref().map_or_else(Instant::now, |(_, due)| *due);
        tokio::time::sleep_until(due.into()).await;
        let (mut packet, _) = self.held.take().expect("held above");
        packet.received_at = Instant::now();
        Ok(packet)
    }

    async fn next(&mut self) -> Result<VideoIngressPacket> {
        loop {
            let incoming = match self.receiver.recv().await {
                Ok(incoming) => incoming,
                Err(broadcast::error::RecvError::Lagged(dropped)) => {
                    // A lagged observation subscriber must not tear down the media
                    // session. The SRTP session and its per-SSRC readers continue to
                    // receive packets; the missing sequence range is reported to the
                    // receiver state machine below and recovered with NACK/PLI just
                    // like packets lost on the network.
                    tracing::warn!(
                        dropped,
                        "ordered RTP ingress subscriber lagged; continuing from the oldest available packet"
                    );
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    bail!("ordered RTP ingress closed")
                }
            };
            let wire_bytes = incoming.data.len();
            let mut raw = incoming.data.as_ref();
            let packet = match RtpPacket::unmarshal(&mut raw) {
                Ok(packet) => packet,
                Err(error) => {
                    tracing::debug!(%error, ordinal = incoming.ordinal, wire_bytes,
                        "dropping malformed decrypted RTP before demux");
                    continue;
                }
            };
            let origin = if packet.header.ssrc == self.media_ssrc {
                VideoIngressOrigin::Primary
            } else if self.rtx_ssrc != 0 && packet.header.ssrc == self.rtx_ssrc {
                VideoIngressOrigin::Rtx
            } else if self.fec_ssrc != 0 && packet.header.ssrc == self.fec_ssrc {
                VideoIngressOrigin::RsFec
            } else {
                continue;
            };
            return Ok(VideoIngressPacket {
                ordinal: incoming.ordinal,
                received_at: incoming.received_at,
                wire_bytes,
                origin,
                packet,
                raw: incoming.data,
            });
        }
    }
}

// Per-track abort handles; the peer owns and joins the actual task handles.
pub(super) struct VideoInterceptorDrainers(pub(super) Vec<tokio::task::AbortHandle>);

impl VideoInterceptorDrainers {
    pub(super) fn start(
        track: Arc<webrtc::track::track_remote::TrackRemote>,
        rtx_ssrc: u32,
        fec_ssrc: u32,
        workers: &SessionWorkers,
        stop: &CancellationToken,
    ) -> Self {
        let mut tasks = Vec::with_capacity(3);
        let media_track = Arc::clone(&track);
        tasks.extend(workers.spawn_in(stop.clone(), async move {
            while media_track.read_rtp().await.is_ok() {}
        }));
        if rtx_ssrc != 0 {
            let rtx_track = Arc::clone(&track);
            tasks.extend(workers.spawn_in(stop.clone(), async move {
                while rtx_track.read_rtx().await.is_ok() {}
            }));
        }
        if fec_ssrc != 0 {
            tasks.extend(workers.spawn_in(stop.clone(), async move {
                while track.read_fec().await.is_ok() {}
            }));
        }
        Self(tasks)
    }
}

impl Drop for VideoInterceptorDrainers {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

pub(super) fn parse_rsfec_recovered_packets(
    recovered_packets: Vec<Vec<u8>>,
    received_at: Instant,
) -> Vec<PendingMediaPacket> {
    recovered_packets
        .into_iter()
        .filter_map(|recovered| {
            let source = Bytes::from(recovered);
            let mut raw = source.as_ref();
            match RtpPacket::unmarshal(&mut raw) {
                Ok(packet) => Some(PendingMediaPacket {
                    packet,
                    flags: MediaRecoveryFlags {
                        recovered_from_rtx: false,
                        recovered_by_fec: true,
                    },
                    received_at,
                    // A packet recovered by FEC re-enters the normal media
                    // path, but is never offered back to any FEC decoder as a
                    // new source.  Only primary media and normalized
                    // RTX-as-source packets feed the repair caches.
                    rsfec_source: None,
                }),
                Err(error) => {
                    tracing::warn!(%error, "RSFEC recovered malformed RTP packet");
                    None
                }
            }
        })
        .collect()
}

pub(super) fn remember_fec_source(
    rsfec_receiver: &mut Option<RsFecReceiver>,
    pending_media: &mut VecDeque<PendingMediaPacket>,
    sequence_number: u16,
    source: Bytes,
    received_at: Instant,
    performance: &PerformanceMonitor,
) {
    if let Some(rsfec_receiver) = rsfec_receiver.as_mut() {
        match rsfec_receiver.remember_media(sequence_number, &source) {
            Ok(recovery) => {
                performance.record_fec_recovered(recovery.recovered_packets.len());
                pending_media.extend(parse_rsfec_recovered_packets(
                    recovery.recovered_packets,
                    received_at,
                ));
            }
            Err(error) => tracing::warn!(%error, "RSFEC media source rejected"),
        }
    }
}

pub(super) fn recover_rtx_packet(
    mut packet: RtpPacket,
    primary_payload_type: u8,
    primary_ssrc: u32,
) -> Option<RtpPacket> {
    if packet.payload.len() < 2 {
        return None;
    }
    packet.header.sequence_number = u16::from_be_bytes([packet.payload[0], packet.payload[1]]);
    packet.header.payload_type = primary_payload_type;
    packet.header.ssrc = primary_ssrc;
    packet.header.padding = false;
    packet.payload = packet.payload.slice(2..);
    Some(packet)
}
