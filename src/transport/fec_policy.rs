//! Farwing forward-error-correction redundancy policy.
//!
//! Both Reed–Solomon users (the FCT control transport and the video RS-FEC
//! sender) protect blocks of `k` original packets with `m` repair packets of an
//! MDS code: any `k` of the `k + m` packets recover the block. Assuming
//! independent packet loss with probability `p`, the block is lost when more
//! than `m` of its `k + m` packets are lost:
//!
//! ```text
//! P_fail(k, m, p) = Σ_{i = m+1}^{k+m} C(k+m, i) · p^i · (1-p)^(k+m-i)
//! ```
//!
//! The policy picks the smallest `m` whose failure probability does not
//! exceed a target. The targets are Farwing's tuning parameters.

/// Target block-loss probability of the control transport (input, RPCs).
pub(crate) const CONTROL_TARGET: f64 = 0.015;
/// Target block-loss probability of ordinary video blocks.
pub(crate) const VIDEO_TARGET: f64 = 1e-3;
/// Below this measured loss the sender sends no video FEC: a clean link
/// carries exactly the media, and NACK repairs the odd loss.
pub(crate) const MIN_LOSS: f64 = 0.005;
/// Target block-loss probability of keyframes and high-RTT video blocks,
/// where a retransmission is expensive.
pub(crate) const VIDEO_STRICT_TARGET: f64 = 1e-4;

/// Probability that a block of `k` originals and `m` repairs cannot be
/// recovered at independent loss rate `p`.
pub(crate) fn failure_probability(k: usize, m: usize, p: f64) -> f64 {
    let n = k + m;
    if p <= 0.0 {
        return 0.0;
    }
    if p >= 1.0 {
        return 1.0;
    }
    // Iterate the binomial PMF to avoid overflow: pmf(i+1) = pmf(i) ·
    // (n-i)/(i+1) · p/(1-p). Sum the head (≤ m losses) and complement it.
    let ratio = p / (1.0 - p);
    let mut pmf = (1.0 - p).powi(n as i32);
    let mut recovered = pmf;
    for i in 0..m {
        pmf *= (n - i) as f64 / (i + 1) as f64 * ratio;
        recovered += pmf;
    }
    (1.0 - recovered).max(0.0)
}

/// Smallest repair count in `minimum..=maximum` meeting `target`, or `maximum`
/// when none does.
pub(crate) fn repair_count(
    k: usize,
    loss: f64,
    target: f64,
    minimum: usize,
    maximum: usize,
) -> usize {
    let maximum = maximum.max(minimum);
    let loss = if loss.is_finite() {
        loss.clamp(0.0, 1.0)
    } else {
        1.0
    };
    (minimum..=maximum)
        .find(|&m| failure_probability(k, m, loss) <= target)
        .unwrap_or(maximum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_and_bounded() {
        assert_eq!(failure_probability(10, 2, 0.0), 0.0);
        assert!((failure_probability(1, 0, 0.5) - 0.5).abs() < 1e-12);
        // k=2, m=1, p=6%: P(≥2 of 3 lost) = 3p²(1-p) + p³.
        let p: f64 = 0.06;
        let expected = 3.0 * p * p * (1.0 - p) + p.powi(3);
        assert!((failure_probability(2, 1, p) - expected).abs() < 1e-12);
        let mut previous = 0;
        for percent in 0..=50 {
            let m = repair_count(20, percent as f64 / 100.0, VIDEO_TARGET, 1, 20);
            assert!(m >= previous && (1..=20).contains(&m));
            previous = m;
        }
        assert_eq!(repair_count(5, 1.0, CONTROL_TARGET, 1, 5), 5);
        assert_eq!(repair_count(5, 0.0, CONTROL_TARGET, 1, 5), 1);
    }
}
