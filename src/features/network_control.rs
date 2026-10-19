//! Viewer commands for the existing signaling/ICE owner, not another connection loop.
use anyhow::{Result, bail};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

#[derive(Clone, Debug, Default)]
pub struct NetworkControlSnapshot {
    pub round_trip_ms: Option<f64>,
    pub relay_enabled: bool,
    pub pending: bool,
    pub available: bool,
    pub unavailable_reason: Option<&'static str>,
    pub error: Option<String>,
    pub notice: Option<&'static str>,
}

#[derive(Default)]
struct State {
    view: NetworkControlSnapshot,
    connected: bool,
    round_trip: Option<(Duration, Instant)>,
    forced: bool,
    has_turns: bool,
    last_request: Option<Instant>,
    waiting_since: Option<Instant>,
    notice_until: Option<Instant>,
}

#[derive(Clone)]
pub(crate) struct NetworkControl {
    state: Arc<Mutex<State>>,
    requests: watch::Sender<Option<bool>>,
}

impl NetworkControl {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            requests: watch::channel(None).0,
        }
    }

    pub(crate) fn configure(&self, relay: bool, forced: bool, has_turns: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.view.relay_enabled = relay;
        state.forced = forced;
        state.has_turns = has_turns;
    }

    pub(crate) fn connected(&self, connected: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.connected = connected;
        if !connected {
            state.round_trip = None;
        }
    }

    /// Selected ICE candidate-pair RTT, independent of any video receiver.
    pub(crate) fn observe_rtt(&self, delay: Option<Duration>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.round_trip = delay
            .filter(|_| state.connected)
            .map(|value| (value, Instant::now()));
    }

    pub(crate) fn snapshot(&self) -> NetworkControlSnapshot {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .notice_until
            .is_some_and(|until| Instant::now() >= until)
        {
            state.view.notice = None;
            state.notice_until = None;
        }
        state.view.unavailable_reason = if !state.connected {
            Some("媒体通道连接后可切换")
        } else if state.forced {
            Some("本次连接要求使用中转")
        } else if !state.has_turns && !state.view.relay_enabled {
            Some("本次会话未提供高速中转线路")
        } else {
            None
        };
        state.view.available = state.view.unavailable_reason.is_none() && !state.view.pending;
        state.view.round_trip_ms = state
            .round_trip
            .filter(|(_, at)| state.connected && at.elapsed() < Duration::from_secs(3))
            .map(|(delay, _)| delay.as_secs_f64() * 1000.0);
        state.view.clone()
    }

    pub(crate) fn request(&self, relay: bool) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.view.pending {
            bail!("正在切换线路");
        }
        if !state.connected {
            bail!("媒体通道尚未连接");
        }
        if state.forced && !relay {
            bail!("本次连接要求使用中转");
        }
        if relay && !state.has_turns {
            bail!("本次会话未提供高速中转线路");
        }
        if state
            .last_request
            .is_some_and(|at| at.elapsed() < Duration::from_secs(5))
        {
            bail!("切换过于频繁，请稍后再试");
        }
        state.last_request = Some(Instant::now());
        state.view.pending = true;
        state.view.error = None;
        state.view.notice = None;
        state.notice_until = None;
        state.waiting_since = None;
        self.requests.send_replace(Some(relay));
        Ok(())
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Option<bool>> {
        let mut receiver = self.requests.subscribe();
        if receiver.borrow().is_some() {
            receiver.mark_changed();
        }
        receiver
    }

    pub(crate) fn submitted(&self, relay: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.view.relay_enabled = relay;
        state.view.pending = relay;
        state.waiting_since = relay.then(Instant::now);
        if !relay {
            state.view.notice = Some("已恢复自动选路");
            state.notice_until = Some(Instant::now() + Duration::from_secs(3));
        }
    }

    pub(crate) fn observe_route(&self, relay: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(start) = state.waiting_since else {
            return;
        };
        if relay {
            state.view.pending = false;
            state.waiting_since = None;
            state.view.notice = Some("已切换到中转");
            state.view.error = None;
            state.notice_until = Some(Instant::now() + Duration::from_secs(3));
        } else if state.view.pending && start.elapsed() >= Duration::from_secs(3) {
            state.view.pending = false;
            state.view.error = Some("尚未切换到中转，当前线路仍在使用".into());
        }
    }

    pub(crate) fn fail(&self, error: impl Into<String>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.view.pending = false;
        state.waiting_since = None;
        state.view.error = Some(error.into());
    }

    pub(crate) fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.connected = false;
        state.view.pending = false;
        state.waiting_since = None;
    }
}
