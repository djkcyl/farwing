mod receiver_stream;
#[cfg(test)]
mod receiver_test;

use std::sync::Mutex as SyncMutex;
use std::time::Duration;

use receiver_stream::ReceiverStream;
use rtp::extension::transport_cc_extension::TransportCcExtension;
use tokio::sync::{mpsc, watch, Mutex};
use util::Unmarshal;
use waitgroup::WaitGroup;

use crate::twcc::sender::TRANSPORT_CC_URI;
use crate::twcc::Recorder;
use crate::*;

/// ReceiverBuilder is a InterceptorBuilder for a SenderInterceptor
#[derive(Default)]
pub struct ReceiverBuilder {
    interval: Option<Duration>,
    interval_updates: Option<watch::Receiver<Duration>>,
}

impl ReceiverBuilder {
    /// with_interval sets send interval for the interceptor.
    pub fn with_interval(mut self, interval: Duration) -> ReceiverBuilder {
        self.interval = Some(interval);
        self
    }

    /// Receive positive interval updates from the owning congestion controller.
    /// Its estimate need not wait for this endpoint to send its first RTP packet.
    pub fn with_interval_updates(mut self, updates: watch::Receiver<Duration>) -> Self {
        self.interval_updates = Some(updates);
        self
    }
}

impl InterceptorBuilder for ReceiverBuilder {
    fn build(&self, _id: &str) -> Result<Arc<dyn Interceptor + Send + Sync>> {
        let (close_tx, close_rx) = mpsc::channel(1);
        Ok(Arc::new(Receiver {
            internal: Arc::new(ReceiverInternal {
                interval: if let Some(interval) = &self.interval {
                    *interval
                } else {
                    Duration::from_millis(100)
                },
                recorder: Arc::new(SyncMutex::new(Recorder::default())),
                interval_updates: Mutex::new(self.interval_updates.clone()),
                streams: Mutex::new(HashMap::new()),
                close_rx: Mutex::new(Some(close_rx)),
            }),
            start_time: tokio::time::Instant::now(),
            wg: Mutex::new(Some(WaitGroup::new())),
            close_tx: Mutex::new(Some(close_tx)),
        }))
    }
}

struct ReceiverInternal {
    interval: Duration,
    interval_updates: Mutex<Option<watch::Receiver<Duration>>>,
    recorder: Arc<SyncMutex<Recorder>>,
    streams: Mutex<HashMap<u32, Arc<ReceiverStream>>>,
    close_rx: Mutex<Option<mpsc::Receiver<()>>>,
}

/// Receiver sends transport-wide congestion control reports as specified in:
/// <https://datatracker.ietf.org/doc/html/draft-holmer-rmcat-transport-wide-cc-extensions-01>
pub struct Receiver {
    internal: Arc<ReceiverInternal>,

    // we use tokio's Instant because it makes testing easier via `tokio::time::advance`.
    start_time: tokio::time::Instant,

    wg: Mutex<Option<WaitGroup>>,
    close_tx: Mutex<Option<mpsc::Sender<()>>>,
}

impl Receiver {
    /// builder returns a new ReceiverBuilder.
    pub fn builder() -> ReceiverBuilder {
        ReceiverBuilder::default()
    }

    async fn is_closed(&self) -> bool {
        let close_tx = self.close_tx.lock().await;
        close_tx.is_none()
    }

