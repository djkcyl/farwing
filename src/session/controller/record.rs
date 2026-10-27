//! The viewer's part of a connection record (`docs/connection-records.md`):
//! the visit that keeps a window's reconnections in one record, and the
//! summary of what it showed, sent when the window closes or switches to
//! another device.

use crate::account::client::AuthenticatedClient;
use crate::diagnostics::performance::PerformanceMonitor;
use crate::features::stream_control::StreamControlHandle;
use farwing_protocol::api::{DirectRecord, RecordReport, ViewerReport};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

const SAMPLE: Duration = Duration::from_millis(250);
const SAMPLES_PER_SECOND: u32 = 4;
/// A picture held this long while video arrives is a stall (as the
/// viewer's stall hint counts them).
const STALL: Duration = Duration::from_millis(300);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// A new visit.
pub(crate) fn new_visit() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[derive(Default)]
struct Tally {
    /// Samples with a picture, per path.
    routes: HashMap<&'static str, u32>,
    relay: &'static str,
    samples: u32,
    fps: f64,
    rtt: f64,
    rtt_samples: u32,
    width: u32,
    height: u32,
    stalls: u32,
    stalling: bool,
    reconnects: u32,
}

impl Tally {
    fn sample(&mut self, monitor: &PerformanceMonitor, forwarded: bool) {
        let snapshot = monitor.snapshot();
        let Some((width, height)) = snapshot.decoded_resolution else {
            return;
        };
        // `LAN · UDP`, `P2P · UDP`, `TURN · TLS`, `SFU · E2EE`.
        let connection = snapshot.connection.as_str();
        let route = if forwarded || connection.starts_with("SFU") {
            "forwarded"
        } else if connection.starts_with("TURN") {
            "relay"
        } else if connection.starts_with("LAN") {
            "lan"
        } else if connection.starts_with("P2P") {
            "p2p"
        } else {
            return;
        };
        if route == "relay" {
            self.relay = if connection.ends_with("TLS") {
                "tls"
            } else if connection.ends_with("TCP") {
                "tcp"
            } else {
                "udp"
            };
        }
        *self.routes.entry(route).or_default() += 1;
        self.samples += 1;
        self.fps += snapshot.actual_fps;
        if let Some(rtt) = snapshot.current_delay_ms.filter(|rtt| *rtt > 0.0) {
            self.rtt += rtt;
            self.rtt_samples += 1;
        }
        (self.width, self.height) = (width, height);
        let held = monitor.picture_age().is_some_and(|age| age >= STALL);
        let stalled =
            held && monitor.switch_pending().is_none() && monitor.video_arrived_within(STALL / 2);
        if stalled && !self.stalling {
            self.stalls += 1;
        }
        self.stalling = held && (self.stalling || stalled);
    }

    fn report(&self) -> ViewerReport {
        let route = self
            .routes
            .iter()
            .max_by_key(|(_, n)| **n)
            .map(|(route, _)| *route)
            .unwrap_or_default();
        let samples = f64::from(self.samples.max(1));
        ViewerReport {
            route: route.to_owned(),
            relay: if route == "relay" { self.relay.to_owned() } else { String::new() },
            seconds: self.samples / SAMPLES_PER_SECOND,
            width: self.width,
            height: self.height,
            fps: ((self.fps / samples) * 10.0).round() as f32 / 10.0,
            rtt_ms: (self.rtt / f64::from(self.rtt_samples.max(1))).round() as u32,
            stalls: self.stalls,
            reconnects: self.reconnects,
        }
    }
}

/// Where the summary goes.
struct Bound {
    /// The account or visitor the connection was made as; a direct
    /// connection reports as the account this device is signed in to.
    client: Option<Arc<AuthenticatedClient>>,
    report: RecordReport,
}

/// One visit's summary, sampled from the window's current connection.
pub(super) struct VisitRecord {
    tally: Arc<Mutex<Tally>>,
    control: Arc<Mutex<Option<StreamControlHandle>>>,
    bound: Mutex<Option<Bound>>,
    task: tokio::task::JoinHandle<()>,
}

