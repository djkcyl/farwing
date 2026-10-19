use super::*;

pub(super) struct ReceiverStream {
    parent_rtp_reader: Arc<dyn RTPReader + Send + Sync>,
    hdr_ext_id: u8,
    ssrc: u32,
    recorder: Arc<SyncMutex<Recorder>>,
    // we use tokio's Instant because it makes testing easier via `tokio::time::advance`.
    start_time: tokio::time::Instant,
}

impl ReceiverStream {
    pub(super) fn new(
        parent_rtp_reader: Arc<dyn RTPReader + Send + Sync>,
        hdr_ext_id: u8,
        ssrc: u32,
        recorder: Arc<SyncMutex<Recorder>>,
        start_time: tokio::time::Instant,
    ) -> Self {
        ReceiverStream {
            parent_rtp_reader,
            hdr_ext_id,
            ssrc,
            recorder,
            start_time,
        }
    }
}

#[async_trait]
impl RTPReader for ReceiverStream {
    /// read a rtp packet
    async fn read(
        &self,
        buf: &mut [u8],
        attributes: &Attributes,
    ) -> Result<(rtp::packet::Packet, Attributes)> {
        let (pkt, attr) = self.parent_rtp_reader.read(buf, attributes).await?;

        if let Some(mut ext) = pkt.header.get_extension(self.hdr_ext_id) {
            // A malformed optional extension must not terminate RTP delivery.
            if let Ok(tcc_ext) = TransportCcExtension::unmarshal(&mut ext) {
                // Custom RTPReaders may lack metadata; SRTP always provides it.
                let arrival = attr
                    .rtp_received_at
                    .unwrap_or_else(|| tokio::time::Instant::now().into_std());
                if let Some(elapsed) = arrival.checked_duration_since(self.start_time.into_std()) {
                    if let Ok(micros) = i64::try_from(elapsed.as_micros()) {
                        self.recorder
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .record(self.ssrc, tcc_ext.transport_sequence, micros);
                    }
                }
            }
        }

        Ok((pkt, attr))
    }
}
