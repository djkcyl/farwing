//! Fixed-rate stereo resampling (quality 5). Playback uses 48 kHz input;
//! microphone capture converts the device's native rate to 48 kHz.
//!
//! Rust adaptation of SpeexDSP 1.2.1 resample.c by Jean-Marc Valin (2007).
//! Filter design and coefficients retain the Speex BSD license; see
//! COPYING.SpeexDSP. This is not an implementation of Speex echo cancellation,
//! jitter buffering, or its unused variable-rate/quality APIs.

use anyhow::{Result, ensure};

const INPUT_RATE: usize = 48_000;
const INPUT_CHUNK: usize = 160;
const BASE_TAPS: usize = 80;

pub(crate) struct Resampler {
    taps: usize,
    denominator: usize,
    integer_step: usize,
    fraction_step: usize,
    position: usize,
    phase: usize,
    history: Vec<[f32; 2]>,
    filter: Filter,
}

enum Filter {
    Direct(Vec<f32>),
    Interpolated { table: Vec<f32>, oversample: usize },
}

impl Resampler {
    pub fn new(output_rate: u32) -> Result<Self> {
        Self::with_rates(INPUT_RATE as u32, output_rate)
    }

    pub(crate) fn with_rates(input_rate: u32, output_rate: u32) -> Result<Self> {
        ensure!(
            (8_000..=384_000).contains(&output_rate) && (8_000..=384_000).contains(&input_rate),
            "不支持采样率转换：{input_rate} → {output_rate}"
        );
        let input_rate = input_rate as usize;
        let output_rate = output_rate as usize;
        let divisor = gcd(input_rate, output_rate);
        let numerator = input_rate / divisor;
        let denominator = output_rate / divisor;
        let mut taps = BASE_TAPS;
        let mut oversample = 16;
        let cutoff = if numerator > denominator {
            // The reference truncates the rational tap count before aligning it.
            taps = (BASE_TAPS * numerator / denominator).next_multiple_of(8);
            for factor in [2, 4, 8, 16] {
                if factor * denominator < numerator {
                    oversample >>= 1;
                }
            }
            0.922_f32 * denominator as f32 / numerator as f32
        } else {
            0.940_f32
        };
        let filter = if taps * denominator <= taps * oversample + 8 {
            let mut table = Vec::with_capacity(taps * denominator);
            for phase in 0..denominator {
                for tap in 0..taps {
                    let x = (tap as i32 - taps as i32 / 2 + 1) as f32
                        - phase as f32 / denominator as f32;
                    table.push(sinc(cutoff, x, taps));
                }
            }
            Filter::Direct(table)
        } else {
            let table = (0..taps * oversample + 8)
                .map(|index| {
                    sinc(
                        cutoff,
                        (index as i32 - 4) as f32 / oversample as f32 - (taps / 2) as f32,
                        taps,
                    )
                })
                .collect();
            Filter::Interpolated { table, oversample }
        };
        Ok(Self {
            taps,
            denominator,
            integer_step: numerator / denominator,
            fraction_step: numerator % denominator,
            position: 0,
            phase: 0,
            // Preserve the original zero history and group delay. No skip_zeros.
            history: vec![[0.0; 2]; taps - 1 + INPUT_CHUNK],
            filter,
        })
    }

