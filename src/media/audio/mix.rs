//! Several Opus streams played as one (`docs/protocol/camera-design.md`
//! §9.5): a NetEq decoder per stream, summed into 10-ms stereo blocks and
//! kept in range. The host's virtual microphone mixes its participants'
//! voices with it; a viewer mixes extra streams (a host's microphone, the
//! voices of a call) into its playback.
use super::neteq;
use anyhow::Result;
use bytes::Bytes;
use crossbeam_queue::ArrayQueue;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

/// One 10-ms stereo block at 48 kHz.
pub(crate) const BLOCK_SAMPLES: usize = super::BLOCK * 2;

/// Packets remembered to drop duplicates.
const SEEN: usize = 128;

struct Packet {
    data: Bytes,
    timestamp: u32,
    sequence: u16,
    generation: u64,
    arrived: Instant,
}

/// One stream's packets on their way from the network to the mixing thread.
pub(crate) struct Voice {
    packets: ArrayQueue<Packet>,
    generation: AtomicU64,
    ended: AtomicBool,
}

impl Voice {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            packets: ArrayQueue::new(256),
            generation: AtomicU64::new(1),
            ended: AtomicBool::new(false),
        })
    }
    /// A packet of the stream (Opus at 48 kHz, 10–120 ms).
    pub(crate) fn receive(&self, data: Bytes, timestamp: u32, sequence: u16) {
        if self.ended.load(Ordering::Acquire) {
            return;
        }
        let Ok(samples) = opusic_c::utils::get_nb_samples(&data, opusic_c::SampleRate::Hz48000) else {
            return;
        };
        if data.len() > super::MAX_PACKET || !(super::BLOCK..=super::MAX_SAMPLES).contains(&samples) {
            return;
        }
        self.packets.force_push(Packet {
            data,
            timestamp,
            sequence,
            generation: self.generation.load(Ordering::Acquire),
            arrived: Instant::now(),
        });
    }
    /// The stream ended; its decoder leaves the mix.
    pub(crate) fn end(&self) {
        self.ended.store(true, Ordering::Release);
    }
    pub(crate) fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }
}

/// A stream's decoder in a mix.
pub(crate) struct Decoder {
    neteq: neteq::Receiver,
    generation: u64,
    seen: VecDeque<(u16, u32)>,
    started: bool,
    stats: neteq::Statistics,
}

impl Decoder {
    pub(crate) fn new(generation: u64) -> Result<Self> {
        Ok(Self {
            neteq: neteq::Receiver::new()?,
            generation,
            seen: VecDeque::with_capacity(SEEN),
            started: false,
            stats: Default::default(),
        })
    }
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
    /// One packet; duplicates are dropped.
    pub(crate) fn insert(&mut self, data: &[u8], timestamp: u32, sequence: u16, arrived: Instant) {
        if self.seen.contains(&(sequence, timestamp)) {
            return;
        }
        if self.seen.len() == SEEN {
            self.seen.pop_front();
        }
        self.seen.push_back((sequence, timestamp));
        self.started |= self.neteq.insert(data, timestamp, sequence, arrived);
    }
    /// Add the next 10 ms to `mix` (nothing before the first packet).
    pub(crate) fn add_to(&mut self, mix: &mut [f32; BLOCK_SAMPLES], scratch: &mut [f32; BLOCK_SAMPLES]) -> Result<()> {
        if !self.started {
            return Ok(());
        }
        scratch.fill(0.0);
        self.neteq.block(scratch, &mut self.stats)?;
        for (out, sample) in mix.iter_mut().zip(scratch.iter()) {
            *out += *sample;
        }
        Ok(())
    }
}

/// The streams a playback mixes in besides its own.
pub(crate) struct Voices {
    voices: Vec<(Arc<Voice>, Option<Decoder>)>,
    scratch: Box<[f32; BLOCK_SAMPLES]>,
}

impl Default for Voices {
    fn default() -> Self {
        Self {
            voices: Vec::new(),
            scratch: Box::new([0.0; BLOCK_SAMPLES]),
        }
    }
}

impl Voices {
    pub(crate) fn add(&mut self, voice: Arc<Voice>) {
        self.voices.push((voice, None));
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.voices.is_empty()
    }
    /// Add each stream's next 10 ms to `mix`; ended streams leave.
    pub(crate) fn mix_into(&mut self, mix: &mut [f32; BLOCK_SAMPLES]) -> Result<()> {
        self.voices.retain(|(voice, _)| !voice.ended());
        for (voice, decoder) in &mut self.voices {
            let generation = voice.generation.load(Ordering::Acquire);
            if decoder.as_ref().is_none_or(|d| d.generation() != generation) {
                *decoder = Some(Decoder::new(generation)?);
            }
            let decoder = decoder.as_mut().expect("created above");
            for _ in 0..32 {
                let Some(packet) = voice.packets.pop() else {
                    break;
                };
                if packet.generation == generation {
                    decoder.insert(&packet.data, packet.timestamp, packet.sequence, packet.arrived);
                }
            }
            decoder.add_to(mix, &mut self.scratch)?;
        }
        Ok(())
    }
}

/// Keep the sum of several streams within range without hard clipping
/// quiet ones.
pub(crate) fn limit(sample: f32) -> f32 {
    const KNEE: f32 = 0.8;
    let magnitude = sample.abs();
    if magnitude <= KNEE {
        sample
    } else {
        let over = magnitude - KNEE;
        sample.signum() * (KNEE + (1.0 - KNEE) * (over / (1.0 - KNEE + over)))
    }
}

#[cfg(test)]
mod tests {
    use super::limit;

    #[test]
    fn the_limiter_keeps_quiet_samples_and_bounds_loud_ones() {
        assert_eq!(limit(0.5), 0.5);
        assert_eq!(limit(-0.8), -0.8);
        for loud in [1.0f32, 2.0, 10.0, 1000.0] {
            let limited = limit(loud);
            assert!(limited > 0.8 && limited < 1.0, "{loud} → {limited}");
            assert_eq!(limit(-loud), -limited);
        }
        // Louder in, louder out.
        assert!(limit(2.0) > limit(1.0));
    }
}
