use async_trait::async_trait;
use srtp::stream::Stream;

use crate::error::Result;
use crate::{Attributes, RTCPReader, RTPReader};

#[async_trait]
impl RTPReader for Stream {
    async fn read(
        &self,
        buf: &mut [u8],
        a: &Attributes,
    ) -> Result<(rtp::packet::Packet, Attributes)> {
        let (packet, arrival) = self.read_rtp_with_arrival(buf).await?;
        let mut attributes = a.clone();
        attributes.rtp_received_at = Some(arrival);
        Ok((packet, attributes))
    }
}

#[async_trait]
impl RTCPReader for Stream {
    async fn read(
        &self,
        buf: &mut [u8],
        a: &Attributes,
    ) -> Result<(Vec<Box<dyn rtcp::packet::Packet + Send + Sync>>, Attributes)> {
        Ok((self.read_rtcp(buf).await?, a.clone()))
    }
}