impl VisitRecord {
    pub(super) fn start(
        mut monitor: watch::Receiver<Option<PerformanceMonitor>>,
    ) -> Self {
        let tally = Arc::new(Mutex::new(Tally::default()));
        let control = Arc::new(Mutex::new(None::<StreamControlHandle>));
        let task = {
            let (tally, control) = (Arc::clone(&tally), Arc::clone(&control));
            tokio::spawn(async move {
                let mut logged = 0u32;
                let mut tick = tokio::time::interval(SAMPLE);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tick.tick().await;
                    if monitor.has_changed().is_err() {
                        break;
                    }
                    let Some(current) = monitor.borrow_and_update().clone() else {
                        continue;
                    };
                    let forwarded = lock(&control)
                        .as_ref()
                        .is_some_and(|c| c.forwarded_e2ee().is_some());
                    lock(&tally).sample(&current, forwarded);
                    logged += 1;
                    if logged % (SAMPLES_PER_SECOND * 2) == 0 {
                        let s = current.snapshot();
                        let audio = lock(&control).as_ref().is_some_and(|c| c.audio().snapshot().receiving);
                        tracing::debug!(target: "farwing::viewer_rate", mbps = s.bitrate_mbps,
                            fps = s.actual_fps, size = ?s.decoded_resolution, route = %s.connection,
                            forwarded, audio, "viewer rate");
                    }
                }
            })
        };
        Self {
            tally,
            control,
            bound: Mutex::new(None),
            task,
        }
    }

    /// The session now playing.
    pub(super) fn connected(&self, control: &StreamControlHandle) {
        *lock(&self.control) = Some(control.clone());
    }

    /// The connection was lost and is being rebuilt.
    pub(super) fn reconnecting(&self) {
        lock(&self.tally).reconnects += 1;
    }

    /// Who the summary is sent as and about: the visit, both devices, and
    /// for a connection without the server, what the record needs.
    pub(super) fn bind(
        &self,
        client: Option<Arc<AuthenticatedClient>>,
        visit: &str,
        host_device_id: &str,
        controller_device_id: &str,
        direct: Option<DirectRecord>,
    ) {
        *lock(&self.bound) = Some(Bound {
            client,
            report: RecordReport {
                visit: visit.to_owned(),
                host_device_id: host_device_id.to_owned(),
                controller_device_id: controller_device_id.to_owned(),
                direct,
                ..RecordReport::default()
            },
        });
    }

    /// Send the summary (a window that showed nothing sends none).
    pub(super) async fn finish(self) {
        self.task.abort();
        let viewer = lock(&self.tally).report();
        let Some(Bound { client, mut report }) = lock(&self.bound).take() else {
            return;
        };
        if viewer.seconds == 0 {
            return;
        }
        let client = match client {
            Some(client) => client,
            None => match AuthenticatedClient::from_saved_session() {
                Ok(client) if !client.is_guest() => Arc::new(client),
                _ => return,
            },
        };
        if let Some(direct) = &mut report.direct {
            direct.ended_at = chrono::Utc::now().timestamp();
        }
        report.viewer = Some(viewer);
        let sent = tokio::time::timeout(
            SEND_TIMEOUT,
            client.request(|api| async move { api.record_report(&report).await }),
        )
        .await;
        match sent {
            Ok(Ok(())) => tracing::debug!("connection record summary sent"),
            Ok(Err(error)) => tracing::info!(%error, "connection record summary not sent"),
            Err(_) => tracing::info!("connection record summary timed out"),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_names_the_longest_path() {
        let mut tally = Tally::default();
        tally.routes.insert("p2p", 40);
        tally.routes.insert("relay", 8);
        tally.relay = "tls";
        tally.samples = 48;
        tally.fps = 48.0 * 59.94;
        tally.rtt = 4.0 * 30.0;
        tally.rtt_samples = 4;
        tally.stalls = 2;
        let report = tally.report();
        assert_eq!(report.route, "p2p");
        assert_eq!(report.relay, "");
        assert_eq!(report.seconds, 12);
        assert_eq!(report.fps, 59.9);
        assert_eq!(report.rtt_ms, 30);
        assert_eq!(report.stalls, 2);
    }
}
