use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{mpsc, Mutex};
use waitgroup::WaitGroup;

pub mod receiver;
pub mod sender;

use receiver::{ReceiverReport, ReceiverReportInternal};
use sender::{SenderReport, SenderReportInternal};

use crate::error::Result;
use crate::{Interceptor, InterceptorBuilder};

type FnTimeGen = Arc<dyn Fn() -> SystemTime + Sync + 'static + Send>;

/// ReceiverBuilder can be used to configure ReceiverReport Interceptor.
#[derive(Default)]
pub struct ReportBuilder {
    is_rr: bool,
    interval: Option<Duration>,
    now: Option<FnTimeGen>,
    media_intervals: Option<(Duration, Duration)>,
}

impl ReportBuilder {
    /// Schedule each receive stream independently: half the base period for
    /// its first report, then a random interval in [0.5, 1.5] times its base.
    pub fn with_jittered_media_intervals(mut self, video: Duration, audio: Duration) -> Self {
        self.media_intervals = Some((video, audio));
        self
    }

    /// with_interval sets send interval for the interceptor.
    pub fn with_interval(mut self, interval: Duration) -> ReportBuilder {
        self.interval = Some(interval);
        self
    }

    /// with_now_fn sets an alternative for the time.Now function.
    pub fn with_now_fn(mut self, now: FnTimeGen) -> ReportBuilder {
        self.now = Some(now);
        self
    }

    fn build_rr(&self) -> ReceiverReport {
        let (close_tx, close_rx) = mpsc::channel(1);
        // The same Clock is used for RTP statistics and the eight-second
        // report expiry. SR receipt and DLSR must use that clock too: mixing
        // anchored RTP time with live wall time makes an OS clock adjustment
        // look like media inactivity or RTCP delay.
        let now = self.now.clone().unwrap_or_else(|| {
            let anchor = (Instant::now(), SystemTime::now());
            Arc::new(move || anchor.1 + anchor.0.elapsed())
        });
        ReceiverReport {
            internal: Arc::new(ReceiverReportInternal {
                interval: if let Some(interval) = &self.interval {
                    *interval
                } else {
                    Duration::from_secs(1)
                },
                now: Some(now),
                streams: Mutex::new(HashMap::new()),
                close_rx: Mutex::new(Some(close_rx)),
                media_intervals: self.media_intervals,
                streams_changed: tokio::sync::Notify::new(),
            }),

            wg: Mutex::new(Some(WaitGroup::new())),
            close_tx: Mutex::new(Some(close_tx)),
        }
    }

    fn build_sr(&self) -> SenderReport {
        let (close_tx, close_rx) = mpsc::channel(1);
        SenderReport {
            internal: Arc::new(SenderReportInternal {
                interval: if let Some(interval) = &self.interval {
                    *interval
                } else {
                    Duration::from_secs(1)
                },
                now: self.now.clone(),
                streams: Mutex::new(HashMap::new()),
                close_rx: Mutex::new(Some(close_rx)),
            }),

            wg: Mutex::new(Some(WaitGroup::new())),
            close_tx: Mutex::new(Some(close_tx)),
        }
    }
}

impl InterceptorBuilder for ReportBuilder {
    fn build(&self, _id: &str) -> Result<Arc<dyn Interceptor + Send + Sync>> {
        if self.is_rr {
            Ok(Arc::new(self.build_rr()))
        } else {
            Ok(Arc::new(self.build_sr()))
        }
    }
}
