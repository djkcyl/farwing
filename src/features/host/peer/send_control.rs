//! One track's sending decisions: the picture it asks for (automatic
//! quality), the bitrate bounds of its transport, its frame rate under the
//! weak-network strategy and its vote for the pipeline. Everything that
//! sets a track's rate is decided here; the pipeline encodes at the minimum
//! of its members' votes.
use super::super::VideoConfig;
use super::encode_group::Vote;
use crate::features::host::format::Format;
use crate::features::host::parameters::{
    self, AutoQuality, Bounds, Degrade, NetworkSample, Quality, Target, Tier, WeakNetwork,
};
use std::time::Instant;

/// The settings an automatic tier was chosen for.
type Requested = (u64, Quality, Tier, u32, u32);

#[derive(Default)]
pub(super) struct SendControl {
    requested: Option<Requested>,
    automatic: Option<AutoQuality>,
    /// Until its first decision the estimate may probe up to the highest
    /// tier, so a fast link is recognised at once.
    initial: bool,
    logged: bool,
    degrade: Degrade,
    degraded: Option<u32>,
}

/// What a track's transport reports.
#[derive(Clone, Copy, Debug)]
pub(super) struct Link {
    /// What the track may send now (bit/s).
    pub media_rate: u32,
    /// Congestion-window reduction ratio.
    pub ratio: f64,
    /// What the track sent over the last second (bit/s).
    pub sent: u64,
}

impl SendControl {
    /// `config`'s settings: automatic quality starts over when switched to,
    /// or when the frame rate changes; other changes keep a settled tier.
    pub fn settings(&mut self, config: &VideoConfig) {
        let settings = (
            config.revision,
            config.quality,
            config.auto_tier,
            config.bitrate,
            config.fps,
        );
        if self.requested == Some(settings) {
            return;
        }
        let keep = config.quality == Quality::Auto
            && self.automatic.is_some()
            && self
                .requested
                .is_some_and(|(_, quality, _, _, fps)| quality == Quality::Auto && fps == config.fps);
        if !keep {
            self.automatic = (config.quality == Quality::Auto)
                .then(|| AutoQuality::new(config.auto_tier, config.maximum_tier));
            self.initial = true;
            self.logged = false;
        }
        self.requested = Some(settings);
    }

    /// The picture this track asks for, at most `maximum` (what the encoder
    /// takes of this screen). `sample`: the transport's estimate, once
    /// frames flow (`None` before).
    pub fn target(
        &mut self,
        now: Instant,
        config: &VideoConfig,
        maximum: Tier,
        budget: (u32, u32),
        sample: Option<NetworkSample>,
    ) -> Target {
        let Some(auto) = self.automatic.as_mut() else {
            // The fallback encoder's real size limit also limits the fixed
            // quality budget and QoS label, not only the GPU texture size.
            return match config.quality {
                Quality::Custom => Target::Custom { bps: config.bitrate },
                quality => Target::Tier(quality.tier(config.auto_tier).min(maximum)),
            };
        };
        auto.limit(maximum);
        if let Some(sample) = sample
            && auto
                .observe(now, config.format, budget, config.fps, true, sample)
                .is_some()
        {
            self.initial = false;
        }
        if auto.settled() {
            self.initial = false;
            if !self.logged {
                self.logged = true;
                tracing::info!(tier = ?auto.tier(), "automatic quality tier settled");
            }
        }
        Target::Tier(auto.tier())
    }

    /// The transport's bounds for the stream this track gets: `target` at
    /// `fps`, starting near `start` (what this controller reached last
    /// time over this kind of route).
    pub fn bounds(
        &self,
        format: Format,
        target: Target,
        budget: (u32, u32),
        fps: u32,
        start: Option<u32>,
    ) -> Bounds {
        let mut bounds = match (&self.automatic, target) {
            (Some(auto), Target::Tier(tier)) if !auto.settled() => parameters::automatic(
                format,
                tier,
                budget,
                fps,
                self.initial,
                auto.maximum(),
            ),
            _ => parameters::fixed(format, target, budget, fps),
        };
        bounds.initial = crate::features::host::bandwidth::start_rate(
            start,
            parameters::NETWORK_FLOOR,
            bounds.initial,
            bounds.maximum,
        );
        bounds
    }

