//! Development: viewers' downlinks as real ones behave, to test how the
//! forwarder serves a room (`FARWING_DEV_SFU_NET`). Entries are separated by
//! `;`, each `N[,N...]:RULE[,RULE...]` for the Nth viewers of a room
//! (counted from 1 as they subscribe). Rules:
//! - `rate=MBPS`: a bottleneck with a drop-tail queue of [`QUEUE`];
//! - `rate=A/B@S`: the bottleneck alternates A and B Mbps every S seconds
//!   (congestion that comes and goes);
//! - `loss=P`: each packet is lost with probability P;
//! - `burst=P/R`: Gilbert–Elliott loss: a good link turns bad with
//!   probability P per packet, a bad one recovers with R; every packet sent
//!   while bad is lost (losses in bursts, now and then);
//! - `delay=MS[/JITTER]`: one-way delay plus up to JITTER ms of variation;
//! - `outage=FROM-TO`: nothing gets through from FROM to TO seconds after
//!   the viewer subscribed.
//!
//! Packets are counted as sent when they enter the emulated network, so the
//! downlink estimate sees its queueing and losses; they arrive in order.

use std::time::{Duration, Instant};

/// Most a bottleneck holds.
const QUEUE: Duration = Duration::from_millis(200);
/// IP, UDP and SRTP overhead per packet.
const OVERHEAD: usize = 48;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Rules {
    rate: Option<(f64, f64, f64)>,
    loss: f64,
    burst: Option<(f64, f64)>,
    delay: Option<(f64, f64)>,
    outage: Option<(f64, f64)>,
}

pub(super) struct Emulator {
    rules: Rules,
    start: Instant,
    free_at: Instant,
    bad: bool,
    last_arrival: Instant,
}

impl Emulator {
    /// The emulator for viewer `index` (leg `leg`) of a room, if the
    /// variable names it.
    pub fn for_viewer(index: usize, leg: &str) -> Option<Self> {
        let spec = std::env::var("FARWING_DEV_SFU_NET").ok()?;
        let rules = parse(&spec, index)?;
        tracing::warn!(viewer = index, leg, ?rules, "development downlink emulation");
        let now = Instant::now();
        Some(Self {
            rules,
            start: now,
            free_at: now,
            bad: false,
            last_arrival: now,
        })
    }

    /// When a packet of `bytes` sent now arrives at the viewer; `None` when
    /// it is lost.
    pub fn admit(&mut self, bytes: usize) -> Option<Instant> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.start).as_secs_f64();
        if self
            .rules
            .outage
            .is_some_and(|(from, to)| (from..to).contains(&elapsed))
        {
            return None;
        }
        if let Some((enter, leave)) = self.rules.burst {
            self.bad = if self.bad {
                rand::random::<f64>() >= leave
            } else {
                rand::random::<f64>() < enter
            };
            if self.bad {
                return None;
            }
        }
        if self.rules.loss > 0.0 && rand::random::<f64>() < self.rules.loss {
            return None;
        }
        let mut arrival = now;
        if let Some((high, low, period)) = self.rules.rate {
            let rate = if period > 0.0 && (elapsed / period) as u64 % 2 == 1 {
                low
            } else {
                high
            };
            let start = self.free_at.max(now);
            if start.duration_since(now) > QUEUE {
                return None;
            }
            self.free_at =
                start + Duration::from_secs_f64((bytes + OVERHEAD) as f64 * 8.0 / (rate * 1e6));
            arrival = self.free_at;
        }
        if let Some((delay, jitter)) = self.rules.delay {
            let extra = delay + jitter * rand::random::<f64>();
            arrival += Duration::from_secs_f64(extra / 1000.0);
        }
        // Packets keep their order.
        arrival = arrival.max(self.last_arrival);
        self.last_arrival = arrival;
        Some(arrival)
    }
}

fn parse(spec: &str, index: usize) -> Option<Rules> {
    for entry in spec.split(';') {
        let Some((viewers, rules)) = entry.split_once(':') else { continue };
        if !viewers
            .split(',')
            .any(|n| n.trim().parse::<usize>().ok() == Some(index))
        {
            continue;
        }
        let mut out = Rules::default();
        for rule in rules.split(',') {
            let Some((name, value)) = rule.trim().split_once('=') else { continue };
            let pair = |value: &str, separator: char| -> Option<(f64, f64)> {
                let (a, b) = value.split_once(separator)?;
                Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
            };
            match name.trim() {
                "rate" => {
                    out.rate = match value.split_once('@') {
                        Some((rates, period)) => {
                            let (high, low) = pair(rates, '/')?;
                            Some((high, low, period.trim().parse().ok()?))
                        }
                        None => {
                            let rate = value.trim().parse().ok()?;
                            Some((rate, rate, 0.0))
                        }
                    };
                }
                "loss" => out.loss = value.trim().parse().ok()?,
                "burst" => out.burst = pair(value, '/'),
                "delay" => {
                    out.delay = pair(value, '/').or_else(|| Some((value.trim().parse().ok()?, 0.0)));
                }
                "outage" => out.outage = pair(value, '-'),
                _ => {}
            }
        }
        return Some(out);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_per_viewer() {
        let spec = "2,4:rate=10/1.5@8,burst=0.01/0.2;3:loss=0.05,delay=40/30;5:outage=20-40";
        let two = parse(spec, 2).unwrap();
        assert_eq!(two.rate, Some((10.0, 1.5, 8.0)));
        assert_eq!(two.burst, Some((0.01, 0.2)));
        assert_eq!(parse(spec, 4), Some(two));
        let three = parse(spec, 3).unwrap();
        assert_eq!((three.loss, three.delay), (0.05, Some((40.0, 30.0))));
        assert_eq!(parse(spec, 5).unwrap().outage, Some((20.0, 40.0)));
        assert_eq!(parse(spec, 1), None);
        assert_eq!(parse("1:rate=2", 1).unwrap().rate, Some((2.0, 2.0, 0.0)));
    }
}
