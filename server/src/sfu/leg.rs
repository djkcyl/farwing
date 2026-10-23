//! One viewer's connection (a leg): forwarded packets go out through the
//! sender streams of the viewer's connection, each with the host's SSRCs.

use super::sdp::{self, Kind};
use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use webrtc::interceptor::stream_info::StreamInfo;
use webrtc::interceptor::{
    Attributes, Interceptor, InterceptorBuilder, RTCPReader, RTCPWriter, RTPReader, RTPWriter,
};
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtcp::packet::Packet as RtcpPacket;
use webrtc::rtp::packet::Packet;
use webrtc::rtp_transceiver::rtp_codec::{RTCRtpCodecParameters, RTPCodecType};
use webrtc::track::track_local::{TrackLocal, TrackLocalContext};

pub(super) type Rtcp = Vec<Box<dyn RtcpPacket + Send + Sync>>;

/// Packets queued per leg; a viewer that cannot keep up loses the excess
/// instead of delaying the others.
const QUEUE: usize = 4096;

/// A sender that forwards another sender's packets: the host's SSRCs and
/// preferred codec.
pub(super) struct ForwardTrack {
    pub track: sdp::Track,
    /// MIME type of the preferred codec, for codec selection on bind.
    pub mime: String,
}

#[async_trait]
impl TrackLocal for ForwardTrack {
    async fn bind(&self, t: &TrackLocalContext) -> webrtc::error::Result<RTCRtpCodecParameters> {
        let codecs = t.codec_parameters();
        codecs
            .iter()
            .find(|c| c.payload_type == self.track.payload_type)
            .or_else(|| {
                codecs
                    .iter()
                    .find(|c| c.capability.mime_type.eq_ignore_ascii_case(&self.mime))
            })
            .or_else(|| codecs.first())
            .cloned()
            .ok_or(webrtc::Error::ErrUnsupportedCodec)
    }

    async fn unbind(&self, _: &TrackLocalContext) -> webrtc::error::Result<()> {
        Ok(())
    }

    fn id(&self) -> &str {
        &self.track.id
    }

    fn rid(&self) -> Option<&str> {
        None
    }

    fn stream_id(&self) -> &str {
        &self.track.stream_id
    }