    async fn run(
        rtcp_writer: Arc<dyn RTCPWriter + Send + Sync>,
        internal: Arc<ReceiverInternal>,
    ) -> Result<()> {
        let mut close_rx = {
            let mut close_rx = internal.close_rx.lock().await;
            if let Some(close_rx) = close_rx.take() {
                close_rx
            } else {
                return Err(Error::ErrInvalidCloseRx);
            }
        };

        let a = Attributes::new();
        let mut updates = internal.interval_updates.lock().await.take();
        let mut interval = updates
            .as_mut()
            .map_or(internal.interval, |rx| *rx.borrow_and_update());
        assert!(
            !interval.is_zero(),
            "TWCC feedback interval must be positive"
        );
        let mut last_process = tokio::time::Instant::now();
        let mut deadline = last_process;
        loop {
            tokio::select! {
                biased;
                _ = close_rx.recv() =>{
                    return Ok(());
                }
                changed = async {
                    match updates.as_mut() {
                        Some(rx) => rx.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if changed.is_err() {
                        updates = None;
                    } else if let Some(rx) = updates.as_mut() {
                        interval = *rx.borrow_and_update();
                        assert!(!interval.is_zero(), "TWCC feedback interval must be positive");
                        // Preserve the last process time when the budget changes;
                        // do not postpone a now-due report by another full interval.
                        deadline = last_process + interval;
                    }
                }
                _ = tokio::time::sleep_until(deadline) =>{
                    last_process = tokio::time::Instant::now();
                    deadline = last_process + interval;
                    // build and send twcc
                    let pkts = {
                        let mut recorder = internal.recorder.lock().unwrap_or_else(|e| e.into_inner());
                        recorder.build_feedback_packet()
                    };

                    if pkts.is_empty() {
                        continue;
                    }

                    // No receive-side lock/channel is held across a route's
                    // potentially blocked write. Closing must cancel that write.
                    tokio::select! {
                        _ = close_rx.recv() => return Ok(()),
                        result = rtcp_writer.write(&pkts, &a) => {
                            if let Err(err) = result { log::warn!("TWCC feedback write failed: {err}"); }
                        }
                    }
                }
            }
        }
    }
}

#[async_trait]
impl Interceptor for Receiver {
    /// bind_rtcp_reader lets you modify any incoming RTCP packets. It is called once per sender/receiver, however this might
    /// change in the future. The returned method will be called once per packet batch.
    async fn bind_rtcp_reader(
        &self,
        reader: Arc<dyn RTCPReader + Send + Sync>,
    ) -> Arc<dyn RTCPReader + Send + Sync> {
        reader
    }

    /// bind_rtcp_writer lets you modify any outgoing RTCP packets. It is called once per PeerConnection. The returned method
    /// will be called once per packet batch.
    async fn bind_rtcp_writer(
        &self,
        writer: Arc<dyn RTCPWriter + Send + Sync>,
    ) -> Arc<dyn RTCPWriter + Send + Sync> {
        if self.is_closed().await {
            return writer;
        }

        {
            let mut recorder = self
                .internal
                .recorder
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            // The module writer supplies its negotiated RTCP sender identity.
            *recorder = Recorder::new(0);
        }

        let mut w = {
            let wait_group = self.wg.lock().await;
            wait_group.as_ref().map(|wg| wg.worker())
        };
        let writer2 = Arc::clone(&writer);
        let internal = Arc::clone(&self.internal);
        tokio::spawn(async move {
            let _d = w.take();
            if let Err(err) = Receiver::run(writer2, internal).await {
                log::warn!("bind_rtcp_writer TWCC Sender::run got error: {err}");
            }
        });

        writer
    }

    /// bind_local_stream lets you modify any outgoing RTP packets. It is called once for per LocalStream. The returned method
    /// will be called once per rtp packet.
    async fn bind_local_stream(
        &self,
        _info: &StreamInfo,
        writer: Arc<dyn RTPWriter + Send + Sync>,
    ) -> Arc<dyn RTPWriter + Send + Sync> {
        writer
    }

    /// unbind_local_stream is called when the Stream is removed. It can be used to clean up any data related to that track.
    async fn unbind_local_stream(&self, _info: &StreamInfo) {}

    /// bind_remote_stream lets you modify any incoming RTP packets. It is called once for per RemoteStream. The returned method
    /// will be called once per rtp packet.
    async fn bind_remote_stream(
        &self,
        info: &StreamInfo,
        reader: Arc<dyn RTPReader + Send + Sync>,
    ) -> Arc<dyn RTPReader + Send + Sync> {
        let mut hdr_ext_id = 0u8;
        for e in &info.rtp_header_extensions {
            if e.uri == TRANSPORT_CC_URI {
                hdr_ext_id = e.id as u8;
                break;
            }
        }
        if hdr_ext_id == 0 {
            // Don't try to read header extension if ID is 0, because 0 is an invalid extension ID
            return reader;
        }

        let stream = Arc::new(ReceiverStream::new(
            reader,
            hdr_ext_id,
            info.ssrc,
            Arc::clone(&self.internal.recorder),
            self.start_time,
        ));

        {
            let mut streams = self.internal.streams.lock().await;
            streams.insert(info.ssrc, Arc::clone(&stream));
        }

        stream
    }

    /// unbind_remote_stream is called when the Stream is removed. It can be used to clean up any data related to that track.
    async fn unbind_remote_stream(&self, info: &StreamInfo) {
        let mut streams = self.internal.streams.lock().await;
        streams.remove(&info.ssrc);
    }

    /// close closes the Interceptor, cleaning up any data if necessary.
    async fn close(&self) -> Result<()> {
        {
            let mut close_tx = self.close_tx.lock().await;
            close_tx.take();
        }

        {
            let mut wait_group = self.wg.lock().await;
            if let Some(wg) = wait_group.take() {
                wg.wait().await;
            }
        }

        Ok(())
    }
}