    /// The frame rate under the weak-network `strategy`: the tier (size,
    /// bitrate ceiling) stays; a shortfall lowers the bitrate or the frame
    /// rate as the viewer chose.
    pub fn fps(&mut self, now: Instant, strategy: WeakNetwork, fps: u32, bounds: Bounds, link: Link) -> u32 {
        let rate_limited = link.sent as f64 >= f64::from(link.media_rate) * 0.7;
        let effective = self
            .degrade
            .fps(now, strategy, link.media_rate, bounds.maximum, fps, rate_limited);
        if effective != fps && self.degraded != Some(effective) {
            tracing::info!(
                requested = fps,
                fps = effective,
                ?strategy,
                "weak-network strategy lowered the frame rate"
            );
        }
        self.degraded = (effective != fps).then_some(effective);
        effective
    }

    /// This track's vote: what its transport takes now, up to the tier's
    /// ceiling; `None` while it cannot take frames.
    pub fn vote(active: bool, bounds: Bounds, link: Link, fps: u32) -> Option<Vote> {
        active.then(|| Vote {
            peak: link.media_rate.min(bounds.maximum),
            ratio: link.ratio,
            fps,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(quality: Quality) -> VideoConfig {
        VideoConfig {
            quality,
            fps: 60,
            maximum_tier: Tier::P2160,
            ..VideoConfig::default()
        }
    }

    #[test]
    fn fixed_tiers_fit_the_encoder_and_custom_keeps_its_rate() {
        let mut control = SendControl::default();
        let now = Instant::now();
        let tier = config(Quality::Tier(Tier::P2160));
        control.settings(&tier);
        assert_eq!(
            control.target(now, &tier, Tier::P1440, (2560, 1440), None),
            Target::Tier(Tier::P1440)
        );
        let custom = VideoConfig {
            bitrate: 20_000_000,
            ..config(Quality::Custom)
        };
        control.settings(&custom);
        assert_eq!(
            control.target(now, &custom, Tier::P1440, (2560, 1440), None),
            Target::Custom { bps: 20_000_000 }
        );
        let bounds = control.bounds(Format::AVC, Target::Custom { bps: 20_000_000 }, (2560, 1440), 60, None);
        assert_eq!(bounds.maximum, 20_000_000);
    }

    #[test]
    fn automatic_quality_survives_unrelated_changes_only() {
        let mut control = SendControl::default();
        let now = Instant::now();
        let auto = config(Quality::Auto);
        control.settings(&auto);
        assert_eq!(
            control.target(now, &auto, Tier::P2160, (1920, 1080), None),
            Target::Tier(Tier::P1080)
        );
        assert!(control.initial);
        // A probing start: the bounds may grow toward the highest tier.
        let bounds = control.bounds(Format::AVC, Target::Tier(Tier::P1080), (1920, 1080), 60, None);
        assert!(bounds.probe > bounds.maximum);
        control.initial = false;
        // Another revision with the same quality and frame rate keeps it...
        control.settings(&VideoConfig { revision: 1, ..auto });
        assert!(!control.initial);
        // ...a new frame rate starts over.
        control.settings(&VideoConfig { revision: 2, fps: 30, ..auto });
        assert!(control.initial);
    }

    #[test]
    fn the_vote_is_the_link_up_to_the_tier_ceiling() {
        let bounds = parameters::fixed(Format::AVC, Target::Tier(Tier::P1080), (1920, 1080), 60);
        let link = Link {
            media_rate: 50_000_000,
            ratio: 0.2,
            sent: 0,
        };
        let vote = SendControl::vote(true, bounds, link, 60).unwrap();
        assert_eq!((vote.peak, vote.ratio, vote.fps), (bounds.maximum, 0.2, 60));
        assert!(SendControl::vote(false, bounds, link, 60).is_none());
        // A remembered start rate sets where the transport begins.
        let control = SendControl::default();
        let started = control.bounds(Format::AVC, Target::Tier(Tier::P1080), (1920, 1080), 60, Some(6_000_000));
        assert!(started.initial > bounds.initial);
    }
}
