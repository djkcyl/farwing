//! Shared Opus packetization and RTP/RTCP sending. Each audio direction owns its source.
use super::encoder::{Config as Encoding, create_encoder as make_encoder};
use bytes::Bytes;
use crossbeam_queue::ArrayQueue;
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use webrtc::{
    rtp::packet::Packet, rtp_transceiver::rtp_sender::RTCRtpSender,
    track::track_local::track_local_static_rtp::TrackLocalStaticRTP,
};
pub(crate) const RATE: u32 = 48_000;
pub(crate) const BLOCK: usize = 480;
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
pub(crate) struct Frame {
    pub samples: [f32; BLOCK * 2],
    pub timestamp: u32,
    pub generation: u64,
    pub created: Instant,
}
pub(crate) trait Source: Send + Sync {
    fn current(&self, generation: u64) -> bool;
    fn stopped(&self) -> &AtomicBool;
    fn encoding(&self) -> Option<Encoding>;
    fn frames(&self) -> &ArrayQueue<Frame>;
    fn wake(&self) -> &Notify;
    fn report_wake(&self) -> &Notify;
    fn fault(&self, error: String);
    fn send_error(&self, error: String);
}
#[derive(Clone, Copy)]
struct SentClock {
    timestamp: u32,
    at: Instant,
    generation: u64,
    first: Instant,
}
#[derive(Default)]
pub(crate) struct Transmitter {
    loss: AtomicU32,
    pub sent: AtomicU64,
    pub reports: AtomicU64,
    octets: AtomicU64,
    sent_clock: Mutex<Option<SentClock>>,
}
impl Transmitter {
    pub fn reset_loss(&self) {
        self.loss.store(0, Ordering::Relaxed);
    }
    pub(crate) async fn feedback(&self, sender: Arc<RTCRtpSender>) {
        while let Ok((packets, _)) = sender.read_rtcp().await {
            let params = sender.get_parameters().await;
            for packet in packets {
                let reports = if let Some(p) = packet
                    .as_any()
                    .downcast_ref::<webrtc::rtcp::receiver_report::ReceiverReport>(
                ) {
                    &p.reports
                } else if let Some(p) = packet
                    .as_any()
                    .downcast_ref::<webrtc::rtcp::sender_report::SenderReport>()
                {
                    &p.reports
                } else {
                    continue;
                };
                for report in reports {
                    if params.encodings.iter().any(|e| e.ssrc == report.ssrc) {
                        self.loss.store(
                            (u32::from(report.fraction_lost) * 100 + 128) / 256,
                            Ordering::Relaxed,
                        );
                        self.reports.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    pub(crate) async fn send_reports(
        &self,
        source: &impl Source,
        connection: std::sync::Weak<webrtc::peer_connection::RTCPeerConnection>,
        sender: Arc<RTCRtpSender>,
    ) {
        use webrtc::rtcp::{
            packet::Packet as RtcpPacket,
            sender_report::SenderReport,
            source_description::{
                SdesType, SourceDescription, SourceDescriptionChunk, SourceDescriptionItem,
            },
        };
        let anchor = (Instant::now(), std::time::SystemTime::now());
        let mut epoch = None;
        let mut due = None;
        loop {
            let wake = source.report_wake().notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if source.stopped().load(Ordering::Acquire) {
                break;
            }
            let clock = (*lock(&self.sent_clock)).filter(|c| source.current(c.generation));
            match clock {
                Some(c) if epoch != Some(c.generation) => {
                    epoch = Some(c.generation);
                    due = Some(c.first + Duration::from_millis(2500));
                }
                None => {
                    epoch = None;
                    due = None;
                }
                _ => {}
            }
            let Some(deadline) = due else {
                wake.await;
                continue;
            };
            tokio::select! {
                _=&mut wake=>continue,
                _=tokio::time::sleep_until(deadline.into())=>{}
            }
            let Some(c) = (*lock(&self.sent_clock)).filter(|c| source.current(c.generation)) else {
                continue;
            };
            let Some(peer) = connection.upgrade() else {
                break;
            };
            let params = sender.get_parameters().await;
            let Some(encoding) = params.encodings.first() else {
                continue;
            };
            let now = Instant::now();
            let report = SenderReport {
                ssrc: encoding.ssrc,
                ntp_time: webrtc::rtp::extension::abs_send_time_extension::unix2ntp(
                    anchor.1 + anchor.0.elapsed(),
                ),
                rtp_time: c.timestamp.wrapping_add(
                    (now.duration_since(c.at).as_secs_f64() * f64::from(RATE)) as u32,
                ),
                packet_count: self.sent.load(Ordering::Relaxed) as u32,
                octet_count: self.octets.load(Ordering::Relaxed) as u32,
                ..Default::default()
            };
            // webrtc builds SDP CNAME from TrackLocal::stream_id (audio_0).
            let sdes = SourceDescription {
                chunks: vec![SourceDescriptionChunk {
                    source: encoding.ssrc,
                    items: vec![SourceDescriptionItem {
                        sdes_type: SdesType::SdesCname,
                        text: Bytes::from_static(b"audio_0"),
                    }],
                }],
            };
            let packets: Vec<Box<dyn RtcpPacket + Send + Sync>> =
                vec![Box::new(report), Box::new(sdes)];
            if source.current(c.generation) {
                match peer.write_rtcp(&packets).await {
                    Ok(_) => tracing::debug!(
                        packets = self.sent.load(Ordering::Relaxed),
                        "audio sender report sent"
                    ),
                    Err(error) => tracing::debug!(%error,"audio sender report failed"),
                }
            }
            due = Some(Instant::now() + Duration::from_secs_f64(2.5 + rand::random::<f64>() * 5.0));
        }
    }
    pub(crate) async fn send(&self, source: &impl Source, track: Arc<TrackLocalStaticRTP>) {
        let wall_clock = (Instant::now(), std::time::SystemTime::now());
        let mut sequence = rand::random::<u16>();
        let origin = rand::random::<u32>();
        let mut generation = u64::MAX;
        let mut encoder = None;
        let mut encoding: Option<Encoding> = None;
        let mut pcm = Vec::with_capacity(RATE as usize * 2 * 120 / 1000);
        let mut first_timestamp = 0;
        let mut first_capture = Instant::now();
        let mut next_timestamp = 0;
        let mut marker = true;
        let mut output = [0u8; super::encoder::MAX_PACKET];
        loop {
            let wake = source.wake().notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if source.stopped().load(Ordering::Acquire) {
                break;
            }
            let Some(frame) = source.frames().pop() else {
                wake.await;
                continue;
            };
            if !source.current(frame.generation)
                || frame.created.elapsed() > Duration::from_millis(100)
            {
                continue;
            }
            let next_encoding = source.encoding();
            if generation != frame.generation
                || !encoding
                    .zip(next_encoding)
                    .is_some_and(|(old, new)| old.same_stream(new))
            {
                generation = frame.generation;
                encoding = next_encoding;
                let Some(config) = encoding else {
                    continue;
                };
                encoder = match make_encoder(config) {
                    Ok(e) => Some(e),
                    Err(e) => {
                        source.fault(format!("音频编码初始化失败：{e:?}"));
                        None
                    }
                };
                pcm.clear();
                marker = true;
            } else if encoding != next_encoding {
                let Some(config) = next_encoding else {
                    continue;
                };
                if let Some(encoder) = encoder.as_mut() {
                    if let Err(error) =
                        encoder.set_bitrate(opusic_c::Bitrate::Value(config.bitrate))
                    {
                        source.fault(format!("音频码率调整失败：{error:?}"));
                        continue;
                    }
                }
                encoding = Some(config);
            }
            let (Some(config), Some(encoder)) = (encoding, encoder.as_mut()) else {
                continue;
            };
            if !pcm.is_empty() && frame.timestamp != next_timestamp {
                pcm.clear();
                marker = true;
            }
            if pcm.is_empty() {
                first_timestamp = frame.timestamp;
                first_capture = frame.created;
            }
            next_timestamp = frame.timestamp.wrapping_add(BLOCK as u32);
            if config.stereo {
                pcm.extend_from_slice(&frame.samples);
            } else {
                pcm.extend(frame.samples.chunks_exact(2).map(|p| (p[0] + p[1]) * 0.5));
            }
            let channels = if config.stereo { 2 } else { 1 };
            if pcm.len() < RATE as usize * config.packet_ms as usize / 1000 * channels {
                continue;
            }
            let loss = self.loss.load(Ordering::Relaxed).min(100) as u8;
            let encoded = encoder
                .set_packet_loss(loss)
                .and_then(|_| encoder.encode_float_to_slice(&pcm, &mut output));
            let rms = (pcm.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>()
                / pcm.len().max(1) as f64)
                .sqrt();
            let level = if rms > 0.0 {
                (-20.0 * rms.log10()).round().clamp(0.0, 127.0) as u8
            } else {
                127
            };
            pcm.clear();
            let length = match encoded {
                Ok(n) => n,
                Err(e) => {
                    source.fault(format!("音频编码失败：{e:?}"));
                    continue;
                }
            };
            if !source.current(generation) || track.all_binding_paused().await {
                marker = true;
                continue;
            }
            let packet = Packet {
                header: webrtc::rtp::header::Header {
                    version: 2,
                    marker,
                    sequence_number: sequence,
                    timestamp: origin.wrapping_add(first_timestamp),
                    ..Default::default()
                },
                payload: Bytes::copy_from_slice(&output[..length]),
            };
            use webrtc::rtp::extension::{
                HeaderExtension, abs_send_time_extension::AbsSendTimeExtension,
                audio_level_extension::AudioLevelExtension,
            };
            let extensions = [
                HeaderExtension::AudioLevel(AudioLevelExtension {
                    level,
                    voice: false,
                }),
                HeaderExtension::AbsSendTime(AbsSendTimeExtension::new(
                    wall_clock.1 + wall_clock.0.elapsed(),
                )),
            ];
            match track.write_rtp_with_extensions(&packet, &extensions).await {
                Ok(n) if n > 0 => {
                    self.sent.fetch_add(1, Ordering::Relaxed);
                    self.octets.fetch_add(length as u64, Ordering::Relaxed);
                    let mut clock = lock(&self.sent_clock);
                    let now = Instant::now();
                    let first = clock
                        .filter(|c| c.generation == generation)
                        .map_or(now, |c| c.first);
                    *clock = Some(SentClock {
                        timestamp: packet.header.timestamp,
                        at: first_capture,
                        generation,
                        first,
                    });
                    drop(clock);
                    source.report_wake().notify_one();
                    marker = false;
                }
                Ok(_) => marker = true,
                Err(e) => {
                    source.send_error(format!("音频发送失败：{e}"));
                    marker = true;
                }
            }
            // A failed write may already have encrypted/submitted the packet.
            // Never reuse its SRTP sequence number on a later payload.
            sequence = sequence.wrapping_add(1);
        }
    }
}
