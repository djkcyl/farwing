//! Sender-initiated route switching: primary RR statistics, 15-sample minima.
use std::{
    collections::{BTreeMap, VecDeque},
    time::{Duration, Instant},
};
use webrtc::ice_transport::{
    ice_candidate_pair::RTCIceCandidatePair, ice_candidate_type::RTCIceCandidateType,
};
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Policy {
    enabled: bool,
    loss: u32,
    latency: u32,
}

impl Policy {
    /// Session network policy from `session.incoming`.
    pub fn from_network(network: &farwing_protocol::api::NetworkPolicy) -> Self {
        let integer = |value: u32| i32::try_from(value).unwrap_or(i32::MAX);
        let loss = integer(network.force_loss);
        let latency = integer(network.force_latency);
        let possible_loss = integer(network.possible_loss);
        let possible_latency = integer(network.possible_latency);
        let minimum_latency = integer(network.possible_min_latency);
        // The whole group is validated before the force thresholds are used.
        // Even valid force values fall back when the other relationships are
        // invalid or missing.
        let valid = loss >= 10
            && latency >= 200
            && minimum_latency >= 200
            && possible_loss >= 5
            && possible_loss <= loss
            && possible_latency >= minimum_latency
            && possible_latency <= latency;
        let policy = Self {
            enabled: network.auto_switch,
            loss: if valid { loss as u32 } else { 30 },
            latency: if valid { latency as u32 } else { 600 },
        };
        tracing::info!(
            ?policy,
            defaulted = !valid,
            "host effective automatic route policy"
        );
        policy
    }
}
struct Stream {
    automatic: bool,
    quality: i32,
    latest: Option<(u8, Duration)>,
    window: VecDeque<(u8, Duration)>,
}
impl Stream {
    fn clear_samples(&mut self) {
        self.latest = None;
        self.window.clear();
    }
}

pub(super) struct AutoSwitch {
    policy: Policy,
    streams: BTreeMap<usize, Stream>,
    at: Option<Instant>,
    route: Option<(bool, bool)>,
    attempt: u8,
}
impl AutoSwitch {
    pub fn attempt(&self) -> u8 {
        self.attempt
    }
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            streams: BTreeMap::new(),
            at: None,
            route: None,
            attempt: 0,
        }
    }
    pub fn quality(&mut self, index: usize, automatic: bool, quality: i32, settings_changed: bool) {
        let stream = self.streams.entry(index).or_insert_with(|| Stream {
            automatic,
            quality,
            latest: None,
            window: VecDeque::with_capacity(15),
        });
        if settings_changed || stream.automatic != automatic || stream.quality != quality {
            // Only this stream's settings changed. A healthy second screen
            // must not erase another screen's sustained loss/RTT evidence.
            stream.clear_samples();
        }
        stream.automatic = automatic;
        stream.quality = quality;
    }
    pub fn remove(&mut self, index: usize) {
        self.streams.remove(&index);
    }
    pub fn reset_samples(&mut self) {
        self.at = None;
        for stream in self.streams.values_mut() {
            stream.clear_samples();
        }
    }
    pub fn route(&mut self, pair: &RTCIceCandidatePair) {
        self.route = Some((
            pair.local.typ == RTCIceCandidateType::Relay
                || pair.remote.typ == RTCIceCandidateType::Relay,
            pair.local.relay_protocol.eq_ignore_ascii_case("tls")
                && pair.remote.relay_protocol.eq_ignore_ascii_case("tls"),
        ));
        self.reset_samples();
    }
    pub fn report(&mut self, index: usize, loss: u8, rtt: Duration) {
        // Late reports from paused/removed streams do not recreate observers.
        if let Some(stream) = self.streams.get_mut(&index) {
            stream.latest = Some((loss, rtt));
        }
    }
    pub fn tick(&mut self, connected: bool, now: Instant) -> Option<u8> {
        if !connected {
            self.reset_samples();
            return None;
        }
        if !self.policy.enabled || self.policy.loss < 10 || self.policy.latency < 200 {
            return None;
        }
        let (relay, tls) = self.route?;
        if self
            .at
            .is_some_and(|at| now.saturating_duration_since(at) < Duration::from_secs(1))
        {
            return None;
        }
        self.at = Some(now);
        let mut reason = None;
        for (&index, stream) in &mut self.streams {
            // Retain per-screen automatic quality. A stream at its lowest
            // usable tier may request rerouting without lowering healthy peers.
            if stream.automatic && stream.quality > 2 {
                continue;
            }
            let Some(latest) = stream.latest else {
                continue;
            };
            if stream.window.len() == 15 {
                stream.window.pop_front();
            }
            stream.window.push_back(latest);
            if stream.window.len() < 15 {
                continue;
            }
            let loss = stream
                .window
                .iter()
                .map(|v| u32::from(v.0) * 100 / 256)
                .min()
                .unwrap_or(0);
            let latency = stream.window.iter().map(|v| v.1).min().unwrap_or_default();
            if loss > self.policy.loss
                || latency > Duration::from_millis(self.policy.latency.into())
            {
                reason.get_or_insert((index, loss, latency));
            }
        }
        let (index, loss, latency) = reason?;
        if tls {
            return None;
        }
        self.attempt = if self.attempt == 2 || relay { 2 } else { 1 };
        tracing::info!(
            stream = index,
            loss_percent = loss,
            latency_ms = latency.as_millis(),
            attempt = self.attempt,
            "host automatic route switch requested"
        );
        self.reset_samples();
        Some(self.attempt)
    }
}
