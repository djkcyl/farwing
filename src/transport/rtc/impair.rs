//! Deterministic video loss and delay at the controller's ingress, for
//! measuring loss recovery (`docs/protocol/rfi-ltr-brief.md` §6.10).
//!
//! Configured by `dev video --loss` or, in debug builds,
//! `FARWING_DEV_VIDEO_LOSS`. Rules, separated by `;`:
//! - `frame:N[,every=M]` drops every packet of the Nth frame (counted by RTP
//!   timestamp), then of every Mth frame after it;
//! - `burst:START_MS+DURATION_MS` drops every video packet arriving in that
//!   window (counted from the first packet);
//! - `ge:P,R[,seed=S]` is a Gilbert–Elliott channel: P = good→bad, R =
//!   bad→good per packet, every packet in the bad state is lost;
//! - `delay:MS` holds every video packet that long (one-way delay; the
//!   receiver's RTT estimate grows by the same amount);
//! - `net:P,R[,seed=S]` is a Gilbert–Elliott channel in front of the
//!   receiver, applied to every video packet (media, RTX, FEC) before repair,
//!   so NACK and FEC can repair what it drops, as on a real network;
//! - `rate:MBPS[,queue=MS]` passes every video packet (media, RTX, FEC)
//!   through a bottleneck of that rate with a drop-tail queue holding at most
//!   MS of data (default 200 ms), so large frames take their real
//!   transmission time.
//!
//! Losses are applied after RTX, RED and FEC recovery, by original sequence
//! number and RTP timestamp, so a dropped packet is never repaired. Queue
//! overflow at the bottleneck is ordinary loss: NACK and FEC can repair it.

use anyhow::{Context, Result, bail};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
enum Rule {
    Frame { first: u64, every: Option<u64> },
    Burst { start: Duration, duration: Duration },
    Gilbert { enter: f64, leave: f64, seed: u64 },
}

/// A bottleneck link in front of the receiver.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Link {
    bits_per_second: f64,
    queue: Duration,
}

/// Random loss in front of the receiver (repairable).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Network {
    enter: f64,
    leave: f64,
    seed: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Config {
    rules: Vec<Rule>,
    delay: Duration,
    link: Option<Link>,
    network: Option<Network>,
    mtu: Option<usize>,
}

impl Config {
    pub(crate) fn parse(spec: &str) -> Result<Self> {
        let mut config = Self::default();
        for part in spec.split(';').map(str::trim).filter(|p| !p.is_empty()) {
            let (kind, value) = part.split_once(':').context("规则要写成 种类:参数")?;
            let number = |text: &str| -> Result<u64> {
                text.trim().parse().with_context(|| format!("不是整数：{text}"))
            };
            let probability = |text: &str| -> Result<f64> {
                let value: f64 = text.trim().parse().with_context(|| format!("不是小数：{text}"))?;
                anyhow::ensure!((0.0..=1.0).contains(&value), "概率要在 0 到 1 之间：{text}");
                Ok(value)
            };
            match kind.trim() {
                "frame" => {
                    let mut fields = value.split(',');
                    let first = number(fields.next().unwrap_or_default())?;
                    anyhow::ensure!(first > 0, "帧号从 1 开始");
                    let mut every = None;
                    for field in fields {
                        match field.trim().split_once('=') {
                            Some(("every", m)) => every = Some(number(m)?).filter(|m| *m > 0),
                            _ => bail!("frame 只接受 every=M：{field}"),
                        }
                    }
                    config.rules.push(Rule::Frame { first, every });
                }
                "burst" => {
                    let (start, duration) = value.split_once('+').context("burst 要写成 起点毫秒+时长毫秒")?;
                    config.rules.push(Rule::Burst {
                        start: Duration::from_millis(number(start)?),
                        duration: Duration::from_millis(number(duration)?),
                    });
                }
                "ge" | "net" => {
                    let mut fields = value.split(',');
                    let enter = probability(fields.next().unwrap_or_default())?;
                    let leave = probability(fields.next().context("ge / net 要写成 P,R[,seed=S]")?)?;
                    let mut seed = 1;
                    for field in fields {
                        match field.trim().split_once('=') {
                            Some(("seed", s)) => seed = number(s)?,
                            _ => bail!("ge / net 只接受 seed=S：{field}"),
                        }
                    }
                    if kind.trim() == "net" {
                        config.network = Some(Network { enter, leave, seed });
                    } else {
                        config.rules.push(Rule::Gilbert { enter, leave, seed });
                    }
                }
                "delay" => config.delay = Duration::from_millis(number(value)?),
                "mtu" => config.mtu = Some(number(value)? as usize),
                "rate" => {
                    let mut fields = value.split(',');
                    let text = fields.next().unwrap_or_default().trim();
                    let mbps: f64 = text.parse().with_context(|| format!("不是数字：{text}"))?;
                    anyhow::ensure!(mbps > 0.0 && mbps.is_finite(), "带宽要大于 0：{text}");
                    let mut queue = Duration::from_millis(200);
                    for field in fields {
                        match field.trim().split_once('=') {
                            Some(("queue", ms)) => queue = Duration::from_millis(number(ms)?),
                            _ => bail!("rate 只接受 queue=MS：{field}"),
                        }
                    }
                    config.link = Some(Link {
                        bits_per_second: mbps * 1e6,
                        queue,
                    });
                }
                other => bail!("未知规则：{other}"),
            }
        }
        Ok(config)
    }

