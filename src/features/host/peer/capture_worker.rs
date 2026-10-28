//! A participant's video track: its settings, automatic quality, weak-network
//! strategy and allowance, and the encoder pipeline it watches
//! ([`super::encode_group`], [`super::pipeline`]). The pipeline delivers the
//! frames to `sender` on this track's clock; this loop only steers.
use super::super::Lease;
use super::super::VideoConfig;
use super::super::capture;
use super::super::encoder;
use super::super::lock;
use super::super::output_size;
use super::Published;
use super::encode_group::{self, End};
use crate::features::host::parameters::{Quality, Target, Tier};
use crate::i18n::t;
use anyhow::Context as _;
use anyhow::Result;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The forwarder's low layer: the 流畅 tier (720p, 1.5 Mbps at 60 fps).
const LOW_TIER: Tier = Tier::P720;
const LOW_FPS: u32 = 60;

pub(super) fn capture_loop(
    mut screen: capture::Screen,
    handle: Lease,
    cancel: CancellationToken,
    connected: Arc<AtomicBool>,
    business_ready: Arc<super::screens::Ready>,
    config: Arc<Mutex<VideoConfig>>,
    keyframe: Arc<AtomicBool>,
    sender: mpsc::Sender<encoder::Encoded>,
    started: Instant,
    transport: crate::features::host::transport::Transport,
    negotiated: Arc<crate::features::host::format::Negotiated>,
    publication: tokio::sync::watch::Sender<Published>,
    pointer: Arc<Mutex<Option<crate::platform::windows::cursor_shape::Snapshot>>>,
    feedback: Arc<crate::features::host::recovery::TrackFeedback>,
    forwarded: Arc<crate::features::host::forwarding::Flags>,
    slot: usize,
) -> Result<()> {
    let _activity = crate::platform::windows::host_service::activity::Work::new();
    // This viewer's references and recovery requests, for the pipeline.
    let mut viewer = crate::features::host::recovery::Viewer::default();
    let mut isolated = 0u64;
    // Encoder candidates whose pipeline failed.
    let mut failed = std::collections::HashSet::new();
    let mut requested = None;
    let mut automatic = None::<crate::features::host::parameters::AutoQuality>;
    let mut initial_auto = true;
    let mut auto_logged = false;
    let mut degrade = crate::features::host::parameters::Degrade::default();
    // Start near what this controller reached last time over this kind of
    // route, once the route is chosen (when the connection is up, before the
    // encoder starts).
    let mut remembered = None::<(&'static str, Option<u32>)>;
    let mut degraded_fps = None::<u32>;
    // What this track got over the last second: is it using its budget?
    let mut output_bps = 0u64;
    let mut membership = None::<encode_group::Membership>;
    // Forwarding just ended: back to this participant's own encoder key.
    let mut forwarded_before = false;
    // A frame went to this participant directly: its viewer has the track a
    // forwarded stream stands in for.
    let mut shown = false;
    // This stream's formats count towards the shared viewer stream's format.
    let _shared_registration = crate::features::host::shared_view::register(
        handle.participant(),
        crate::features::host::shared_view::formats(&negotiated),
    );
    let mut was_shared = None::<bool>;
    // The pipeline's encoder and source last reported for this participant.
    let mut followed = None::<(i32, (u32, u32), String, capture::Screen)>;
    let mut last_diagnostics = None;

    while !cancel.is_cancelled() && handle.requested() {
        let ready_seen = business_ready.changes();
        // A pipeline that ended sends its members elsewhere.
        if let Some(end) = membership.as_ref().and_then(encode_group::Membership::ended) {
            let ended = membership.take().map(|m| m.group_key().clone());
            match end {
                End::Candidate => {
                    if let Some(key) = ended {
                        tracing::warn!(?key, "encoder pipeline failed; trying another candidate");
                        failed.insert((key.adapter, key.backend, key.format));
                    }
                }
                End::SourceGone => return Err(capture::SourceGone.into()),
                End::Failed(message) => anyhow::bail!(message),
                End::Stopped => {}
            }
        }
        let mut wanted = *lock(&config);
        if !negotiated.permits_format(wanted.format) {
            let previous = wanted.revision;
            let chroma = wanted.format.chroma;
            let hdr = wanted.color.is_hdr();
            negotiated.apply_requested(&mut wanted, chroma, hdr, (screen.width, screen.height))?;
            let mut active = lock(&config);
            if active.revision != previous {
                continue;
            }
            wanted.revision = previous.wrapping_add(1);
            *active = wanted;
        }

        // Several participants: everyone without the control token watches
        // the one shared stream (the stored settings stay for later).
        let shared = crate::features::host::shared_view::apply(
            handle.participant(),
            &mut wanted,
            forwarded.in_room.load(Ordering::Acquire),
        );
        if was_shared != Some(shared) {
            was_shared = Some(shared);
            tracing::info!(shared, format = ?wanted.format, "participant stream policy");
        }
        let requested_hdr = wanted.color.is_hdr();
        let automatic_requested = wanted.quality == Quality::Auto;
        // Gate HDR on the screen's reported state, so the keys of members
        // agree; the pipeline checks its source again.
        negotiated.apply_source(&mut wanted, (screen.width, screen.height), screen.hdr)?;
        let settings = (
            wanted.revision,
            wanted.quality,
            wanted.auto_tier,
            wanted.bitrate,
            wanted.fps,
        );
        if requested != Some(settings) {
            // An unrelated setting change keeps the settled automatic tier;
            // only switching to Auto or changing the frame rate starts over.
            let keep = wanted.quality == Quality::Auto
                && automatic.is_some()
                && requested.is_some_and(|(_, quality, _, _, fps)| {
                    quality == Quality::Auto && fps == wanted.fps
                });
            if !keep {
                automatic = (wanted.quality == Quality::Auto).then(|| {
                    crate::features::host::parameters::AutoQuality::new(
                        wanted.auto_tier,
                        wanted.maximum_tier,
                    )
                });
                initial_auto = true;
                auto_logged = false;
            }
            requested = Some(settings);
        }
        let source_gpu = screen.render_adapter.unwrap_or(screen.adapter);
        let candidate = shared
            .then(|| {
                negotiated.candidate(&wanted, (screen.width, screen.height), source_gpu, &failed, true)
            })
            .flatten()
            .or_else(|| {
                negotiated.candidate(&wanted, (screen.width, screen.height), source_gpu, &failed, false)
            })
            .context(t().features.peer_no_negotiated_encoder)?;
        wanted.format = candidate.capability.format;
        if wanted.format.depth < 10 {
            wanted.color = crate::features::host::format::Color::Sdr;
        }
        wanted.maximum = wanted.requested_maximum.map_or(candidate.maximum, |cap| {
            (candidate.maximum.0.min(cap.0), candidate.maximum.1.min(cap.1))
        });
        wanted.fps = wanted.fps.min(candidate.fps).min(screen.fps).max(1);
        // The key uses the tier before this viewer's decoder cap and
        // weak-network strategy; those only lower the pipeline's frame rate.
        let tier_fps = wanted.fps;
        if wanted.receiver_fps > 0 {
            wanted.fps = wanted.fps.min(wanted.receiver_fps);
        }
        let maximum_tier = negotiated.maximum_tier((screen.width, screen.height), wanted.maximum);
        let budget_size =
            crate::media::geometry::fit_size(screen.width, screen.height, wanted.maximum);
        let mut target = if let Some(auto) = automatic.as_mut() {
            auto.limit(maximum_tier);
            let sample = transport.automatic_rates();
            let has_frames = membership
                .as_ref()
                .and_then(encode_group::Membership::last_frame)
                .is_some_and(|at| at.elapsed() < Duration::from_secs(1));
            if wanted.sending
                && wanted.capturing
                && connected.load(Ordering::Acquire)
                && auto
                    .observe(Instant::now(), wanted.format, budget_size, wanted.fps, has_frames, sample)
                    .is_some()
            {
                initial_auto = false;
            }
            Target::Tier(auto.tier())
        } else {
            // The fallback encoder's real size limit also limits the fixed
            // quality budget and QoS label, not only the GPU texture size.
            match wanted.quality {
                Quality::Custom => Target::Custom { bps: wanted.bitrate },
                quality => Target::Tier(quality.tier(wanted.auto_tier).min(maximum_tier)),
            }
        };

        // In a forwarding room: the uplink takes this stream's frames too;
        // once the participant shows them, instead.
        let forwarding = forwarded.in_room.load(Ordering::Acquire) && shown;
        let replaced = forwarding && forwarded.receiving.load(Ordering::Acquire);
        // The forwarder's low layer for a participant whose downlink cannot
        // carry the main stream (`deployment-design.md` §5.5).
        let low = forwarding && forwarded.low.load(Ordering::Acquire);
        if low {
            target = Target::Tier(LOW_TIER);
            wanted.fps = wanted.fps.min(LOW_FPS);
        }
        let tier_fps = if low { tier_fps.min(LOW_FPS) } else { tier_fps };

        if !wanted.capturing {
            if membership.take().is_some() || last_diagnostics.take().is_some() {
                handle.video(None);
            }
            followed = None;
            transport.pause();
            publication.send_if_modified(|state| {
                if state.capturing || state.visible {
                    state.capturing = false;
                    state.visible = false;
                    true
                } else {
                    false
                }
            });
            business_ready.wait(ready_seen, Duration::from_millis(20));
            continue;
        }

        // Which pipeline this participant watches.
        let key = encode_group::Key {
            screen: screen.device_name.clone(),
            adapter: candidate.capability.adapter,
            backend: candidate.capability.backend,
            format: candidate.capability.format,
            hdr: requested_hdr,
            maximum: wanted.maximum,
            fps: tier_fps,
            cursor: wanted.cursor_capture,
            cinema: wanted.cinema,
            target,
            isolated,
            low,
        };
        let regroup = membership.as_ref().is_none_or(|m| {
            if forwarding {
                // Forwarded viewers share whichever stream of this screen
                // and format there is (one per layer): one stream goes to
                // the forwarder.
                m.group_key().screen != key.screen
                    || m.group_key().format != key.format
                    || m.group_key().low != key.low
            } else {
                m.wanted != key || forwarded_before
            }
        });
        forwarded_before = forwarding;
        if regroup {
            // Leave first: the old pipeline must not count against the limit.
            drop(membership.take());
            let size = output_size(budget_size.0, budget_size.1, target.tier());
            let joining = encode_group::Joining {
                frames: sender.clone(),
                clock: started,
                start: super::pipeline::Start {
                    screen: screen.clone(),
                    gpu_compatibility: negotiated.gpu_compatibility,
                },
            };
            membership = Some(if forwarding {
                encode_group::REGISTRY.join_forwarded(key, size, joining)
            } else {
                encode_group::REGISTRY.join(key, size, encode_group::MAX_ENCODERS, joining)
            });
        }
        let membership = membership.as_mut().context(t().features.peer_shared_group_missing)?;

        // A settled automatic tier is kept for the stream: nothing to probe
        // toward any more.
        let settled = automatic.as_ref().is_some_and(|auto| auto.settled());
        if settled {
            initial_auto = false;
        }
        let mut bounds = if automatic.is_some() && !settled {
            crate::features::host::parameters::automatic(
                wanted.format,
                target.tier(),
                budget_size,
                wanted.fps,
                initial_auto,
                automatic.as_ref().map_or(Tier::P2160, |auto| auto.maximum()),
            )
        } else {
            crate::features::host::parameters::fixed(wanted.format, target, budget_size, wanted.fps)
        };
        let start = transport.route_class().and_then(|route| match remembered {
            Some((known, bps)) if known == route => bps,
            _ => {
                let bps = crate::features::host::bandwidth::remembered(
                    &handle.controller_device(),
                    route,
                );
                if let Some(bps) = bps {
                    tracing::info!(bps, route, "host start bitrate from the previous session");
                }
                remembered = Some((route, bps));
                bps
            }
        });
        bounds.initial = crate::features::host::bandwidth::start_rate(
            start,
            crate::features::host::parameters::NETWORK_FLOOR,
            bounds.initial,
            bounds.maximum,
        );
        // Weak-network strategy: the tier (size, bitrate ceiling) stays; a
        // shortfall lowers the bitrate or the frame rate as the viewer chose.
        if let Some(rate) = membership.delivered_rate() {
            output_bps = rate;
        }
        let available = transport.media_rate();
        let rate_limited = output_bps as f64 >= f64::from(available) * 0.7;
        let effective_fps = degrade.fps(
            Instant::now(),
            wanted.weak_network,
            available,
            bounds.maximum,
            wanted.fps,
            rate_limited,
        );
        if effective_fps != wanted.fps && degraded_fps != Some(effective_fps) {
            tracing::info!(
                requested = wanted.fps,
                fps = effective_fps,
                strategy = ?wanted.weak_network,
                "weak-network strategy lowered the frame rate"
            );
        }
        degraded_fps = (effective_fps != wanted.fps).then_some(effective_fps);
        wanted.fps = effective_fps;
        // This participant's allowance; the pipeline encodes for the
        // weakest active member.
        membership.set_forwarded(forwarding.then(|| (handle.participant().unwrap_or_default(), slot)));
        let own_active = connected.load(Ordering::Acquire)
            && business_ready.get()
            && transport.media_ready()
            && wanted.sending
            && !replaced;
        let own_peak = transport.media_rate().min(bounds.maximum);
        membership.vote(own_active.then(|| encode_group::Vote {
            peak: own_peak,
            ratio: transport.cwnd_ratio(),
            fps: wanted.fps,
        }));
        if let Some(group) = membership.aggregate().filter(|group| group.fps > 0) {
            wanted.fps = group.fps;
        }
        transport.fps(wanted.fps);
        transport.smooth(wanted.cinema);
        if let Some(auto) = automatic.as_ref()
            && auto.settled()
            && !auto_logged
        {
            auto_logged = true;
            tracing::info!(tier = ?auto.tier(), "automatic quality tier settled");
        }
        if wanted.sending {
            transport.configure(bounds);
        } else {
            transport.pause();
        }

        // This viewer's keyframe needs (PLI/FIR, a reoffer) go to the
        // pipeline once it takes frames: one made before is missed.
        if own_active
            && own_peak >= crate::features::host::parameters::MIN_ENCODER_RATE
            && keyframe.swap(false, Ordering::AcqRel)
        {
            membership.request_keyframe();
        }
        if let Some(output) = membership.output_changed() {
            screen = output.screen.clone();
            let source = (
                output.implementation,
                output.maximum_size,
                output.capture.clone(),
                output.screen.clone(),
            );
            if followed.as_ref() != Some(&source) {
                handle.encoder(
                    output.implementation,
                    output.maximum_size,
                    &output.screen,
                    &output.capture,
                );
                handle.source(&output.screen, &output.capture);
                followed = Some(source);
            }
            let (preferred_gpu, preferred_codec) = negotiated.preference();
            let video = crate::features::host::ActiveEncoding {
                preferred_gpu,
                preferred_codec,
                selection: negotiated.selection_reason(&candidate.capability, !failed.is_empty()),
                ..output.video
            };
            if last_diagnostics != Some(video) {
                handle.video(Some(video));
                last_diagnostics = Some(video);
            }
            let state = Published {
                automatic: automatic_requested,
                hdr: requested_hdr && output.format.depth == 10,
                screen: output.screen,
                capturing: true,
                visible: output.visible,
                target: output.target,
                fps: output.fps,
                format: output.format,
                budget_size: output.budget_size,
                encoder: Some(output.backend),
                capture: output.capture,
            };
            publication.send_if_modified(|old| {
                if *old != state {
                    *old = state;
                    true
                } else {
                    false
                }
            });
        }
        *lock(&pointer) = membership.cursor();
        // The pipeline decides references for every member's viewer.
        let (held, request) = feedback.take();
        isolated = viewer.update(
            Instant::now(),
            held.as_ref(),
            feedback.take_keyframe_requests(),
            membership.has_others(),
        );
        // A repair the transport refused will not arrive: recover now
        // rather than when the viewer gives up on it.
        let refused = if transport.take_repair_refused() {
            viewer.refused(Instant::now(), held.as_ref(), feedback.browser())
        } else {
            crate::features::host::recovery::Refused::Nothing
        };
        if refused == crate::features::host::recovery::Refused::Keyframe {
            tracing::debug!(target: "farwing::recovery", "repair refused; keyframe for the browser");
            keyframe.store(true, Ordering::Release);
        }
        let request = request.or(match refused {
            crate::features::host::recovery::Refused::Recover(request) => Some(request),
            _ => None,
        });
        membership.report(held, request, feedback.rtt());
        shown |= membership.last_frame().is_some();
        business_ready.wait(ready_seen, Duration::from_millis(20));
    }
    Ok(())
}
