//! PacerV2 burst feedback.
//! Groups are actual pacer bursts, not video frames or feedback datagrams.
use std::collections::{BTreeMap, VecDeque};

struct Packet {
    status: Option<bool>,
    lost_at: i64,
}
struct Group {
    bytes: usize,
    last_send: i64,
    sealed: bool,
    packets: BTreeMap<i64, Packet>,
}
struct Sample {
    at: i64,
    bytes: usize,
    loss: f64,
}
pub(super) struct Burst {
    pending: BTreeMap<u64, Group>,
    packets: usize,
    current: u64,
    discarded: Option<u64>,
    samples: VecDeque<Sample>,
    evaluated: i64,
    changed: Option<i64>,
    increase_since: Option<i64>,
    pub bytes: usize,
}
impl Default for Burst {
    fn default() -> Self {
        Self {
            pending: BTreeMap::new(),
            packets: 0,
            current: 0,
            discarded: None,
            samples: VecDeque::new(),
            evaluated: 0,
            changed: None,
            increase_since: None,
            bytes: 63000,
        }
    }
}
impl Burst {
    pub fn sent(&mut self, group: u64, sequence: i64, bytes: usize, now: i64) {
        if self.discarded == Some(group) {
            return;
        }
        if group != self.current {
            if let Some(previous) = self.pending.get_mut(&self.current) {
                previous.sealed = true;
            }
            self.current = group;
        }
        // Incomplete/overflowed groups cannot become a no-loss observation.
        if self.packets >= 4096 {
            if let Some((id, discarded)) = self.pending.pop_first() {
                self.packets -= discarded.packets.len();
                self.discarded = Some(id);
                if id == group {
                    return;
                }
            }
        }
        let entry = self.pending.entry(group).or_insert_with(|| Group {
            bytes: 0,
            last_send: now,
            sealed: false,
            packets: BTreeMap::new(),
        });
        entry.bytes += bytes;
        entry.last_send = now;
        if entry
            .packets
            .insert(
                sequence,
                Packet {
                    status: None,
                    lost_at: 0,
                },
            )
            .is_none()
        {
            self.packets += 1;
        }
    }
    pub fn feedback(&mut self, group: u64, sequence: i64, received: bool, now: i64) {
        if let Some(packet) = self
            .pending
            .get_mut(&group)
            .and_then(|g| g.packets.get_mut(&sequence))
        {
            if packet.status != Some(true) {
                if !received && packet.status.is_none() {
                    packet.lost_at = now;
                }
                packet.status = Some(received);
            }
        }
    }
    pub fn tick(&mut self, now: i64, rtt_us: i64) {
        let timeout = (rtt_us.max(0) + 250_000).min(1_000_000);
        let ready: Vec<_> = self
            .pending
            .iter()
            .filter_map(|(&id, group)| {
                if !group.sealed {
                    return None;
                }
                let pending_loss = group
                    .packets
                    .values()
                    .any(|p| p.status == Some(false) && now - p.lost_at < 50_000);
                let missing = group.packets.values().any(|p| p.status.is_none());
                (!pending_loss && (!missing || now - group.last_send >= timeout)).then_some(id)
            })
            .collect();
        for id in ready {
            let group = self.pending.remove(&id).unwrap();
            self.packets -= group.packets.len();
            if group.bytes >= 12000 && group.packets.values().all(|p| p.status.is_some()) {
                let lost = group
                    .packets
                    .values()
                    .filter(|p| p.status == Some(false))
                    .count();
                self.samples.push_back(Sample {
                    at: now,
                    bytes: group.bytes,
                    loss: lost as f64 / group.packets.len() as f64,
                });
            }
        }
        if now - self.evaluated < 500_000 {
            return;
        }
        self.evaluated = now;
        while self.samples.front().is_some_and(|s| now - s.at > 1_000_000) {
            self.samples.pop_front();
        }
        let changed = *self.changed.get_or_insert(now);
        let n = self.samples.len() as f64;
        if n < 8.0 {
            self.increase_since = None;
            return;
        }
        let x = self.samples.iter().map(|s| s.bytes as f64).sum::<f64>() / n;
        let y = self.samples.iter().map(|s| s.loss).sum::<f64>() / n;
        let (mut xx, mut yy, mut xy) = (0.0, 0.0, 0.0);
        for sample in &self.samples {
            let dx = sample.bytes as f64 - x;
            let dy = sample.loss - y;
            xx += dx * dx;
            yy += dy * dy;
            xy += dx * dy;
        }
        // Identical burst sizes do not establish the correlation needed to
        // classify the window. A constant zero loss is a valid normal window.
        if xx == 0.0 {
            self.increase_since = None;
            return;
        }
        let overuse = y >= 0.03 && yy > 0.0 && xy / (xx * yy).sqrt() >= 0.5;
        let next = if overuse {
            self.increase_since = None;
            (self.bytes as f64 * 0.85).round().max(24000.0) as usize
        } else {
            let hits = self
                .samples
                .iter()
                .filter(|s| s.bytes >= (self.bytes as f64 * 0.9).round() as usize)
                .count();
            if hits as f64 / n < 0.3 {
                return;
            }
            if now - changed < 1_000_000 {
                self.increase_since = None;
                return;
            }
            let since = *self.increase_since.get_or_insert(now);
            if now - changed < 4_000_000 || now - since < 3_000_000 {
                return;
            }
            (self.bytes
                + if 128000 - self.bytes <= 32000 {
                    8000
                } else {
                    16000
                })
            .min(128000)
        };
        if next != self.bytes {
            self.bytes = next;
            self.changed = Some(now);
            self.increase_since = None;
        }
    }
}
