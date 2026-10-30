//! Viewer commands for the existing signaling/ICE owner, not another connection loop.
use crate::i18n::t;
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

/// Why a session may not use the server's relays, as the user reads it.
pub(crate) fn relay_denied_text(reason: &str) -> &'static str {
    use farwing_protocol::realtime::relay_denied;
    let errors = &t().errors;
    match reason {
        relay_denied::QUOTA => errors.relay_quota,
        relay_denied::BUSY => errors.relay_busy,
        _ => errors.relay_not_allowed,
    }
}

/// The reason the session may not use relays (none: it may).
pub(crate) fn relay_denied(
    access: &farwing_protocol::realtime::RelayAccess,
) -> Option<&'static str> {
    (!access.allowed).then(|| relay_denied_text(access.reason.as_deref().unwrap_or_default()))
}

#[derive(Default)]
struct State {
    view: NetworkControlSnapshot,
    connected: bool,
    round_trip: Option<(Duration, Instant)>,
    forced: bool,
    has_relay: bool,
    /// Why the session may not use the server's relays.
    denied: Option<&'static str>,
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

    pub(crate) fn configure(
        &self,
        relay: bool,
        forced: bool,
        has_relay: bool,
        denied: Option<&'static str>,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.view.relay_enabled = relay;
        state.forced = forced;
        state.has_relay = has_relay;
        state.denied = denied;
    }

    /// The session's relays changed; without any, it no longer relays.
    pub(crate) fn relays_changed(&self, has_relay: bool, denied: Option<&'static str>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.has_relay = has_relay;
        state.denied = denied;
        if !has_relay && !state.forced {
            state.view.relay_enabled = false;
        }
    }

    pub(crate) fn forced(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).forced
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
            Some(t().features.network_media_not_connected)
        } else if state.forced {
            Some(t().features.network_relay_required)
        } else if !state.has_relay && !state.view.relay_enabled {
            Some(state.denied.unwrap_or(t().features.network_no_relay))
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
            bail!("{}", t().features.network_switching);
        }
        if !state.connected {
            bail!("{}", t().features.network_media_not_connected);
        }
        if state.forced && !relay {
            bail!("{}", t().features.network_relay_required);
        }
        if relay && !state.has_relay {
            bail!("{}", state.denied.unwrap_or(t().features.network_no_relay));
        }
        if state
            .last_request
            .is_some_and(|at| at.elapsed() < Duration::from_secs(5))
        {
            bail!("{}", t().features.network_switching_too_often);
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
            state.view.notice = Some(t().features.network_auto_route_restored);
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
            state.view.notice = Some(t().features.network_switched_to_relay);
            state.view.error = None;
            state.notice_until = Some(Instant::now() + Duration::from_secs(3));
        } else if state.view.pending && start.elapsed() >= Duration::from_secs(3) {
            state.view.pending = false;
            state.view.error = Some(t().features.network_relay_switch_failed.into());
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
