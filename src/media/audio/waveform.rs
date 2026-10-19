//! Optional display tap. The audio callback never waits for a viewer or keeps PCM.
use crossbeam_queue::ArrayQueue;
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};

pub(crate) const SECONDS: f32 = 8.0;
const CAPACITY: usize = 1024;

#[derive(Clone, Copy)]
pub(crate) struct Block {
    pub min: [f32; 2],
    pub max: [f32; 2],
    pub sequence: u64,
    pub at: Instant,
    epoch: u64,
}

pub(super) struct Waveform {
    readers: AtomicUsize,
    epoch: AtomicU64,
    blocks: ArrayQueue<Block>,
}

impl Waveform {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            readers: AtomicUsize::new(0),
            epoch: AtomicU64::new(0),
            blocks: ArrayQueue::new(CAPACITY),
        })
    }

    pub fn clear(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
    }
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Exactly one 10 ms, 48 kHz stereo block, after NetEq and before output gain.
    pub fn record(&self, pcm: &[f32; super::BLOCK * 2], sequence: u64, epoch: u64) {
        if self.readers.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut min = [f32::INFINITY; 2];
        let mut max = [f32::NEG_INFINITY; 2];
        for pair in pcm.chunks_exact(2) {
            for channel in 0..2 {
                let value = if pair[channel].is_finite() {
                    pair[channel].clamp(-1.0, 1.0)
                } else {
                    0.0
                };
                min[channel] = min[channel].min(value);
                max[channel] = max[channel].max(value);
            }
        }
        self.blocks.force_push(Block {
            min,
            max,
            sequence,
            at: Instant::now(),
            epoch,
        });
    }

    pub fn subscribe(self: &Arc<Self>) -> Reader {
        self.readers.fetch_add(1, Ordering::Relaxed);
        Reader {
            shared: Arc::clone(self),
            started: Instant::now(),
            epoch: self.epoch(),
            history: VecDeque::new(),
        }
    }
}

// The audio-only window owns the single visualization consumer for its session.
pub(crate) struct Reader {
    shared: Arc<Waveform>,
    started: Instant,
    epoch: u64,
    history: VecDeque<Block>,
}

impl Reader {
    pub fn snapshot(&mut self) -> &VecDeque<Block> {
        let epoch = self.shared.epoch();
        if epoch != self.epoch {
            self.history.clear();
            self.epoch = epoch;
        }
        for _ in 0..CAPACITY {
            let Some(block) = self.shared.blocks.pop() else {
                break;
            };
            if block.epoch != epoch || block.at < self.started {
                continue;
            }
            if self.history.back().is_some_and(|last| {
                block.sequence <= last.sequence
                    || block.at.duration_since(last.at).as_secs_f32() > 0.5
            }) {
                self.history.clear();
            }
            self.history.push_back(block);
        }
        let now = Instant::now();
        while self.history.len() > (SECONDS * 100.0) as usize
            || self
                .history
                .front()
                .is_some_and(|b| now.duration_since(b.at).as_secs_f32() > SECONDS)
        {
            self.history.pop_front();
        }
        &self.history
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.shared.readers.fetch_sub(1, Ordering::Relaxed);
    }
}
