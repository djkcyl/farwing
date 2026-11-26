//! Bounded, aggregate timings for a host that supplies fewer frames than requested.
use std::time::{Duration, Instant};

#[derive(Default)]
pub(super) struct Stage {
    count: u64,
    total: Duration,
    maximum: Duration,
}
impl Stage {
    pub fn record(&mut self, elapsed: Duration) {
        self.absorb(1, elapsed, elapsed);
    }
    pub fn absorb(&mut self, count: u64, total: Duration, maximum: Duration) {
        self.count += count;
        self.total += total;
        self.maximum = self.maximum.max(maximum);
    }
    fn average_ms(&self) -> f64 {
        self.total.as_secs_f64() * 1000.0 / self.count.max(1) as f64
    }
    fn maximum_ms(&self) -> f64 {
        self.maximum.as_secs_f64() * 1000.0
    }
}

pub(super) struct Metrics {
    started: Instant,
    pub capture: Stage,
    pub transfer: Stage,
    pub rate: Stage,
    pub encode: Stage,
    pub work: Stage,
    pub new_frames: u64,
    pub encoded_frames: u64,
    pub empty_captures: u64,
    pub admission_drops: u64,
    pub transfer_busy: u64,
    pub backpressure_waits: u64,
    pub send_drops: u64,
    /// 本窗口编出去的关键帧，以及其中「我们要的」与「编码器自己插的」各多少。
    pub keyframes: u64,
    pub forced_keyframes: u64,
    pub scene_keyframes: u64,
    pub keyframe_bytes: u64,
    pub encoded_bytes: u64,
}
impl Default for Metrics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            capture: Stage::default(),
            transfer: Stage::default(),
            rate: Stage::default(),
            encode: Stage::default(),
            work: Stage::default(),
            new_frames: 0,
            encoded_frames: 0,
            empty_captures: 0,
            admission_drops: 0,
            transfer_busy: 0,
            backpressure_waits: 0,
            send_drops: 0,
            keyframes: 0,
            forced_keyframes: 0,
            scene_keyframes: 0,
            keyframe_bytes: 0,
            encoded_bytes: 0,
        }
    }
}
impl Metrics {
    /// 把独立编码线程的统计并进本采样窗口。
    pub fn absorb(&mut self, stats: super::pipeline::Stats) {
        self.encoded_frames += stats.encoded;
        self.encoded_bytes += stats.encoded_bytes;
        self.keyframes += stats.keyframes;
        self.forced_keyframes += stats.forced_keyframes;
        self.scene_keyframes += stats.scene_keyframes;
        self.keyframe_bytes += stats.keyframe_bytes;
        self.encode
            .absorb(stats.encode_count, stats.encode_total, stats.encode_max);
        self.send_drops += stats.send_drops;
        self.transfer_busy += stats.busy;
    }
    pub fn report(&mut self, target_fps: u32, source_adapter: u64, encoder_adapter: u64) {
        let elapsed = self.started.elapsed();
        if elapsed < Duration::from_secs(5) {
            return;
        }
        if self.capture.count != 0 {
            tracing::info!(
                target_fps,
                source_adapter,
                encoder_adapter,
                sample_ms = elapsed.as_millis(),
                capture_attempts = self.capture.count,
                new_frames = self.new_frames,
                encoded_frames = self.encoded_frames,
                encoded_fps = self.encoded_frames as f64 / elapsed.as_secs_f64(),
                empty_captures = self.empty_captures,
                admission_drops = self.admission_drops,
                transfer_busy = self.transfer_busy,
                backpressure_waits = self.backpressure_waits,
                send_drops = self.send_drops,
                capture_avg_ms = self.capture.average_ms(),
                capture_max_ms = self.capture.maximum_ms(),
                transfer_avg_ms = self.transfer.average_ms(),
                transfer_max_ms = self.transfer.maximum_ms(),
                rate_avg_ms = self.rate.average_ms(),
                rate_max_ms = self.rate.maximum_ms(),
                encode_avg_ms = self.encode.average_ms(),
                encode_max_ms = self.encode.maximum_ms(),
                work_avg_ms = self.work.average_ms(),
                work_max_ms = self.work.maximum_ms(),
                keyframes = self.keyframes,
                forced_keyframes = self.forced_keyframes,
                scene_keyframes = self.scene_keyframes,
                keyframe_share = self.keyframe_bytes as f64 / self.encoded_bytes.max(1) as f64,
                keyframe_avg_bytes = self.keyframe_bytes / self.keyframes.max(1),
                "host capture and encoding throughput"
            );
        }
        *self = Self::default();
    }
}
