// SPDX-License-Identifier: LGPL-2.1-or-later
//! Single-layer RC_BITRATE_MODE / GOM specialization.
//! Source formulas: Cisco OpenH264 ratectl.cpp (BSD-2-Clause), see notices.
use super::Config;
pub(super) const MAXIMUM_BITRATE: u32 = 288_000_000;
const QSTEP: [i64; 52] = [
    63, 71, 79, 89, 100, 112, 126, 141, 159, 178, 200, 224, 252, 283, 317, 356, 400, 449, 504, 566,
    635, 713, 800, 898, 1008, 1131, 1270, 1425, 1600, 1796, 2016, 2263, 2540, 2851, 3200, 3592,
    4032, 4525, 5080, 5702, 6400, 7184, 8063, 9051, 10159, 11404, 12800, 14368, 16127, 18102,
    20319, 22807,
];
fn round(n: i64, d: i64) -> i64 {
    if d > 0 { (n + d / 2) / d } else { n }
}
fn frame_budget(cfg: Config) -> i64 {
    // The public encoder accepts up to 144fps. Capping this divisor at 60
    // silently doubles the allowed bitrate when configured for 120fps.
    let fps = cfg.fps as f32;
    ((cfg.bitrate as f32 + 0.5 * fps) / fps) as i64
}
fn qp(step: i64) -> i32 {
    if step <= 63 {
        0
    } else {
        (6.0 * ((step as f32 / 100.0) as f64).log2() + 4.0 + 0.5) as i32
    }
}
#[derive(Default)]
struct Model {
    linear: i64,
    complexity: i64,
    count: u8,
}
impl Model {
    fn estimate(&self, complexity: i64, target: i64) -> i32 {
        let ratio = round(complexity.saturating_mul(100), self.complexity).clamp(80, 120);
        qp(round(
            self.linear.saturating_mul(ratio),
            target.saturating_mul(100),
        ))
    }
    fn update(&mut self, bits: i64, q: i32, complexity: i64) {
        let sample = bits * QSTEP[q as usize];
        if self.count == 0 {
            self.linear = sample;
            self.complexity = complexity;
        } else {
            self.linear = round(self.linear * 80 + sample * 20, 100);
            self.complexity = round(self.complexity * 80 + complexity * 20, 100);
        }
        self.count = self.count.saturating_add(1);
    }
}
pub(super) struct Rate {
    cfg: Config,
    bits_per_frame: i64,
    remaining: i64,
    allocated: i64,
    frames: u8,
    position: u8,
    intra: Model,
    inter: Model,
    last_qp: i32,
    initial_qp: i32,
    idr: bool,
    frame_qp: i32,
    min_qp: i32,
    max_qp: i32,
    target: i64,
    complexity: i64,
    group_size: usize,
    groups: Vec<u64>,
    group_start_bits: i64,
    group_target: i64,
    group_qp: i32,
    coded_qp: i64,
    coded_mbs: i64,
    buffer_bits: i64,
    skip_next: bool,
    buffer_limit: i64,
    max_bits_per_frame: i64,
    max_buffer_bits: [i64; 2],
    predicted_bits: i64,
    consecutive_skips: i64,
    window_start: Option<i64>,
    window_half: bool,
    previous_overflow: [bool; 2],
}
impl Rate {
    pub fn new(cfg: Config) -> Self {
        let bpf = frame_budget(cfg);
        Self {
            cfg,
            bits_per_frame: bpf,
            remaining: bpf * 8,
            allocated: bpf * 8,
            frames: 0,
            position: 0,
            intra: Model::default(),
            inter: Model::default(),
            last_qp: 30,
            initial_qp: 30,
            idr: true,
            frame_qp: 30,
            min_qp: 10,
            max_qp: 51,
            target: bpf * 4,
            complexity: 1,
            group_size: cfg.width.div_ceil(16) as usize
                * if cfg.width.div_ceil(16) * 16 < 496 {
                    1
                } else {
                    2
                },
            groups: Vec::new(),
            group_start_bits: 0,
            group_target: 0,
            group_qp: 30,
            coded_qp: 0,
            coded_mbs: 0,
            buffer_bits: 0,
            skip_next: false,
            buffer_limit: (i64::from(cfg.bitrate) + 1) / 2,
            max_bits_per_frame: frame_budget(Config {
                bitrate: MAXIMUM_BITRATE,
                ..cfg
            }),
            max_buffer_bits: [0; 2],
            predicted_bits: 0,
            consecutive_skips: 0,
            window_start: None,
            window_half: false,
            previous_overflow: [false; 2],
        }
    }
    pub fn configure(&mut self, cfg: Config) {
        self.cfg = cfg;
        // Admission runs before begin(). Waiting for an accepted picture to
        // apply a new rate leaves skipped inputs repaying the old budget and
        // delays recovery after a bandwidth increase. Keep the real bit debt,
        // but apply the new drain rate and capacity before the next decision.
        self.refresh_budget();
    }
    fn refresh_budget(&mut self) {
        let bpf = frame_budget(self.cfg);
        if bpf != self.bits_per_frame && self.bits_per_frame > 1 {
            self.remaining = round(self.remaining * bpf, self.bits_per_frame);
        }
        self.bits_per_frame = bpf;
        self.buffer_limit = (i64::from(self.cfg.bitrate) + 1) / 2;
        self.max_bits_per_frame = frame_budget(Config {
            bitrate: MAXIMUM_BITRATE,
            ..self.cfg
        });
    }
    pub fn request_keyframe(&mut self) {
        // Clear the window initialization flag. Automatic scene
        // and periodic IDRs do not call this explicit-request operation.
        self.window_start = None;
    }
    /// Explicit/period IDR wins over the pending RC skip.
    /// A skipped input repays one current frame budget without advancing VGOP.
    pub fn admit(&mut self, idr: bool, timestamp: i64) -> bool {
        // An unspecified maximum normalizes to 288 Mbps. Every input checks
        // overlapping 5 s windows with a 2.5 s shift.
        let time = timestamp / 10_000;
        if self.window_start.is_none() {
            self.buffer_bits = 0;
            self.max_buffer_bits = [0; 2];
            self.previous_overflow = [false; 2];
        }
        let start = *self.window_start.get_or_insert(time);
        let mut elapsed = time.wrapping_sub(start) as i32 as i64;
        if elapsed >= 2500 && !self.window_half {
            self.window_half = true;
            self.previous_overflow[0] =
                self.max_buffer_bits[1] > 0 && self.max_buffer_bits[1] != self.max_buffer_bits[0];
            self.max_buffer_bits[1] = 0;
        }
        let shifted = if elapsed >= 2500 {
            elapsed - 2500
        } else {
            elapsed + 2500
        };
        if elapsed >= 5000 || elapsed == 0 {
            self.window_start = Some(time);
            elapsed = 0;
            self.window_half = false;
            self.previous_overflow[1] = self.max_buffer_bits[0] > 0;
            self.max_buffer_bits[0] = 0;
        }
        let pending = std::mem::take(&mut self.skip_next);
        let must_skip = pending || {
            let predicted_target = (round(self.buffer_bits, self.bits_per_frame) + 1) >> 1;
            let predicted_max =
                (round(self.max_buffer_bits[0], self.max_bits_per_frame).max(0) + 1) >> 1;
            // The SDK stores the window interval and remaining-bit budget in
            // signed 32-bit fields, including when a long input gap wraps it.
            let available = |offset| {
                ((i64::from(MAXIMUM_BITRATE) * (5000 - offset) + 500) / 1000) as i32 as i64
            };
            let remaining = available(elapsed);
            let primary = self.max_buffer_bits[0] + self.predicted_bits;
            self.consecutive_skips <= predicted_target && self.buffer_bits > self.buffer_limit
                || elapsed > 2500
                    && (self.consecutive_skips <= predicted_max && primary > remaining
                        || self.previous_overflow[0]
                            && primary + self.max_bits_per_frame > remaining)
                || shifted > 2500
                    && self.previous_overflow[1]
                    && self.max_buffer_bits[1] + self.predicted_bits + self.max_bits_per_frame
                        > available(shifted)
        };
        if must_skip && !idr {
            self.buffer_bits = (self.buffer_bits - self.bits_per_frame).max(0);
            for debt in &mut self.max_buffer_bits {
                *debt -= self.max_bits_per_frame;
            }
            self.remaining += self.bits_per_frame;
            self.consecutive_skips += 1;
            return false;
        }
        true
    }
    pub fn begin(&mut self, idr: bool, complexity: u64, mb_cost: &[u32]) -> i32 {
        // VGOP allocation stays unchanged until the next boundary; admission
        // already consumes the current rate, including after skipped inputs.
        self.refresh_budget();
        self.consecutive_skips = 0;
        if idr || self.position >= 8 {
            self.remaining -= (8 - i64::from(self.position)) * (self.allocated / 8);
            self.remaining = if self.remaining < 0 {
                self.remaining + self.bits_per_frame * 8
            } else {
                self.bits_per_frame * 8
            };
            self.allocated = self.remaining;
            self.frames = 0;
            self.position = 0;
        }
        self.idr = idr;
        self.complexity = complexity as i64;
        let target = if idr {
            self.bits_per_frame * 4
        } else {
            round(self.remaining, 8 - i64::from(self.position))
        };
        self.position += 1;
        let exceeded = target <= 0;
        self.target = if idr {
            target
        } else {
            target.clamp(
                round(self.bits_per_frame * 55, 100),
                round(self.bits_per_frame * 150, 100),
            )
        }
        .max(1);
        self.frame_qp = if idr {
            let area =
                self.cfg.width.div_ceil(16) as usize * self.cfg.height.div_ceil(16) as usize * 256;
            let row = if area <= 28800 {
                0
            } else if area <= 115200 {
                1
            } else if area <= 460800 {
                2
            } else {
                3
            };
            let thresholds = [
                [0.25, 0.5, 0.75, 1.0],
                [0.1, 0.2, 0.3, 0.4],
                [0.03, 0.05, 0.09, 0.13],
                [0.01, 0.03, 0.06, 0.1],
            ];
            let bpp = self.cfg.bitrate as f64 / (self.cfg.fps as f64 * area as f64);
            let col = thresholds[row].iter().position(|&x| bpp <= x).unwrap_or(4);
            // Share AVC's useful quantizer range with hardware rate control.
            // The old 26..35 clamp prevented high-budget detail refinement
            // and repaid difficult scenes with skips instead of coarser QP.
            self.min_qp = 10;
            self.max_qp = 51;
            let initial = [
                [34, 28, 26, 24, 22],
                [36, 30, 28, 26, 24],
                [36, 32, 30, 28, 26],
                [36, 34, 32, 30, 28],
            ];
            let q = if self.intra.count == 0 {
                initial[row][col]
            } else {
                self.intra.estimate(self.complexity, self.target)
            };
            self.initial_qp = q.clamp(self.min_qp, self.max_qp);
            self.initial_qp
        } else {
            self.min_qp = (self.last_qp - 3).clamp(10, 51);
            self.max_qp = (self.last_qp + 5).clamp(10, 51);
            let q = if self.inter.count == 0 {
                self.initial_qp
            } else if exceeded {
                self.last_qp + 3
            } else {
                self.inter.estimate(self.complexity, self.target)
            };
            q.clamp(self.min_qp, self.max_qp)
        };
        self.group_qp = self.frame_qp;
        self.group_start_bits = 0;
        self.group_target = 0;
        self.coded_qp = 0;
        self.coded_mbs = 0;
        // Current SDK writes eight-row screen-complexity groups into its
        // RC buffer, whose decision cadence remains one/two MB rows. Retain
        // that verified producer/consumer mapping, including the zero tail.
        self.groups
            .resize(mb_cost.len().div_ceil(self.group_size), 0);
        self.groups.fill(0);
        let rows8 = self.cfg.width.div_ceil(16) as usize * 8;
        for (i, c) in mb_cost.chunks(rows8).enumerate() {
            self.groups[i] = c.iter().map(|&v| u64::from(v)).sum();
        }
        self.frame_qp
    }
    #[inline]
    pub fn reference_qp(&self) -> i32 {
        self.last_qp
    }
    pub fn mb_qp(&mut self, index: usize, bits: usize) -> i32 {
        if self.idr {
            return self.frame_qp;
        }
        if index % self.group_size == 0 {
            let i = index / self.group_size;
            let bits = bits as i64;
            let left = self.target - bits;
            if index > 0 {
                let spent = bits - self.group_start_bits;
                let expected = left + spent - self.group_target;
                let ratio = if left <= 0 || expected <= 0 {
                    0
                } else {
                    10000 * left / (expected + 1)
                };
                // The verified consumer's >10600 arm already covers >11900.
                self.group_qp += if ratio < 8409 {
                    2
                } else if ratio < 9439 {
                    1
                } else if ratio > 10600 {
                    -1
                } else {
                    0
                };
                self.group_qp = self.group_qp.clamp(self.min_qp, self.max_qp);
            }
            self.group_start_bits = bits;
            // RcGomTargetBits consumes complexity_index + 1. The initial
            // slice index is zero; the last group receives the remainder.
            self.group_target = if left <= 0 {
                0
            } else if i + 1 == self.groups.len() {
                left
            } else {
                let tail = &self.groups[i + 1..];
                let sum = tail.iter().sum::<u64>();
                if sum == 0 {
                    round(left, tail.len() as i64)
                } else {
                    round(left * tail[0] as i64, sum as i64)
                }
            };
        }
        self.group_qp
    }
    pub fn mb_done(&mut self, bits: usize, q: i32) {
        if bits > 0 {
            self.coded_qp += i64::from(q);
            self.coded_mbs += 1;
        }
    }
    pub fn finish(&mut self, bits: usize) {
        let avg = if self.idr || self.coded_mbs == 0 {
            self.frame_qp
        } else {
            round(self.coded_qp, self.coded_mbs) as i32
        };
        self.last_qp = avg;
        if self.idr {
            self.intra.update(bits as i64, avg, self.complexity);
        } else {
            self.inter.update(bits as i64, avg, self.complexity);
        }
        self.remaining -= bits as i64;
        // Target VBV debt and virtual-GOP deviation independently
        // request a skip. Maximum-rate windows are checked at admission.
        self.buffer_bits += bits as i64 - self.bits_per_frame;
        for debt in &mut self.max_buffer_bits {
            *debt += bits as i64 - self.max_bits_per_frame;
        }
        let qp_limit = if self.cfg.width.div_ceil(16) * 16 < 496 {
            24
        } else {
            31
        };
        let minimum = round(self.bits_per_frame * 55, 100);
        let future = i64::from(7u8.saturating_sub(self.frames)) * minimum;
        let excess =
            (future - self.remaining) as f64 * 100. / (8 * self.bits_per_frame) as f64 - 5.;
        self.skip_next = self.buffer_bits > self.buffer_limit && avg > qp_limit || excess > 10.;
        self.frames += 1;
        // Update the predictor here; this is not merely statistics.
        self.predicted_bits = if self.predicted_bits == 0 {
            bits as i64
        } else {
            (self.predicted_bits + bits as i64) / 2
        };
    }
}
