#[cfg(test)]
mod session_rtcp_test;
#[cfg(test)]
mod session_rtp_test;

use std::collections::{HashMap, HashSet};
use std::marker::{Send, Sync};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use tokio::sync::{broadcast, mpsc, Mutex};
use util::conn::Conn;
use util::marshal::*;

use crate::config::*;
use crate::context::*;
use crate::error::{Error, Result};
use crate::option::*;
use crate::stream::*;

const DEFAULT_SESSION_SRTP_REPLAY_PROTECTION_WINDOW: usize = 64;
const DEFAULT_SESSION_SRTCP_REPLAY_PROTECTION_WINDOW: usize = 64;
const ORDERED_RTP_INGRESS_CAPACITY: usize = 32 * 1024;

// None prevents a late open from installing an unreadable stream after exit.
type StreamsMap = Arc<Mutex<Option<HashMap<u32, Arc<Stream>>>>>;

/// One decrypted RTP datagram in the exact order it was accepted by the SRTP
/// session. This is an observation stream: normal per-SSRC streams and their
/// interceptor chains remain unchanged.
#[derive(Clone, Debug)]
pub struct IncomingRtpPacket {
    pub ordinal: u64,
    pub received_at: Instant,
    pub data: Bytes,
}

/// Session implements io.ReadWriteCloser and provides a bi-directional SRTP session
/// SRTP itself does not have a design like this, but it is common in most applications
/// for local/remote to each have their own keying material. This provides those patterns
/// instead of making everyone re-implement
pub struct Session {
    local_context: Arc<Mutex<Context>>,
    streams_map: StreamsMap,
    #[allow(clippy::type_complexity)]
    new_stream_rx: Arc<Mutex<mpsc::Receiver<(Arc<Stream>, Option<rtp::header::Header>)>>>,
    close_stream_tx: mpsc::UnboundedSender<u32>,
    close_session_tx: mpsc::Sender<()>,
    pub(crate) udp_tx: Arc<dyn Conn + Send + Sync>,
    is_rtp: bool,
    incoming_rtp_tx: Option<broadcast::WeakSender<IncomingRtpPacket>>,
}

impl Session {
    pub async fn new(
        conn: Arc<dyn Conn + Send + Sync>,
        config: Config,
        is_rtp: bool,
    ) -> Result<Self> {
        let local_context = Context::new(
            &config.keys.local_master_key,
            &config.keys.local_master_salt,
            config.profile,
            config.local_rtp_options,
            config.local_rtcp_options,
        )?;

        let mut remote_context = Context::new(
            &config.keys.remote_master_key,
            &config.keys.remote_master_salt,
            config.profile,
            if config.remote_rtp_options.is_none() {
                Some(srtp_replay_protection(
                    DEFAULT_SESSION_SRTP_REPLAY_PROTECTION_WINDOW,
                ))
            } else {
                config.remote_rtp_options
            },
            if config.remote_rtcp_options.is_none() {
                Some(srtcp_replay_protection(
                    DEFAULT_SESSION_SRTCP_REPLAY_PROTECTION_WINDOW,
                ))
            } else {
                config.remote_rtcp_options
            },
        )?;

        let streams_map = Arc::new(Mutex::new(Some(HashMap::new())));
        let (mut new_stream_tx, new_stream_rx) = mpsc::channel(8);
        // Stream close must not wait behind a full new-stream notification:
        // transceivers stop before Session::close in the owner cleanup chain.
        let (close_stream_tx, mut close_stream_rx) = mpsc::unbounded_channel();
        let (close_session_tx, mut close_session_rx) = mpsc::channel(1);
        let udp_tx = Arc::clone(&conn);
        let udp_rx = Arc::clone(&conn);
        let cloned_streams_map = Arc::clone(&streams_map);
        let cloned_close_stream_tx = close_stream_tx.clone();
        let incoming_rtp_tx = is_rtp.then(|| {
            let (sender, _) = broadcast::channel(ORDERED_RTP_INGRESS_CAPACITY);
            sender
        });
        let task_incoming_rtp_tx = incoming_rtp_tx.clone();
        let arrival_ordinal = Arc::new(AtomicU64::new(0));
        let task_arrival_ordinal = Arc::clone(&arrival_ordinal);

        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];

