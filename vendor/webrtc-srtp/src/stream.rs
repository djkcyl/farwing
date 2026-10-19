use bytes::Bytes;
use std::collections::VecDeque;
use std::time::Instant;
use tokio::sync::{mpsc, Mutex, Notify};
use util::marshal::*;
use util::Buffer;

use crate::error::{Error, Result};

/// Limit the buffer size to 1MB
pub const SRTP_BUFFER_SIZE: usize = 1000 * 1000;

/// Limit the buffer size to 100KB
pub const SRTCP_BUFFER_SIZE: usize = 100 * 1000;

/// Stream handles decryption for a single RTP/RTCP SSRC
#[derive(Debug)]
pub struct Stream {
    ssrc: u32,
    tx: mpsc::UnboundedSender<u32>,
    buffer: Buffer,
    rtp: Mutex<RtpQueue>,
    rtp_ready: Notify,
    is_rtp: bool,
}

/// Replaces the RTP byte-only buffer; bytes and their arrival are one queue
/// entry and are consumed atomically, including short-buffer/error paths.
#[derive(Debug, Default)]
struct RtpQueue {
    packets: VecDeque<(Bytes, Instant)>,
    bytes: usize,
    closed: bool,
}

impl Stream {
    /// Create a new stream
    pub fn new(ssrc: u32, tx: mpsc::UnboundedSender<u32>, is_rtp: bool) -> Self {
        Stream {
            ssrc,
            tx,
            // RTCP keeps the existing byte-only packet buffer. RTP uses the
            // same bounded capacity below, retaining its arrival metadata.
            buffer: Buffer::new(0, SRTCP_BUFFER_SIZE),
            rtp: Mutex::new(RtpQueue::default()),
            rtp_ready: Notify::new(),
            is_rtp,
        }
    }

    /// GetSSRC returns the SSRC we are demuxing for
    pub fn get_ssrc(&self) -> u32 {
        self.ssrc
    }

    /// Check if RTP is a stream.
    pub fn is_rtp_stream(&self) -> bool {
        self.is_rtp
    }

    /// Read reads and decrypts full RTP packet from the nextConn
    pub async fn read(&self, buf: &mut [u8]) -> Result<usize> {
        if self.is_rtp {
            Ok(self.read_datagram(buf).await?.0)
        } else {
            Ok(self.buffer.read(buf, None).await?)
        }
    }

    pub(crate) async fn write(&self, bytes: &Bytes, arrival: Instant) -> util::Result<usize> {
        if !self.is_rtp {
            return self.buffer.write(bytes).await;
        }
        if bytes.len() >= 0x10000 {
            return Err(util::Error::ErrPacketTooBig);
        }
        let mut queue = self.rtp.lock().await;
        if queue.closed {
            return Err(util::Error::ErrBufferClosed);
        }
        if queue.bytes + bytes.len() + 2 > SRTP_BUFFER_SIZE {
            return Err(util::Error::ErrBufferFull);
        }
        queue.bytes += bytes.len() + 2;
        queue.packets.push_back((bytes.clone(), arrival));
        self.rtp_ready.notify_one();
        Ok(bytes.len())
    }

    async fn read_datagram(&self, buf: &mut [u8]) -> Result<(usize, Instant)> {
        loop {
            let ready = self.rtp_ready.notified();
            tokio::pin!(ready);
            // Register before inspecting the queue: close wakes every reader.
            ready.as_mut().enable();
            {
                let mut queue = self.rtp.lock().await;
                if let Some((bytes, arrival)) = queue.packets.pop_front() {
                    queue.bytes -= bytes.len() + 2;
                    let copied = bytes.len().min(buf.len());
                    buf[..copied].copy_from_slice(&bytes[..copied]);
                    if copied != bytes.len() {
                        return Err(util::Error::ErrBufferShort.into());
                    }
                    return Ok((copied, arrival));
                }
                if queue.closed {
                    return Err(util::Error::ErrBufferClosed.into());
                }
            }
            ready.await;
        }
    }

    /// ReadRTP reads and decrypts full RTP packet and its header from the nextConn
    pub async fn read_rtp(&self, buf: &mut [u8]) -> Result<rtp::packet::Packet> {
        Ok(self.read_rtp_with_arrival(buf).await?.0)
    }

    pub async fn read_rtp_with_arrival(
        &self,
        buf: &mut [u8],
    ) -> Result<(rtp::packet::Packet, Instant)> {
        if !self.is_rtp {
            return Err(Error::InvalidRtpStream);
        }

        let (n, arrival) = self.read_datagram(buf).await?;
        let mut b = &buf[..n];
        let pkt = rtp::packet::Packet::unmarshal(&mut b)?;

        Ok((pkt, arrival))
    }

    /// read_rtcp reads and decrypts full RTP packet and its header from the nextConn
    pub async fn read_rtcp(
        &self,
        buf: &mut [u8],
    ) -> Result<Vec<Box<dyn rtcp::packet::Packet + Send + Sync>>> {
        if self.is_rtp {
            return Err(Error::InvalidRtcpStream);
        }

        let n = self.buffer.read(buf, None).await?;
        let mut b = &buf[..n];
        let pkt = rtcp::packet::unmarshal(&mut b)?;

        Ok(pkt)
    }

    /// Close removes the ReadStream from the session and cleans up any associated state
    pub async fn close(&self) -> Result<()> {
        if self.close_buffer().await {
            // One lifecycle notification per Stream, not a packet queue.
            let _ = self.tx.send(self.ssrc);
        }
        Ok(())
    }

    pub(crate) async fn close_buffer(&self) -> bool {
        let mut queue = self.rtp.lock().await;
        let was_closed = queue.closed;
        queue.closed = true;
        drop(queue);
        self.rtp_ready.notify_waiters();
        self.buffer.close().await;
        !was_closed
    }
}