    /// `mtu:N`: datagrams longer than N bytes on the wire (IP, UDP and SRTP
    /// included) are lost, as on a path with a smaller MTU.
    pub(crate) fn mtu(&self) -> Option<usize> {
        self.mtu
    }

    pub(crate) fn delay(&self) -> Duration {
        self.delay
    }

    pub(crate) fn link(&self) -> Option<Link> {
        self.link
    }

    pub(crate) fn network(&self) -> Option<Network> {
        self.network
    }
}

/// SplitMix64: a fixed sequence of uniform draws in [0, 1) for a seed.
fn uniform(state: &mut u64) -> f64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 11) as f64 / (1u64 << 53) as f64
}

/// The state of a `Network` channel, fed in arrival order.
pub(crate) struct NetworkLoss {
    network: Network,
    bad: bool,
    random: u64,
}

impl NetworkLoss {
    pub(crate) fn new(network: Network) -> Self {
        Self {
            network,
            bad: false,
            random: network.seed,
        }
    }

    /// Whether the next packet is lost.
    pub(crate) fn drops(&mut self) -> bool {
        let draw = uniform(&mut self.random);
        self.bad = if self.bad {
            draw >= self.network.leave
        } else {
            draw < self.network.enter
        };
        if self.bad {
            NETWORK_DROPS.fetch_add(1, Ordering::Relaxed);
        }
        self.bad
    }
}

static NETWORK_DROPS: AtomicU64 = AtomicU64::new(0);

/// Packets the `net:` channel dropped before repair.
pub(crate) fn network_drops() -> u64 {
    NETWORK_DROPS.load(Ordering::Relaxed)
}

/// UDP/IP and SRTP overhead added to each decrypted RTP packet.
const PACKET_OVERHEAD: usize = 28 + 10;

/// The drop-tail queue of a `Link`, fed in arrival order.
pub(crate) struct Bottleneck {
    link: Link,
    free_at: Option<Instant>,
}

impl Bottleneck {
    pub(crate) fn new(link: Link) -> Self {
        Self { link, free_at: None }
    }

    /// When a packet of `bytes` arriving at `arrival` has left the link, or
    /// `None` when the queue is full and it is lost.
    pub(crate) fn admit(&mut self, arrival: Instant, bytes: usize) -> Option<Instant> {
        let start = self.free_at.map_or(arrival, |free| free.max(arrival));
        let waiting = start.saturating_duration_since(arrival);
        if waiting > self.link.queue {
            QUEUE_DROPS.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        QUEUE_MAX_US.fetch_max(waiting.as_micros() as u64, Ordering::Relaxed);
        let transmit = Duration::from_secs_f64((bytes + PACKET_OVERHEAD) as f64 * 8.0 / self.link.bits_per_second);
        let departure = start + transmit;
        self.free_at = Some(departure);
        Some(departure)
    }
}

static QUEUE_DROPS: AtomicU64 = AtomicU64::new(0);
static QUEUE_MAX_US: AtomicU64 = AtomicU64::new(0);

/// Packets the bottleneck queue dropped, and the longest queueing delay.
pub(crate) fn queue_stats() -> (u64, Duration) {
    (
        QUEUE_DROPS.load(Ordering::Relaxed),
        Duration::from_micros(QUEUE_MAX_US.load(Ordering::Relaxed)),
    )
}

static CONFIG: OnceLock<Option<Config>> = OnceLock::new();

/// Set by `dev video` before it connects. Fails once the receive path has
/// already read its configuration.
pub(crate) fn configure(config: Config) -> Result<()> {
    CONFIG.set(Some(config)).map_err(|_| anyhow::anyhow!("丢包注入已经生效，不能再改"))
}

pub(crate) fn config() -> Option<&'static Config> {
    CONFIG
        .get_or_init(|| {
            if !cfg!(debug_assertions) {
                return None;
            }
            let spec = std::env::var("FARWING_DEV_VIDEO_LOSS").ok()?;
            match Config::parse(&spec) {
                Ok(config) => Some(config),
                Err(error) => {
                    tracing::warn!(%error, %spec, "ignoring FARWING_DEV_VIDEO_LOSS");
                    None
                }
            }
        })
        .as_ref()
}