            loop {
                let result = tokio::select! {
                    biased;
                    _ = close_session_rx.recv() => break,
                    Some(ssrc) = close_stream_rx.recv() => {
                        Session::close_stream(&cloned_streams_map, ssrc).await;
                        continue;
                    },
                    result = udp_rx.recv_with_timestamp(&mut buf) => result,
                };
                let (n, received_at) = match result {
                    Ok((0, _)) => continue,
                    Ok(packet) => packet,
                    Err(util::Error::ErrBufferShort) => continue,
                    Err(err) => {
                        log::debug!("SRTP transport receive ended: {err}");
                        break;
                    }
                };
                let incoming_stream = Session::incoming(
                    &buf[..n],
                    received_at,
                    &cloned_streams_map,
                    &cloned_close_stream_tx,
                    &mut new_stream_tx,
                    &mut remote_context,
                    is_rtp,
                    task_incoming_rtp_tx.as_ref(),
                    &task_arrival_ordinal,
                );
                tokio::select! {
                    biased;
                    _ = close_session_rx.recv() => break,
                    result = incoming_stream => match result{
                        Ok(()) => {},
                        Err(err) => log::info!("{err}"),
                    },
                }
            }
            // Do not enqueue close notifications back into this reader during
            // cleanup. Wake per-SSRC reads; dropping the task also ends accept
            // and the ordered RTP subscription, even if Session is still held.
            drop(close_stream_rx);
            let remaining = cloned_streams_map.lock().await.take();
            if let Some(remaining) = remaining {
                for stream in remaining.into_values() {
                    stream.close_buffer().await;
                }
            }
        });

        Ok(Session {
            local_context: Arc::new(Mutex::new(local_context)),
            streams_map,
            new_stream_rx: Arc::new(Mutex::new(new_stream_rx)),
            close_stream_tx,
            close_session_tx,
            udp_tx,
            is_rtp,
            incoming_rtp_tx: incoming_rtp_tx.as_ref().map(broadcast::Sender::downgrade),
        })
    }

    async fn close_stream(streams_map: &StreamsMap, ssrc: u32) {
        let mut streams = streams_map.lock().await;
        if let Some(streams) = streams.as_mut() {
            streams.remove(&ssrc);
        }
    }

    async fn incoming(
        buf: &[u8],
        received_at: Instant,
        streams_map: &StreamsMap,
        close_stream_tx: &mpsc::UnboundedSender<u32>,
        new_stream_tx: &mut mpsc::Sender<(Arc<Stream>, Option<rtp::header::Header>)>,
        remote_context: &mut Context,
        is_rtp: bool,
        incoming_rtp_tx: Option<&broadcast::Sender<IncomingRtpPacket>>,
        arrival_ordinal: &AtomicU64,
    ) -> Result<()> {
        let decrypted = if is_rtp {
            remote_context.decrypt_rtp(buf)?
        } else {
            remote_context.decrypt_rtcp(buf)?
        };

        let mut buf = &decrypted[..];
        let (ssrcs, header) = if is_rtp {
            let header = rtp::header::Header::unmarshal(&mut buf)?;
            (vec![header.ssrc], Some(header))
        } else {
            let pkts = rtcp::packet::unmarshal(&mut buf)?;
            (destination_ssrc(&pkts), None)
        };

        if let Some(sender) = incoming_rtp_tx {
            let _ = sender.send(IncomingRtpPacket {
                ordinal: arrival_ordinal.fetch_add(1, Ordering::Relaxed),
                received_at,
                data: decrypted.clone(),
            });
        }

        for ssrc in ssrcs {
            let (stream, is_new) =
                Session::get_or_create_stream(streams_map, close_stream_tx.clone(), is_rtp, ssrc)
                    .await;

            if is_new {
                log::trace!(
                    "srtp session got new {} stream {}",
                    if is_rtp { "rtp" } else { "rtcp" },
                    ssrc
                );
                new_stream_tx
                    .send((Arc::clone(&stream), header.clone()))
                    .await?;
            }

            match stream.write(&decrypted, received_at).await {
                Ok(_) => {}
                Err(err) => {
                    // Silently drop data when the buffer is full.
                    if util::Error::ErrBufferFull != err {
                        return Err(err.into());
                    }
                }
            }
        }

        Ok(())
    }

    async fn get_or_create_stream(
        streams_map: &StreamsMap,
        close_stream_tx: mpsc::UnboundedSender<u32>,
        is_rtp: bool,
        ssrc: u32,
    ) -> (Arc<Stream>, bool) {
        let mut streams = streams_map.lock().await;
        let Some(streams) = streams.as_mut() else {
            let stream = Arc::new(Stream::new(ssrc, close_stream_tx, is_rtp));
            stream.close_buffer().await;
            return (stream, false);
        };

        if let Some(stream) = streams.get(&ssrc) {
            (Arc::clone(stream), false)
        } else {
            let stream = Arc::new(Stream::new(ssrc, close_stream_tx, is_rtp));
            streams.insert(ssrc, Arc::clone(&stream));
            (stream, true)
        }
    }

    /// open on the given SSRC to create a stream, it can be used
    /// if you want a certain SSRC, but don't want to wait for Accept
    pub async fn open(&self, ssrc: u32) -> Arc<Stream> {
        let (stream, _) = Session::get_or_create_stream(
            &self.streams_map,
            self.close_stream_tx.clone(),
            self.is_rtp,
            ssrc,
        )
        .await;

        stream
    }

    /// accept returns a stream to handle RTCP for a single SSRC
    pub async fn accept(&self) -> Result<(Arc<Stream>, Option<rtp::header::Header>)> {
        let mut new_stream_rx = self.new_stream_rx.lock().await;

        new_stream_rx
            .recv()
            .await
            .ok_or(Error::SessionSrtpAlreadyClosed)
    }

    /// Subscribe to decrypted RTP datagrams before SSRC-specific consumers
    /// reorder them. RTCP sessions do not expose this stream.
    pub fn subscribe_incoming_rtp(&self) -> Option<broadcast::Receiver<IncomingRtpPacket>> {
        self.incoming_rtp_tx
            .as_ref()
            .and_then(broadcast::WeakSender::upgrade)
            .map(|sender| sender.subscribe())
    }

    pub async fn close(&self) -> Result<()> {
        let _ = self.close_session_tx.try_send(());
        self.close_session_tx.closed().await;
        Ok(())
    }

    pub async fn write(&self, buf: &Bytes, is_rtp: bool) -> Result<usize> {
        if self.close_session_tx.is_closed() {
            return Err(Error::SessionSrtpAlreadyClosed);
        }
        if self.is_rtp != is_rtp {
            return Err(Error::SessionRtpRtcpTypeMismatch);
        }

        let encrypted = {
            let mut local_context = self.local_context.lock().await;

            if is_rtp {
                local_context.encrypt_rtp(buf)?
            } else {
                local_context.encrypt_rtcp(buf)?
            }
        };

        Ok(self.udp_tx.send(&encrypted).await?)
    }

    pub async fn write_rtp(&self, pkt: &rtp::packet::Packet) -> Result<usize> {
        let raw = pkt.marshal()?;
        self.write(&raw, true).await
    }

    pub async fn write_rtcp(
        &self,
        pkt: &(dyn rtcp::packet::Packet + Send + Sync),
    ) -> Result<usize> {
        let raw = pkt.marshal()?;
        self.write(&raw, false).await
    }
}

/// create a list of Destination SSRCs
/// that's a superset of all Destinations in the slice.
fn destination_ssrc(pkts: &[Box<dyn rtcp::packet::Packet + Send + Sync>]) -> Vec<u32> {
    let mut ssrc_set = HashSet::new();
    for p in pkts {
        // XR RRTR has no report-block destination; DLRR names a local SSRC.
        // Receive modules subscribe to the remote sender SSRC, and validate
        // each DLRR sub-block against their registered local SSRC themselves.
        if let Some(xr) = p
            .as_any()
            .downcast_ref::<rtcp::extended_report::ExtendedReport>()
        {
            ssrc_set.insert(xr.sender_ssrc);
        }
        for ssrc in p.destination_ssrc() {
            ssrc_set.insert(ssrc);
        }
    }
    ssrc_set.into_iter().collect()
}