    fn kind(&self) -> RTPCodecType {
        match self.track.kind {
            Kind::Audio => RTPCodecType::Audio,
            Kind::Video => RTPCodecType::Video,
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn ssrcs(&self) -> Option<(u32, u32, u32)> {
        Some((self.track.ssrc, self.track.rtx, self.track.fec))
    }
}

/// A bound sender stream of the leg.
#[derive(Clone)]
struct Bound {
    writer: Arc<dyn RTPWriter + Send + Sync>,
}

/// The leg's interceptor: keeps the writer of every sender stream (media,
/// RTX and FEC SSRCs) and passes every incoming RTCP batch to the leg.
pub(super) struct Tap {
    streams: Mutex<HashMap<u32, Bound>>,
    rtcp: mpsc::UnboundedSender<Rtcp>,
}

impl Tap {
    pub fn new(rtcp: mpsc::UnboundedSender<Rtcp>) -> Arc<Self> {
        Arc::new(Self {
            streams: Mutex::default(),
            rtcp,
        })
    }

    fn bound(&self, ssrc: u32) -> Option<Bound> {
        self.streams.lock().get(&ssrc).cloned()
    }
}

pub(super) struct TapBuilder(pub Arc<Tap>);

impl InterceptorBuilder for TapBuilder {
    fn build(
        &self,
        _: &str,
    ) -> Result<Arc<dyn Interceptor + Send + Sync>, webrtc::interceptor::Error> {
        Ok(self.0.clone())
    }
}

struct TapReader {
    parent: Arc<dyn RTCPReader + Send + Sync>,
    rtcp: mpsc::UnboundedSender<Rtcp>,
}

#[async_trait]
impl RTCPReader for TapReader {
    async fn read(
        &self,
        buf: &mut [u8],
        attributes: &Attributes,
    ) -> Result<(Rtcp, Attributes), webrtc::interceptor::Error> {
        let (packets, attributes) = self.parent.read(buf, attributes).await?;
        let _ = self.rtcp.send(packets.iter().map(|p| p.cloned()).collect());
        Ok((packets, attributes))
    }
}

#[async_trait]
impl Interceptor for Tap {
    async fn bind_rtcp_reader(
        &self,
        reader: Arc<dyn RTCPReader + Send + Sync>,
    ) -> Arc<dyn RTCPReader + Send + Sync> {
        Arc::new(TapReader {
            parent: reader,
            rtcp: self.rtcp.clone(),
        })
    }

    async fn bind_rtcp_writer(
        &self,
        writer: Arc<dyn RTCPWriter + Send + Sync>,
    ) -> Arc<dyn RTCPWriter + Send + Sync> {
        writer
    }

    async fn bind_local_stream(
        &self,
        info: &StreamInfo,
        writer: Arc<dyn RTPWriter + Send + Sync>,
    ) -> Arc<dyn RTPWriter + Send + Sync> {
        self.streams.lock().insert(
            info.ssrc,
            Bound {
                writer: writer.clone(),
            },
        );
        writer
    }

    async fn unbind_local_stream(&self, info: &StreamInfo) {
        self.streams.lock().remove(&info.ssrc);
    }

    async fn bind_remote_stream(
        &self,
        _: &StreamInfo,
        reader: Arc<dyn RTPReader + Send + Sync>,
    ) -> Arc<dyn RTPReader + Send + Sync> {
        reader
    }

    async fn unbind_remote_stream(&self, _: &StreamInfo) {}

    async fn close(&self) -> Result<(), webrtc::interceptor::Error> {
        self.streams.lock().clear();
        Ok(())
    }
}

/// How the leg's packets differ from the host's: none for Farwing clients
/// (same tables), payload types and extension IDs for others.
#[derive(Default)]
pub(super) struct Rewrite {
    /// Host payload type → leg payload type; absent = not negotiated.
    pub payload_types: HashMap<u8, u8>,
    /// Host extension ID → leg extension ID.
    pub extensions: HashMap<u8, u8>,
    /// The leg's MID extension ID and each forwarded SSRC's MID on the leg:
    /// browsers route packets by MID, which the host set for its own
    /// connection.
    pub mid_extension: Option<u8>,
    pub mids: HashMap<u32, Bytes>,
    pub identity: bool,
}

impl Rewrite {
    pub fn new(host: &sdp::Wire, leg: &sdp::Wire) -> Self {
        let mut payload_types = HashMap::new();
        for (pt, codec) in &host.codecs {
            if codec.apt().is_some() {
                continue;
            }
            let same = leg.codecs.get(pt).is_some_and(|c| c.compatible(codec));
            let target = if same {
                Some(*pt)
            } else {
                leg.codecs
                    .iter()
                    .filter(|(_, c)| c.compatible(codec))
                    .map(|(p, _)| *p)
                    .min()
            };
            if let Some(target) = target {
                payload_types.insert(*pt, target);
            }
        }
        // RTX follows its primary codec.
        for (pt, codec) in &host.codecs {
            let Some(apt) = codec.apt() else { continue };
            let Some(target) = payload_types.get(&apt) else { continue };
            if let Some(rtx) = leg
                .codecs
                .iter()
                .find_map(|(p, c)| (c.apt() == Some(*target)).then_some(*p))
            {
                payload_types.insert(*pt, rtx);
            }
        }
        let extensions: HashMap<u8, u8> = host
            .extensions
            .iter()
            .filter_map(|(id, uri)| Some((*id, leg.extension_id(uri)?)))
            .collect();
        let identity = payload_types.iter().all(|(a, b)| a == b)
            && payload_types.len() == host.codecs.len()
            && extensions.iter().all(|(a, b)| a == b)
            && extensions.len() == host.extensions.len();
        Self {
            payload_types,
            extensions,
            mid_extension: leg.extension_id(farwing_rtc_wire::MID_URI),
            mids: HashMap::new(),
            identity,
        }
    }

    /// `false`: the leg did not negotiate the packet's codec.
    fn apply(&self, packet: &mut Packet) -> bool {
        if self.identity {
            return true;
        }
        let Some(pt) = self.payload_types.get(&packet.header.payload_type) else {
            return false;
        };
        packet.header.payload_type = *pt;
        let mid = self.mids.get(&packet.header.ssrc);
        let extensions = std::mem::take(&mut packet.header.extensions);
        packet.header.extensions = extensions
            .into_iter()
            .filter_map(|mut e| {
                e.id = *self.extensions.get(&e.id)?;
                if Some(e.id) == self.mid_extension
                    && let Some(mid) = mid
                {
                    e.payload = mid.clone();
                }
                Some(e)
            })
            .collect();
        // The parsed header keeps the original padding of the extension block.
        let length = packet.header.get_extension_payload_len();
        packet.header.extensions_padding = (4 - length % 4) % 4;
        if packet.header.extensions.is_empty() {
            packet.header.extension = false;
            packet.header.extensions_padding = 0;
        }
        true
    }
}

/// A development bottleneck in front of the viewer
/// (`FARWING_DEV_SFU_DOWNLINK`): packets leave at the link's rate, recorded
/// as sent when they enter it; a queue longer than [`SHAPER_QUEUE`] drops.
struct Shaper {
    bits_per_second: f64,
    free_at: Instant,
}

const SHAPER_QUEUE: Duration = Duration::from_millis(200);

impl Shaper {
    /// When a packet of `bytes` arrives; `None` when the queue is full.
    fn admit(&mut self, bytes: usize) -> Option<Instant> {
        let now = Instant::now();
        let start = self.free_at.max(now);
        if start.duration_since(now) > SHAPER_QUEUE {
            return None;
        }
        self.free_at = start + Duration::from_secs_f64((bytes + 48) as f64 * 8.0 / self.bits_per_second);
        Some(self.free_at)
    }
}

#[derive(Default)]
pub(super) struct LegStats {
    pub packets: AtomicU64,
    pub bytes: AtomicU64,
    pub repairs: AtomicU64,
    pub dropped: AtomicU64,
    /// Fraction lost (×256) of the latest report.
    pub loss: AtomicU16,
    pub rtt_ms: AtomicU64,
}

pub(super) struct Leg {
    pub id: String,
    pub pc: Arc<RTCPeerConnection>,
    tap: Arc<Tap>,
    queue: mpsc::Sender<Packet>,
    pub rewrite: Rewrite,
    /// Transport-wide sequence extension ID of the leg.
    tcc: Option<u8>,
    tcc_seq: AtomicU16,
    rtx_seq: Mutex<HashMap<u32, u16>>,
    /// (SSRC, sequence) → last repair, against repeated requests.
    repaired: Mutex<HashMap<(u32, u16), Instant>>,
    /// Sender reports passed on: NTP middle 32 bits → when.
    reports: Mutex<VecDeque<(u32, Instant)>>,
    pub stats: LegStats,
    /// The viewer's downlink, from its transport-wide feedback.
    pub downlink: Mutex<super::estimate::Downlink>,
    shaper: Option<Mutex<Shaper>>,
    pub tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl Leg {
    pub fn new(
        id: String,
        pc: Arc<RTCPeerConnection>,
        tap: Arc<Tap>,
        rewrite: Rewrite,
        tcc: Option<u8>,
        bottleneck: Option<f64>,
    ) -> (Arc<Self>, mpsc::Receiver<Packet>) {
        let (queue, packets) = mpsc::channel(QUEUE);
        let leg = Arc::new(Self {
            id,
            pc,
            tap,
            queue,
            rewrite,
            tcc,
            tcc_seq: AtomicU16::new(rand::random()),
            rtx_seq: Mutex::default(),
            repaired: Mutex::default(),
            reports: Mutex::default(),
            stats: LegStats::default(),
            downlink: Mutex::new(super::estimate::Downlink::new()),
            shaper: bottleneck.map(|bits_per_second| {
                Mutex::new(Shaper {
                    bits_per_second,
                    free_at: Instant::now(),
                })
            }),
            tasks: Mutex::default(),
        });
        (leg, packets)
    }

    pub fn push(&self, packet: Packet) {
        if self.queue.try_send(packet).is_err() {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Writes queued packets until the connection closes.
    pub async fn run(self: Arc<Self>, mut packets: mpsc::Receiver<Packet>) {
        // Behind a development bottleneck, packets leave later, in order.
        let delayed = self.shaper.as_ref().map(|_| {
            let (delayed, mut queue) = mpsc::unbounded_channel::<(Instant, Bound, Packet)>();
            let leg = Arc::clone(&self);
            tokio::spawn(async move {
                while let Some((at, bound, packet)) = queue.recv().await {
                    tokio::time::sleep_until(at.into()).await;
                    leg.write(&bound, &packet).await;
                }
            });
            delayed
        });
        while let Some(mut packet) = packets.recv().await {
            let Some(bound) = self.tap.bound(packet.header.ssrc) else {
                continue;
            };
            if !self.rewrite.apply(&mut packet) {
                continue;
            }
            let mut sequence = None;
            if let Some(id) = self.tcc {
                let seq = self.tcc_seq.fetch_add(1, Ordering::Relaxed);
                if packet
                    .header
                    .set_extension(id, Bytes::copy_from_slice(&seq.to_be_bytes()))
                    .is_ok()
                {
                    packet.header.extension = true;
                    sequence = Some(seq);
                }
            }
            let size = webrtc::util::MarshalSize::marshal_size(&packet);
            if let Some(seq) = sequence {
                self.downlink.lock().sent(seq, size);
            }
            if let (Some(shaper), Some(delayed)) = (&self.shaper, &delayed) {
                match shaper.lock().admit(size) {
                    Some(at) => {
                        let _ = delayed.send((at, bound, packet));
                    }
                    None => {
                        self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                continue;
            }
            self.write(&bound, &packet).await;
        }
    }

    async fn write(&self, bound: &Bound, packet: &Packet) {
        match bound.writer.write(packet, &Attributes::new()).await {
            Ok(n) => {
                self.stats.packets.fetch_add(1, Ordering::Relaxed);
                self.stats.bytes.fetch_add(n as u64, Ordering::Relaxed);
            }
            Err(error) => tracing::trace!(%error, leg = %self.id, "forwarding write failed"),
        }
    }

    /// An RTX packet repairing `original` on this leg; `None` if just repaired.
    pub fn repair(&self, original: &Packet, rtx_ssrc: u32, rtx_pt: u8) -> Option<Packet> {
        let key = (original.header.ssrc, original.header.sequence_number);
        {
            let mut repaired = self.repaired.lock();
            let now = Instant::now();
            if repaired
                .get(&key)
                .is_some_and(|at| now.duration_since(*at) < Duration::from_millis(40))
            {
                return None;
            }
            if repaired.len() > 4096 {
                repaired.retain(|_, at| now.duration_since(*at) < Duration::from_secs(2));
            }
            repaired.insert(key, now);
        }
        let mut packet = original.clone();
        let mut payload = Vec::with_capacity(original.payload.len() + 2);
        payload.extend_from_slice(&original.header.sequence_number.to_be_bytes());
        payload.extend_from_slice(&original.payload);
        packet.payload = Bytes::from(payload);
        packet.header.ssrc = rtx_ssrc;
        packet.header.payload_type = rtx_pt;
        packet.header.sequence_number = {
            let mut seqs = self.rtx_seq.lock();
            let seq = seqs.entry(rtx_ssrc).or_insert_with(rand::random);
            let current = *seq;
            *seq = seq.wrapping_add(1);
            current
        };
        self.stats.repairs.fetch_add(1, Ordering::Relaxed);
        Some(packet)
    }

    /// A host sender report passed on to this viewer.
    pub fn report_sent(&self, ntp: u64) {
        let mut reports = self.reports.lock();
        reports.push_back(((ntp >> 16) as u32, Instant::now()));
        while reports.len() > 16 {
            reports.pop_front();
        }
    }

    /// A viewer's receiver report block for a forwarded stream.
    pub fn report_received(&self, fraction_lost: u8, lsr: u32, dlsr: u32) {
        self.stats.loss.store(u16::from(fraction_lost), Ordering::Relaxed);
        if lsr == 0 {
            return;
        }
        let sent = self
            .reports
            .lock()
            .iter()
            .find_map(|(ntp, at)| (*ntp == lsr).then_some(*at));
        if let Some(sent) = sent {
            let delay = Duration::from_micros(u64::from(dlsr) * 1_000_000 / 65_536);
            if let Some(rtt) = sent.elapsed().checked_sub(delay) {
                self.stats
                    .rtt_ms
                    .store(rtt.as_millis() as u64, Ordering::Relaxed);
            }
        }
    }

    pub async fn close(&self) {
        for task in self.tasks.lock().drain(..) {
            task.abort();
        }
        let _ = self.pc.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use webrtc::rtp::header::Header;
    use webrtc::util::marshal::Marshal;

    #[test]
    fn browser_legs_get_their_payload_types_extension_ids_and_mids() {
        let host = sdp::wire(
            "a=rtpmap:35 AV1/90000\r\n\
a=fmtp:35 level-idx=17;profile=0;tier=0\r\n\
a=extmap:2 urn:ietf:params:rtp-hdrext:sdes:mid\r\n\
a=extmap:3 http://www.webrtc.org/experiments/rtp-hdrext/video-timing\r\n",
        );
        let leg = sdp::wire(
            "a=rtpmap:45 AV1/90000\r\n\
a=fmtp:45 profile=0;tier=0\r\n\
a=extmap:9 urn:ietf:params:rtp-hdrext:sdes:mid\r\n",
        );
        let mut rewrite = Rewrite::new(&host, &leg);
        assert!(!rewrite.identity);
        rewrite.mids.insert(7, Bytes::from_static(b"0"));
        let mut header = Header {
            version: 2,
            extension: true,
            extension_profile: 0xBEDE,
            payload_type: 35,
            ssrc: 7,
            ..Default::default()
        };
        header.set_extension(2, Bytes::from_static(b"3")).unwrap();
        header.set_extension(3, Bytes::from_static(&[1; 13])).unwrap();
        let mut packet = Packet {
            header,
            payload: Bytes::from_static(&[1, 2, 3]),
        };
        assert!(rewrite.apply(&mut packet));
        assert_eq!(packet.header.payload_type, 45);
        assert_eq!(packet.header.get_extension(9), Some(Bytes::from_static(b"0")));
        assert_eq!(packet.header.extensions.len(), 1);
        let bytes = packet.marshal().unwrap();
        assert_eq!(
            <Packet as webrtc::util::Unmarshal>::unmarshal(&mut bytes.clone()).unwrap().payload,
            packet.payload
        );
    }
}
