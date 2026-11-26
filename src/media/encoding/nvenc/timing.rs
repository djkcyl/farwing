//! Bounded diagnostics for the driver's synchronous encode path. CPU call
//! durations include any driver waits; conversion submission is not GPU time.
//! The platform wrapper times its own input stages (0: conversion, 1: CUDA
//! input); the shared session times mapping, encoding and the bitstream lock.
use std::time::{Duration, Instant};

pub(crate) struct Frame {
    started: Instant,
    checkpoint: Instant,
    stages: [Duration; 5],
}
impl Frame {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            checkpoint: now,
            stages: [Duration::ZERO; 5],
        }
    }
    pub fn stage(&mut self, index: usize) {
        let now = Instant::now();
        self.stages[index] = now.duration_since(self.checkpoint);
        self.checkpoint = now;
    }
}

#[derive(Default)]
pub(crate) struct Diagnostics {
    last_report: Option<Instant>,
    slow_frames: u64,
    worst: Option<(Duration, [Duration; 5], Duration, bool)>,
}
impl Diagnostics {
    pub fn record(&mut self, frame: Frame, success: bool) {
        let now = Instant::now();
        let total = now.duration_since(frame.started);
        if total >= Duration::from_millis(100) {
            self.slow_frames += 1;
            if self.worst.as_ref().is_none_or(|old| total > old.0) {
                self.worst = Some((
                    total,
                    frame.stages,
                    now.duration_since(frame.checkpoint),
                    success,
                ));
            }
        }
        if self
            .last_report
            .is_some_and(|last| now.duration_since(last) < Duration::from_secs(5))
        {
            return;
        }
        if let Some((total, stages, output, success)) = self.worst.take() {
            tracing::warn!(
                slow_frames = self.slow_frames,
                total_ms = total.as_secs_f64() * 1000.,
                conversion_submit_ms = stages[0].as_secs_f64() * 1000.,
                cuda_input_ms = stages[1].as_secs_f64() * 1000.,
                map_input_ms = stages[2].as_secs_f64() * 1000.,
                encode_picture_ms = stages[3].as_secs_f64() * 1000.,
                lock_bitstream_ms = stages[4].as_secs_f64() * 1000.,
                output_cleanup_ms = output.as_secs_f64() * 1000.,
                success,
                "NVENC slow frame driver stages (worst sample)"
            );
            self.slow_frames = 0;
            self.last_report = Some(now);
        }
    }
}
