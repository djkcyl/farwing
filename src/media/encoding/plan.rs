//! The rate-control plan of a [`Rate`] for every encoder backend: the
//! policy lives here, each backend only maps it to its own parameters.
//! The buffer allowances differ by driver where measurements showed they
//! must.
use super::{Backend, Codec, Rate};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    pub target: u32,
    /// Never below `target`.
    pub peak: u32,
    /// The rate-control window (VBV / HRD buffer) in bits; `None` leaves
    /// the driver's own.
    pub buffer: Option<u64>,
    /// The lowest quantizer (0: none): a budget the picture cannot use is
    /// not spent on refining noise.
    pub min_qp: u32,
}

/// `rate` on `backend`, its buffer sized for `fps` input frames per second.
pub(crate) fn plan(rate: Rate, codec: Codec, fps: u32, size: (u32, u32), backend: Backend) -> Plan {
    let peak = rate.peak.max(rate.target);
    let per_second = u64::from(fps.max(1));
    let target = u64::from(rate.target);
    let buffer = if rate.smooth {
        // About one second: quality first for watching video.
        match backend {
            Backend::Amd => Some(u64::from(peak)),
            Backend::Nvidia | Backend::Intel => Some(target),
            Backend::Software => None,
        }
    } else {
        // A few frames: an IDR can borrow from its neighbours without
        // queueing. AV1 holds its quality at one frame.
        match (backend, codec) {
            (Backend::Nvidia, Codec::Av1) => Some((target / per_second).max(target / 8)),
            (Backend::Nvidia, _) => Some((target * 5 / per_second).max(target / 8)),
            (Backend::Amd, Codec::Av1) => Some(u64::from(peak) / per_second),
            // AMF needs more transient allocation than NVENC to recover an
            // IDR without a run of duplicate pictures.
            (Backend::Amd, _) => {
                let frames = if peak < 120_000_000 { 5 } else { 1 };
                Some((u64::from(peak) * frames / per_second).max(u64::from(peak) / 5))
            }
            (Backend::Intel, Codec::Av1) => Some(target / per_second),
            (Backend::Intel, _) | (Backend::Software, _) => None,
        }
    };
    let min_qp = if backend == Backend::Nvidia && codec == Codec::Av1 {
        av1_minimum_qp(rate.quality_target.bitrate, size, rate.quality_target.fps)
    } else {
        0
    };
    Plan {
        target: rate.target,
        peak,
        buffer,
        min_qp,
    }
}

/// The selected ceiling and frame rate stay stable through congestion and
/// idle input: one policy for every tier, custom rate and bit depth. Keep
/// the measured conservative floor at <= 0.1 bits/pixel/frame, then release
/// it smoothly by 0.2 so a generous budget can still improve precision.
/// This limits refinement, never forces frames to spend the bandwidth.
fn av1_minimum_qp(bitrate: u32, size: (u32, u32), fps: u32) -> u32 {
    let pixels_per_second = f64::from(size.0.max(1)) * f64::from(size.1.max(1)) * f64::from(fps.max(1));
    let density = f64::from(bitrate) / pixels_per_second;
    ((0.2 - density) * 400.0).round().clamp(0.0, 40.0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::encoding::QualityTarget;
    use crate::media::geometry::Tier;

    fn rate(target: u32, peak: u32, smooth: bool) -> Rate {
        Rate {
            target,
            peak,
            fps: 60,
            tier: Tier::P1080,
            quality_target: QualityTarget {
                bitrate: 8_000_000,
                fps: 60,
            },
            smooth,
        }
    }

    #[test]
    fn low_latency_buffers_hold_a_few_frames() {
        let r = rate(6_000_000, 8_000_000, false);
        let at = |backend, codec| plan(r, codec, 60, (1920, 1080), backend).buffer;
        assert_eq!(at(Backend::Nvidia, Codec::H264), Some(750_000));
        assert_eq!(at(Backend::Nvidia, Codec::Av1), Some(750_000), "an eighth of a second at least");
        assert_eq!(at(Backend::Amd, Codec::H265), Some(1_600_000));
        assert_eq!(at(Backend::Amd, Codec::Av1), Some(133_333));
        assert_eq!(at(Backend::Intel, Codec::Av1), Some(100_000));
        assert_eq!(at(Backend::Intel, Codec::H264), None);
        assert_eq!(at(Backend::Software, Codec::H264), None);
    }

    #[test]
    fn cinema_buffers_hold_a_second() {
        let r = rate(6_000_000, 8_000_000, true);
        let at = |backend| plan(r, Codec::H264, 60, (1920, 1080), backend).buffer;
        assert_eq!(at(Backend::Nvidia), Some(6_000_000));
        assert_eq!(at(Backend::Intel), Some(6_000_000));
        assert_eq!(at(Backend::Amd), Some(8_000_000));
    }

    #[test]
    fn av1_on_nvidia_keeps_a_quantizer_floor_for_thin_budgets() {
        let thin = plan(rate(1_000_000, 1_000_000, false), Codec::Av1, 60, (1920, 1080), Backend::Nvidia);
        assert!(thin.min_qp > 0);
        let mut generous = rate(60_000_000, 60_000_000, false);
        generous.quality_target.bitrate = 60_000_000;
        assert_eq!(plan(generous, Codec::Av1, 60, (1920, 1080), Backend::Nvidia).min_qp, 0);
        assert_eq!(plan(rate(1_000_000, 1_000_000, false), Codec::H264, 60, (1920, 1080), Backend::Nvidia).min_qp, 0);
    }
}