    /// Returns interleaved samples consumed/produced. No allocation or locking
    /// occurs here. A short output buffer leaves the remaining input to caller.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<(usize, usize)> {
        ensure!(
            input.len().is_multiple_of(2) && output.len().is_multiple_of(2),
            "音频重采样声道不完整"
        );
        let mut consumed = 0;
        let mut produced = 0;
        let history_len = self.taps - 1;
        while consumed < input.len() / 2 && produced < output.len() / 2 {
            let frames = (input.len() / 2 - consumed).min(INPUT_CHUNK);
            for (frame, sample) in self.history[history_len..history_len + frames]
                .iter_mut()
                .zip(input[consumed * 2..].chunks_exact(2))
            {
                *frame = [sample[0], sample[1]];
            }
            while self.position < frames && produced < output.len() / 2 {
                let samples = &self.history[self.position..self.position + self.taps];
                let result = match &self.filter {
                    Filter::Direct(table) => {
                        let coefficients =
                            &table[self.phase * self.taps..(self.phase + 1) * self.taps];
                        let mut sum = [0.0; 2];
                        for (&coefficient, sample) in coefficients.iter().zip(samples) {
                            sum[0] += coefficient * sample[0];
                            sum[1] += coefficient * sample[1];
                        }
                        sum
                    }
                    Filter::Interpolated { table, oversample } => {
                        let fraction = self.phase * oversample;
                        let offset = fraction / self.denominator;
                        let fraction =
                            (fraction % self.denominator) as f32 / self.denominator as f32;
                        let mut sums = [[0.0; 4]; 2];
                        let start = 2 + oversample - offset;
                        for (index, sample) in samples.iter().enumerate() {
                            let index = start + index * oversample;
                            let coefficients = &table[index..index + 4];
                            // Keep the four independent accumulators explicit:
                            // the application's size-optimized build can then
                            // vectorize them without changing summation order.
                            sums[0][0] += sample[0] * coefficients[0];
                            sums[0][1] += sample[0] * coefficients[1];
                            sums[0][2] += sample[0] * coefficients[2];
                            sums[0][3] += sample[0] * coefficients[3];
                            sums[1][0] += sample[1] * coefficients[0];
                            sums[1][1] += sample[1] * coefficients[1];
                            sums[1][2] += sample[1] * coefficients[2];
                            sums[1][3] += sample[1] * coefficients[3];
                        }
                        let weights = cubic(fraction);
                        sums.map(|sum| {
                            weights[0] * sum[0]
                                + weights[1] * sum[1]
                                + weights[2] * sum[2]
                                + weights[3] * sum[3]
                        })
                    }
                };
                output[produced * 2..produced * 2 + 2].copy_from_slice(&result);
                produced += 1;
                self.position += self.integer_step;
                self.phase += self.fraction_step;
                if self.phase >= self.denominator {
                    self.phase -= self.denominator;
                    self.position += 1;
                }
            }
            let used = self.position.min(frames);
            self.position -= used;
            self.history.copy_within(used..used + history_len, 0);
            consumed += used;
        }
        Ok((consumed * 2, produced * 2))
    }
}

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

fn cubic(fraction: f32) -> [f32; 4] {
    let first = -0.16667_f32 * fraction + 0.16667_f32 * fraction * fraction * fraction;
    let second =
        fraction + 0.5_f32 * fraction * fraction - 0.5_f32 * fraction * fraction * fraction;
    let fourth = -0.33333_f32 * fraction + 0.5_f32 * fraction * fraction
        - 0.16667_f32 * fraction * fraction * fraction;
    let third = (1.0_f64 - first as f64 - second as f64 - fourth as f64) as f32;
    [first, second, third, fourth]
}

fn sinc(cutoff: f32, x: f32, taps: usize) -> f32 {
    if (x as f64).abs() < 1e-6 {
        return cutoff;
    }
    if (x as f64).abs() > 0.5 * taps as f64 {
        return 0.0;
    }
    let angle = std::f64::consts::PI * (x * cutoff) as f64;
    (cutoff as f64 * angle.sin() / angle * window((2.0 * x as f64 / taps as f64).abs() as f32))
        as f32
}

fn window(x: f32) -> f64 {
    let position = x * 32.0;
    let index = position.floor() as usize;
    let fraction = position - index as f32;
    let square = (fraction * fraction) as f64;
    let cube = (fraction * fraction * fraction) as f64;
    let fraction = fraction as f64;
    let d = -0.1666666667 * fraction + 0.1666666667 * cube;
    let c = fraction + 0.5 * square - 0.5 * cube;
    let a = -0.3333333333 * fraction + 0.5 * square - 0.1666666667 * cube;
    let b = 1.0 - d - c - a;
    a * KAISER10[index]
        + b * KAISER10[index + 1]
        + c * KAISER10[index + 2]
        + d * KAISER10[index + 3]
}

const KAISER10: [f64; 36] = [
    0.99537781, 1.0, 0.99537781, 0.98162644, 0.95908712, 0.92831446, 0.89005583, 0.84522401,
    0.79486424, 0.74011713, 0.68217934, 0.62226347, 0.56155915, 0.50119680, 0.44221549, 0.38553619,
    0.33194107, 0.28205962, 0.23636152, 0.19515633, 0.15859932, 0.12670280, 0.09935205, 0.07632451,
    0.05731132, 0.04193980, 0.02979584, 0.02044510, 0.01345224, 0.00839739, 0.00488951, 0.00257636,
    0.00115101, 0.00035515, 0.0, 0.0,
];