/// Extra round-trip time the receiver adds to its estimate for `delay`.
pub(crate) fn extra_rtt() -> Duration {
    config().map_or(Duration::ZERO, Config::delay)
}

/// One injected loss: the first dropped packet of a run of drops.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LossEvent {
    pub at: Instant,
    pub rtp_timestamp: u32,
    pub packets: u32,
}

static EVENTS: Mutex<Vec<LossEvent>> = Mutex::new(Vec::new());
static KEYFRAME_REQUESTS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn events() -> Vec<LossEvent> {
    EVENTS.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Every PLI this process sent.
pub(crate) fn keyframe_requests() -> u64 {
    KEYFRAME_REQUESTS.load(Ordering::Relaxed)
}

pub(super) fn record_keyframe_request() {
    KEYFRAME_REQUESTS.fetch_add(1, Ordering::Relaxed);
}

const REMEMBERED: usize = 8192;

/// Loss state of one video receive stream.
pub(super) struct Injector {
    rules: Vec<Rule>,
    started: Option<Instant>,
    frames: u64,
    last_timestamp: Option<u32>,
    bad: bool,
    random: u64,
    dropped_timestamps: VecDeque<u32>,
    dropped_sequences: HashSet<u16>,
    dropped_order: VecDeque<u16>,
    dropping: bool,
    record: bool,
}

impl Injector {
    pub(super) fn new() -> Option<Self> {
        let config = config().filter(|c| !c.rules.is_empty())?;
        Some(Self::with(config.rules.clone(), true))
    }

    fn with(rules: Vec<Rule>, record: bool) -> Self {
        let seed = rules
            .iter()
            .find_map(|rule| match rule {
                Rule::Gilbert { seed, .. } => Some(*seed),
                _ => None,
            })
            .unwrap_or(1);
        Self {
            rules,
            started: None,
            frames: 0,
            last_timestamp: None,
            bad: false,
            random: seed,
            dropped_timestamps: VecDeque::new(),
            dropped_sequences: HashSet::new(),
            dropped_order: VecDeque::new(),
            dropping: false,
            record,
        }
    }

    fn uniform(&mut self) -> f64 {
        uniform(&mut self.random)
    }

    /// Whether this media packet is lost. `recovered`: it came from RTX or
    /// FEC, so it only follows earlier decisions.
    pub(super) fn drops(&mut self, sequence: u16, timestamp: u32, recovered: bool, now: Instant) -> bool {
        if recovered {
            return self.dropped_sequences.contains(&sequence) || self.dropped_timestamps.contains(&timestamp);
        }
        let started = *self.started.get_or_insert(now);
        // A newer RTP timestamp starts the next frame.
        if self
            .last_timestamp
            .is_none_or(|last| (timestamp.wrapping_sub(last) as i32) > 0)
        {
            self.last_timestamp = Some(timestamp);
            self.frames += 1;
            let frames = self.frames;
            let lost = self.rules.iter().any(|rule| match *rule {
                Rule::Frame { first, every } => {
                    frames == first || every.is_some_and(|m| frames > first && (frames - first) % m == 0)
                }
                _ => false,
            });
            if lost {
                self.dropped_timestamps.push_back(timestamp);
                if self.dropped_timestamps.len() > 256 {
                    self.dropped_timestamps.pop_front();
                }
            }
        }
        let elapsed = now.saturating_duration_since(started);
        let mut drop = self.dropped_timestamps.contains(&timestamp);
        for index in 0..self.rules.len() {
            match self.rules[index] {
                Rule::Burst { start, duration } => drop |= elapsed >= start && elapsed < start + duration,
                Rule::Gilbert { enter, leave, .. } => {
                    let draw = self.uniform();
                    self.bad = if self.bad { draw >= leave } else { draw < enter };
                    drop |= self.bad;
                }
                Rule::Frame { .. } => {}
            }
        }
        if drop {
            if self.dropped_sequences.insert(sequence) {
                self.dropped_order.push_back(sequence);
                if self.dropped_order.len() > REMEMBERED
                    && let Some(old) = self.dropped_order.pop_front()
                {
                    self.dropped_sequences.remove(&old);
                }
            }
            if self.record {
                let mut events = EVENTS.lock().unwrap_or_else(|p| p.into_inner());
                match events.last_mut() {
                    Some(event) if self.dropping => event.packets += 1,
                    _ => events.push(LossEvent {
                        at: now,
                        rtp_timestamp: timestamp,
                        packets: 1,
                    }),
                }
            }
        }
        self.dropping = drop;
        drop
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rules() {
        let config = Config::parse("frame:300,every=120; burst:5000+200;ge:0.01,0.3,seed=7;delay:40").unwrap();
        assert_eq!(
            config.rules,
            vec![
                Rule::Frame {
                    first: 300,
                    every: Some(120)
                },
                Rule::Burst {
                    start: Duration::from_secs(5),
                    duration: Duration::from_millis(200)
                },
                Rule::Gilbert {
                    enter: 0.01,
                    leave: 0.3,
                    seed: 7
                },
            ]
        );
        assert_eq!(config.delay(), Duration::from_millis(40));
        assert_eq!(config.link(), None);
        assert_eq!(
            Config::parse("rate:8.5,queue=50").unwrap().link(),
            Some(Link {
                bits_per_second: 8.5e6,
                queue: Duration::from_millis(50)
            })
        );
        assert_eq!(
            Config::parse("rate:2").unwrap().link().map(|l| l.queue),
            Some(Duration::from_millis(200))
        );
        assert!(Config::parse("rate:0").is_err());
        let network = Config::parse("net:0.01,0.5,seed=3").unwrap();
        assert!(network.rules.is_empty());
        assert_eq!(
            network.network(),
            Some(Network {
                enter: 0.01,
                leave: 0.5,
                seed: 3
            })
        );
        assert!(Config::parse("frame:0").is_err());
        assert!(Config::parse("ge:2,0.1").is_err());
        assert!(Config::parse("loss:1").is_err());
    }

    #[test]
    fn frame_rule_drops_whole_frames_and_their_repairs() {
        let mut injector = Injector::with(
            vec![Rule::Frame {
                first: 2,
                every: Some(3),
            }],
            false,
        );
        let now = Instant::now();
        let mut sequence = 0u16;
        let mut dropped = Vec::new();
        for frame in 1..=8u32 {
            for _ in 0..3 {
                if injector.drops(sequence, frame * 3000, false, now) {
                    dropped.push(sequence);
                }
                sequence += 1;
            }
        }
        // Frames 2, 5 and 8.
        assert_eq!(dropped, vec![3, 4, 5, 12, 13, 14, 21, 22, 23]);
        // RTX or FEC copies of lost packets stay lost; others pass.
        assert!(injector.drops(4, 6000, true, now));
        assert!(!injector.drops(6, 9000, true, now));
    }

    #[test]
    fn bottleneck_spreads_bursts_and_drops_beyond_its_queue() {
        // 1 Mbps, 20 ms queue: a packet of 1250 bytes on the wire takes 10 ms.
        let mut link = Bottleneck::new(Link {
            bits_per_second: 1e6,
            queue: Duration::from_millis(20),
        });
        let now = Instant::now();
        let departures: Vec<_> = (0..5)
            .map(|_| link.admit(now, 1250 - PACKET_OVERHEAD).map(|d| d.duration_since(now).as_millis()))
            .collect();
        // Waits of 0, 10 and 20 ms fit; the fourth would wait 30 ms.
        assert_eq!(departures, vec![Some(10), Some(20), Some(30), None, None]);
        // Once drained, a packet passes straight through.
        let later = now + Duration::from_millis(100);
        assert_eq!(link.admit(later, 1250 - PACKET_OVERHEAD), Some(later + Duration::from_millis(10)));
    }

    #[test]
    fn burst_and_gilbert_are_deterministic() {
        let started = Instant::now();
        let mut burst = Injector::with(
            vec![Rule::Burst {
                start: Duration::from_millis(100),
                duration: Duration::from_millis(50),
            }],
            false,
        );
        let lost: Vec<_> = (0..20u16)
            .filter(|i| burst.drops(*i, u32::from(*i), false, started + Duration::from_millis(u64::from(*i) * 10)))
            .collect();
        assert_eq!(lost, vec![10, 11, 12, 13, 14]);
        let run = || {
            let mut channel = Injector::with(
                vec![Rule::Gilbert {
                    enter: 0.05,
                    leave: 0.5,
                    seed: 42,
                }],
                false,
            );
            (0..2000u16)
                .filter(|i| channel.drops(*i, u32::from(*i), false, started))
                .count()
        };
        let lost = run();
        assert_eq!(lost, run());
        // Stationary loss P / (P + R) ≈ 9 %.
        assert!((100..260).contains(&lost), "{lost}");
    }
}
