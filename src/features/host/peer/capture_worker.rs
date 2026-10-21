//! Capture/encoder worker lifecycle; owns driver objects on one thread.
//!
//! Participants watching the same screen with the same parameters share one
//! encoder ([`super::encode_group`]): the group's leader captures and
//! encodes, the followers forward its frames on their own transports.
use super::super::Lease;
use super::super::VideoConfig;
use super::super::capture;
use super::super::encoder;
use super::super::lock;
use super::super::output_size;
use super::Published;
use super::encode_group;
use crate::features::host::format::{Backend, Color, Format};
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

mod metrics;

/// A follower that lost frames asks again if no keyframe came this long.
const KEYFRAME_RETRY: Duration = Duration::from_secs(1);

/// How long a new participant waits for its own `CaptureSetting` before its
/// session start parameters count as its settings.
const SETTINGS_WAIT: Duration = Duration::from_secs(2);

/// The device the last encoder used: when the worker ends it keeps the
/// driver loaded until a fresh device is ready for a reconnect
/// (`capture::keep_for_reconnect`).
struct Reconnect(Option<(u64, windows::Win32::Graphics::Direct3D11::ID3D11Device)>);

impl Drop for Reconnect {
    fn drop(&mut self) {
        if let Some((adapter, device)) = self.0.take() {
            capture::keep_for_reconnect(adapter, device);
        }
    }
}

pub(super) fn capture_loop(
    mut screen: capture::Screen,
    handle: Lease,
    cancel: CancellationToken,
    encode_cancel: Arc<AtomicBool>,
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
) -> Result<()> {
    // Declared first, dropped last: after the encoder and transfer that use
    // the device.
    let mut reconnect = Reconnect(None);
    let _activity = crate::platform::windows::host_service::activity::Work::new();
    let mut runtime = encoder::Runtime::new()?;
    let mut desktop = None;
    let mut encoder = None::<encoder::Encoder>;
    // `frame-meta` numbering of this participant's encoder, and its
    // long-term reference decisions.
    let mut frame_ids = crate::media::frame_meta::Counter::default();
    let mut decider = crate::features::host::recovery::Decider::new(0);
    let mut viewer = crate::features::host::recovery::Viewer::default();
    let mut isolated = 0u64;
    let mut current: Option<((u32, u32), ((u64, Backend, Format), Color))> = None;
    let mut next = Instant::now();
    let mut paced_fps = 0;
    let mut cached = None::<capture::Frame>;
    let mut last_frame = None::<Instant>;
    let mut failed = std::collections::HashSet::new();
    let mut encode_errors = 0u32;
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
    // Encoded bytes of the last second: is the encoder using its budget?
    let mut encoded_recently = std::collections::VecDeque::<(Instant, usize)>::new();
    let mut admission = crate::features::host::parameters::WindowAdmission::default();
    let mut generation = 0;
    let mut encoding_device = None::<(u64, windows::Win32::Graphics::Direct3D11::ID3D11Device)>;
    let mut transfer = None::<crate::features::host::transfer::Transfer>;
    let mut frame_metadata = std::collections::BTreeMap::new();
    let mut last_diagnostics = None;
    let mut metrics = metrics::Metrics::default();
    let mut paused_capture_at = None::<Instant>;
    // A new duplication can first report only the pointer; until its first
    // image, wait for the next present rather than the next frame time.
    let mut quick_retries = 0u8;
    // The cached image was captured before media could flow and is still
    // unsent: the first frame, not a repeat.
    let mut unsent_cache = false;
    // Shared encoding: this participant's group, if capturing.
    let mut membership = None::<encode_group::Membership>;
    let mut leading = true;
    // Follower: decoding must (re)start at a keyframe; when one was asked.
    let mut awaiting = true;
    let mut keyframe_asked = None::<Instant>;
    // This stream's formats count towards the shared viewer stream's format.
    let _shared_registration = crate::features::host::shared_view::register(
        handle.participant(),
        crate::features::host::shared_view::formats(&negotiated),
    );
    let mut was_shared = None::<bool>;
    // Leader: frames went only to the others while this participant could
    // not take them; its own stream restarts at a keyframe.
    let mut own_needs_keyframe = false;
    let mut encoder_info = None::<(i32, (u32, u32))>;
    // Follower: the shared encoder and source last reported for this
    // participant.
    let mut followed = None::<(i32, (u32, u32), String, capture::Screen)>;

    let worker_started = Instant::now();
    while !cancel.is_cancelled() && handle.requested() {
        let iteration_started = Instant::now();
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
        let shared = crate::features::host::shared_view::apply(handle.participant(), &mut wanted);
        if was_shared != Some(shared) {
            was_shared = Some(shared);
            tracing::info!(shared, format = ?wanted.format, "participant stream policy");
        }
        let requested_hdr = wanted.color.is_hdr();
        // Gate HDR on the actual source, not only the requested setting.
        // Keep the request so same-source HDR recovery can re-enable the
        // negotiated format without rewriting user intent. Without a source
        // of its own (before opening it, or as a follower) the screen's
        // reported HDR state decides, so group keys agree.
        negotiated.apply_source(
            &mut wanted,
            (screen.width, screen.height),
            desktop
                .as_ref()
                .map_or(screen.hdr, |desktop: &capture::Desktop| desktop.hdr_available()),
        )?;
        let settings = (
            wanted.revision,
            wanted.quality,
            wanted.auto_quality,
            wanted.bitrate,
            wanted.fps,
        );
        let changed = requested != Some(settings);
        if changed {
            // An unrelated setting change keeps the settled automatic tier;
            // only switching to Auto or changing the frame rate starts over.
            let keep = wanted.quality == 5
                && automatic.is_some()
                && requested.is_some_and(|(_, quality, _, _, fps)| quality == 5 && fps == wanted.fps);
            if !keep {
                automatic = (wanted.quality == 5).then(|| {
                    crate::features::host::parameters::AutoQuality::new(
                        wanted.auto_quality,
                        wanted.maximum_quality,
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
            .context("协商的编码器均不可用")?;
        let hardware =
            candidate.capability.backend != crate::features::host::format::Backend::Software;
        let candidate_id = (
            candidate.capability.adapter,
            candidate.capability.backend,
            candidate.capability.format,
        );
        wanted.format = candidate.capability.format;
        if wanted.format.depth < 10 {
            wanted.color = crate::features::host::format::Color::Sdr;
        }
        wanted.maximum = candidate.maximum_for(wanted.requested_maximum);
        wanted.fps = wanted.fps.min(candidate.fps).min(screen.fps).max(1);
        // The group key uses the tier before this viewer's decoder cap and
        // weak-network strategy; those only lower the group's frame rate.
        let tier_fps = wanted.fps;
        if wanted.receiver_fps > 0 {
            wanted.fps = wanted.fps.min(wanted.receiver_fps);
        }
        metrics.report(wanted.fps, screen.adapter, candidate.capability.adapter);
        let maximum_quality = negotiated.maximum_quality(
            wanted.format,
            (screen.width, screen.height),
            wanted.maximum,
        );
        let budget_size =
            crate::media::geometry::fit_size(screen.width, screen.height, wanted.maximum);
        if let Some(auto) = automatic.as_mut() {
            auto.limit(maximum_quality);
            let sample = transport.automatic_rates();
            if wanted.sending
                && wanted.capturing
                && connected.load(Ordering::Acquire)
                && auto
                    .observe(
                        Instant::now(),
                        wanted.format,
                        budget_size,
                        wanted.fps,
                        last_frame.is_some_and(|at| at.elapsed() < Duration::from_secs(1)),
                        sample,
                    )
                    .is_some()
            {
                initial_auto = false;
            }
            wanted.quality = auto.quality();
        } else if wanted.quality != 6 {
            // The fallback encoder's real size limit also limits the fixed
            // quality budget and QoS label, not only the GPU texture size.
            wanted.quality = wanted.quality.min(maximum_quality);
        }

        // Which encoder this participant uses: lead a group or follow one.
        if wanted.capturing {
            let key = encode_group::Key {
                screen: screen.device_name.clone(),
                adapter: candidate_id.0,
                backend: candidate_id.1,
                format: candidate_id.2,
                hdr: requested_hdr,
                maximum: wanted.maximum,
                fps: tier_fps,
                cursor: wanted.cursor_capture,
                cinema: wanted.cinema,
                quality: wanted.quality,
                bitrate: if wanted.quality == 6 { wanted.bitrate } else { 0 },
                isolated,
            };
            // Until this participant's own settings arrive (or a moment
            // passes, for a controller that sends none), the start
            // parameters only stand in: share an encoder rather than open one.
            let configured = wanted.configured || worker_started.elapsed() >= SETTINGS_WAIT;
            // A follower whose group already has the key stays.
            if let Some(m) = membership.as_mut()
                && !m.is_leader()
                && *m.group_key() == key
            {
                m.wanted = key.clone();
                m.provisional = false;
            }
            // A leader that only stands in for another key at the encoder
            // limit regroups, so the members of its group keep their stream;
            // so does a provisional follower once its settings are known.
            let regroup = membership.as_ref().is_none_or(|m| {
                m.wanted != key
                    || (*m.group_key() != key && (m.is_leader() || (m.provisional && configured)))
            });
            if regroup {
                // Leave first: the old group must not count against the limit.
                drop(membership.take());
                let size = output_size(budget_size.0, budget_size.1, wanted.quality);
                membership = Some(if configured {
                    encode_group::REGISTRY.join(key, size, encode_group::MAX_ENCODERS)
                } else {
                    encode_group::REGISTRY.join_provisional(key, size)
                });
                awaiting = true;
                keyframe_asked = None;
            }
        } else {
            membership = None;
        }
        let was_leading = leading;
        leading = membership.as_ref().is_none_or(|m| m.is_leader());
        if leading {
            followed = None;
        }
        if !leading && was_leading {
            // Another participant encodes this stream now.
            if desktop.is_some() || encoder.is_some() {
                tracing::info!("participant follows a shared encoder; releasing its own");
            }
            desktop = None;
            encoder = None;
            encoder_info = None;
            current = None;
            transfer = None;
            encoding_device = None;
            cached = None;
            runtime.end_gpu_work();
            if last_diagnostics.take().is_some() {
                handle.video(None);
            }
        } else if leading && !was_leading {
            // Took over a group (or left one): start from a keyframe.
            if let Some(membership) = membership.as_mut() {
                membership.discard_queued();
            }
            keyframe.store(true, Ordering::Release);
            next = Instant::now();
            last_frame = None;
        }

        if leading && desktop.is_none() && wanted.sending && wanted.capturing {
            // Prepare the authorized track's native capture while the offer,
            // ICE and DTLS are still on their way; the gate below still
            // forbids frame acquisition/encoding/sending until transport is up.
            // Nothing is kept warm outside this track's lease/cancellation scope.
            let opened_at = Instant::now();
            let connecting = !connected.load(Ordering::Acquire);
            runtime.begin_gpu_work(negotiated.gpu_compatibility);
            desktop = Some(capture::Desktop::open_selected(&screen)?);
            generation = desktop.as_ref().unwrap().generation;
            quick_retries = 3;
            tracing::info!(
                elapsed_ms = opened_at.elapsed().as_millis(),
                connecting,
                "host capture preparation completed"
            );
            // Native setup may overlap incoming CaptureSetting changes. Read
            // the current revision before configuring or encoding any frame.
            continue;
        }

        let mut bounds = if automatic.is_some() {
            crate::features::host::parameters::automatic(
                wanted.format,
                wanted.quality,
                budget_size,
                wanted.fps,
                initial_auto,
                automatic.as_ref().map_or(4, |auto| auto.maximum()),
            )
        } else {
            crate::features::host::parameters::fixed(
                wanted.format,
                wanted.quality,
                wanted.bitrate,
                budget_size,
                wanted.fps,
            )
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
        let quality_target = crate::features::host::format::QualityTarget {
            bitrate: bounds.maximum,
            fps: wanted.fps,
        };
        // Weak-network strategy: the tier (size, bitrate ceiling) stays; a
        // shortfall lowers the bitrate or the frame rate as the viewer chose.
        let now = Instant::now();
        while encoded_recently
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > Duration::from_secs(1))
        {
            encoded_recently.pop_front();
        }
        let available = transport.media_rate();
        let output_bps = encoded_recently.iter().map(|(_, bytes)| *bytes as u64 * 8).sum::<u64>();
        let rate_limited = output_bps as f64 >= f64::from(available) * 0.7;
        let effective_fps = degrade.fps(
            now,
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
        // This participant's allowance; the group encodes for the weakest
        // active member.
        let ready_seen = business_ready.changes();
        let own_active = connected.load(Ordering::Acquire)
            && business_ready.get()
            && transport.media_ready()
            && wanted.sending
            && wanted.capturing;
        let own_peak = transport.media_rate().min(bounds.maximum);
        let codec_minimum = crate::features::host::parameters::MIN_ENCODER_RATE;
        let shared = membership.as_ref().and_then(|membership| {
            membership.vote(own_active.then(|| encode_group::Vote {
                peak: own_peak,
                ratio: transport.cwnd_ratio(),
                fps: wanted.fps,
            }));
            membership.aggregate(codec_minimum)
        });
        if let Some(group) = shared {
            wanted.fps = group.fps.max(1);
        }
        let group_peak = shared.map_or(own_peak, |group| group.peak);
        let group_ratio = shared.map_or_else(|| transport.cwnd_ratio(), |group| group.ratio);
        transport.fps(wanted.fps);
        transport.smooth(wanted.cinema);
        let period = Duration::from_secs_f64(1.0 / f64::from(wanted.fps));
        if paced_fps != wanted.fps {
            paced_fps = wanted.fps;
            next = Instant::now();
        }
        if let Some(auto) = automatic.as_ref()
            && auto.settled()
            && !auto_logged
        {
            auto_logged = true;
            tracing::info!(quality = auto.quality(), "automatic quality tier settled");
        }
        if wanted.sending && wanted.capturing {
            transport.quality(automatic.is_some(), wanted.quality, changed);
            transport.configure(bounds);
        } else {
            transport.pause();
        }

        if !leading {
            let membership = membership.as_mut().context("缺少共享编码组")?;
            // This viewer's own keyframe needs (PLI/FIR, a reoffer) go to
            // the shared encoder.
            if keyframe.swap(false, Ordering::AcqRel) {
                membership.request_keyframe();
                keyframe_asked = Some(Instant::now());
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
                if last_diagnostics != Some(output.video) {
                    handle.video(Some(output.video));
                    last_diagnostics = Some(output.video);
                }
                let mut active = lock(&config);
                if active.revision == wanted.revision {
                    active.reported_quality = output.quality;
                }
                drop(active);
                let state = Published {
                    screen: output.screen,
                    capturing: true,
                    visible: true,
                    quality: output.quality,
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
            // The leader decides references for every member's viewer.
            let (held, request) = feedback.take();
            isolated = viewer.update(
                Instant::now(),
                held.as_ref(),
                feedback.take_keyframe_requests(),
                membership.has_others(),
            );
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
            membership.report(held, request);
            let Some(received) = membership.receive(Duration::from_millis(20)) else {
                if awaiting
                    && own_active
                    && keyframe_asked.is_none_or(|at| at.elapsed() >= KEYFRAME_RETRY)
                {
                    membership.request_keyframe();
                    keyframe_asked = Some(Instant::now());
                }
                continue;
            };
            let frame = received.frame;
            if received.gap || !own_active || own_peak < codec_minimum {
                // Missed frames, or this viewer cannot take them now: its
                // decoder restarts at the next keyframe.
                awaiting = true;
            }
            if !own_active || own_peak < codec_minimum {
                continue;
            }
            if awaiting && !frame.keyframe {
                if keyframe_asked.is_none_or(|at| at.elapsed() >= KEYFRAME_RETRY) {
                    membership.request_keyframe();
                    keyframe_asked = Some(Instant::now());
                }
                continue;
            }
            let Some(encoded) = frame.encoded(started) else {
                continue;
            };
            if encoded.keyframe {
                awaiting = false;
                keyframe_asked = None;
                tracing::info!("follower forwarded a shared keyframe");
            }
            metrics.encoded_frames += 1;
            encoded_recently.push_back((Instant::now(), encoded.data.len()));
            last_frame = Some(frame.captured);
            if cancel.is_cancelled() || !handle.requested() {
                break;
            }
            sender.blocking_send(encoded).context("画面发送队列已关闭")?;
            continue;
        }

        // Leader: encode while this participant or another member takes
        // frames.
        let others = membership.as_ref().is_some_and(|m| m.others_active());
        // While the connection is made, open the encoder from the first
        // image of the prepared capture: once media may flow, the first
        // frame only has to be encoded.
        let preroll = !own_active
            && !others
            && wanted.sending
            && wanted.capturing
            && desktop.is_some()
            && encoder.is_none();
        if !own_active && wanted.capturing && others {
            own_needs_keyframe = true;
        } else if !own_active && !preroll {
            if !wanted.capturing {
                if last_diagnostics.take().is_some() {
                    handle.video(None);
                }
                desktop = None;
                encoder = None;
                encoder_info = None;
                current = None;
                transfer = None;
                encoding_device = None;
                publication.send_if_modified(|state| {
                    if state.capturing || state.visible {
                        state.capturing = false;
                        state.visible = false;
                        true
                    } else {
                        false
                    }
                });
            }
            if !wanted.capturing || !unsent_cache {
                cached = None;
            }
            if !wanted.capturing {
                runtime.end_gpu_work();
            }
            last_frame = None;
            keyframe.store(true, Ordering::Release);
            next = Instant::now();
            business_ready.wait(ready_seen, Duration::from_millis(20));
            continue;
        } else if std::mem::take(&mut own_needs_keyframe) {
            keyframe.store(true, Ordering::Release);
        }
        if Instant::now() < next {
            std::thread::sleep(
                next.saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(20)),
            );
            continue;
        }
        if own_active && sender.capacity() == 0 {
            metrics.backpressure_waits += 1;
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        if desktop.is_none() {
            runtime.begin_gpu_work(negotiated.gpu_compatibility);
            desktop = Some(capture::Desktop::open_selected(&screen)?);
            generation = desktop.as_ref().unwrap().generation;
            quick_retries = 3;
            // Readiness can change after the preparation gate above. Apply the
            // same policy and reread settings after either native open path.
            continue;
        }
        if group_peak < codec_minimum && !preroll {
            // Keep source/cursor/recovery observations alive during a transport
            // pause, without preprocessing the desktop at the full video rate.
            // Do not advance WindowAdmission: it counts actual candidate frames.
            if paused_capture_at.is_some_and(|at| at.elapsed() < Duration::from_millis(100)) {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            paused_capture_at = Some(Instant::now());
        } else {
            paused_capture_at = None;
        }
        let wait_ms = ((1000.0 / f64::from(wanted.fps)).round() as u32).max(1);
        let capture_started = Instant::now();
        // Advance the planned deadline, not the (usually late) wake-up time.
        // Otherwise sleep/dispatch overhead is added to every frame period.
        // A whole missed period resets the clock instead of replaying a backlog.
        next = if capture_started.saturating_duration_since(next) >= period {
            capture_started + period
        } else {
            next + period
        };
        let captured = desktop.as_mut().context("缺少采集源")?.next(
            wait_ms,
            wanted.quality,
            wanted.cursor_capture,
            wanted.color.is_hdr(),
            wanted.maximum,
        );
        metrics.capture.record(capture_started.elapsed());
        let captured = captured?;
        let capture_state = desktop.as_ref().context("缺少采集源")?;
        *lock(&pointer) = capture_state.cursor.clone();
        if let Some(membership) = membership.as_ref() {
            membership.set_cursor(capture_state.cursor.clone());
        }
        if generation != capture_state.generation {
            generation = capture_state.generation;
            encoder = None;
            encoder_info = None;
            cached = None;
            current = None;
            last_frame = None;
            transfer = None;
            keyframe.store(true, Ordering::Release);
        }
        screen = capture_state.screen.clone();
        // A selected source remains attached during temporary capture loss.
        // Removing its track here can make the receiver unsubscribe and prevent
        // the next capture attempt. Visibility has its own permission report.
        publication.send_if_modified(|state| {
            let changed = state.screen != screen || !state.capturing;
            if state.screen != screen {
                state.screen.clone_from(&screen);
            }
            state.capturing = true;
            changed
        });
        handle.source(&screen, capture_state.backend_name());
        if wanted.color.is_hdr()
            != (requested_hdr && capture_state.hdr_available() && wanted.format.depth == 10)
        {
            // Capture recovery or source refresh changed the effective format.
            // Rebuild preprocessing/encoder before admitting this frame.
            cached = None;
            continue;
        }
        if !capture_state.available {
            cached = None;
            publication.send_if_modified(|state| {
                if state.visible {
                    state.visible = false;
                    true
                } else {
                    false
                }
            });
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        let frame = match captured {
            Some(frame) => {
                metrics.new_frames += u64::from(frame.is_new);
                cached = Some(frame.clone());
                unsent_cache = false;
                frame
            }
            None => {
                metrics.empty_captures += 1;
                let Some(mut frame) = cached.clone() else {
                    if quick_retries > 0 {
                        quick_retries -= 1;
                        next = capture_started;
                    }
                    continue;
                };
                if last_frame.is_some_and(|at| {
                    at.elapsed() < Duration::from_secs_f64(1.0 / f64::from(wanted.fps.min(30)))
                }) {
                    continue;
                }
                frame.captured = Instant::now();
                frame.is_new = std::mem::take(&mut unsent_cache);
                frame
            }
        };
        let peak = group_peak;
        // A zero/tiny allocation is a transport pause, not an encoder failure.
        // Do not fabricate bandwidth by clamping it back to a quality floor.
        if peak < codec_minimum && !preroll {
            continue;
        }
        let (media_rate, discard_frame) = if preroll {
            (peak, false)
        } else {
            admission.next(peak, codec_minimum, group_ratio)
        };
        if discard_frame {
            metrics.admission_drops += 1;
            continue;
        }
        let size = output_size(frame.width, frame.height, wanted.quality);
        let source_device = &desktop.as_ref().context("缺少采集源")?.device;
        // NVENC owns a separate D3D11 context even on the capture adapter.
        // Transfer explicitly orders capture/preprocessing before encoding,
        // instead of relying on NVENC's handling of the capture context.
        let isolated_nvenc = hardware
            && candidate.capability.backend == crate::features::host::format::Backend::Nvidia;
        let device =
            if !hardware || (!isolated_nvenc && candidate.capability.adapter == screen.adapter) {
                source_device.clone()
            } else {
                if encoding_device
                    .as_ref()
                    .is_none_or(|(adapter, _)| *adapter != candidate.capability.adapter)
                {
                    match capture::create_device(candidate.capability.adapter) {
                        Ok((device, _)) => {
                            tracing::info!(
                                source_adapter = screen.adapter,
                                encoder_adapter = candidate.capability.adapter,
                                isolated_nvenc,
                                "host encoding device created independently of capture"
                            );
                            encoding_device = Some((candidate.capability.adapter, device))
                        }
                        Err(error) => {
                            tracing::warn!(%error,?candidate_id,"encoder adapter failed");
                            failed.insert(candidate_id);
                            continue;
                        }
                    }
                }
                encoding_device.as_ref().unwrap().1.clone()
            };
        if current.is_some_and(|(old_size, old_candidate)| {
            (old_size != size || old_candidate != (candidate_id, wanted.color))
                && old_candidate.0.0 == candidate_id.0
                && old_candidate.0.1 == candidate_id.1
        }) && let Some(active) = encoder.as_mut()
        {
            let rate = crate::features::host::format::Rate {
                target: bounds.initial.max(300_000),
                peak: bounds.initial.max(300_000),
                fps: wanted.fps,
                quality: wanted.quality,
                quality_target,
                smooth: wanted.cinema,
            };
            match active.reconfigure_stream(&device, size, wanted.format, rate, wanted.color) {
                Ok(true) => {
                    handle.encoder(
                        active.implementation(),
                        active.maximum_size(),
                        &screen,
                        desktop.as_ref().context("缺少采集源")?.backend_name(),
                    );
                    encoder_info = Some((active.implementation(), active.maximum_size()));
                    current = Some((size, (candidate_id, wanted.color)));
                    frame_metadata.clear();
                    frame_ids.restart();
                    decider.reset(active.ltr_slots());
                    encode_errors = 0;
                    keyframe.store(true, Ordering::Release);
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%error, ?size, ?candidate_id, "host encoder stream reconfiguration rejected; rebuilding candidate")
                }
            }
        }
        if current.is_none_or(|(old_size, old_candidate)| {
            old_size != size || old_candidate != (candidate_id, wanted.color)
        }) {
            let rebuilding = Instant::now();
            // Release before replacement to avoid consuming a second driver
            // session solely for a size/candidate transition.
            drop(encoder.take());
            let close_ms = rebuilding.elapsed().as_secs_f64() * 1000.;
            let created = if hardware {
                encoder::Encoder::hardware_format(
                    &device,
                    size,
                    wanted.format,
                    crate::features::host::format::Rate {
                        target: bounds.initial.max(300_000),
                        peak: bounds.initial.max(300_000),
                        fps: wanted.fps,
                        quality: wanted.quality,
                        quality_target,
                        smooth: wanted.cinema,
                    },
                    wanted.color,
                )
            } else {
                encoder::Encoder::software_format(
                    &device,
                    size,
                    wanted.format,
                    crate::features::host::format::Rate {
                        target: bounds.initial.max(300_000),
                        peak: bounds.initial.max(300_000),
                        fps: wanted.fps,
                        quality: wanted.quality,
                        quality_target,
                        smooth: wanted.cinema,
                    },
                )
            };
            encoder = match created {
                Ok(created) => {
                    tracing::info!(
                        ?size,
                        ?candidate_id,
                        close_ms,
                        elapsed_ms = rebuilding.elapsed().as_secs_f64() * 1000.,
                        "host encoder replacement completed"
                    );
                    handle.encoder(
                        created.implementation(),
                        created.maximum_size(),
                        &screen,
                        desktop.as_ref().context("缺少采集源")?.backend_name(),
                    );
                    encoder_info = Some((created.implementation(), created.maximum_size()));
                    frame_ids.restart();
                    decider.reset(created.ltr_slots());
                    reconnect.0 = Some((candidate.capability.adapter, device.clone()));
                    Some(created)
                }
                Err(error) => {
                    tracing::warn!(%error,?candidate_id,"host encoder initialization failed; disabling this candidate");
                    failed.insert(candidate_id);
                    encoder = None;
                    encoder_info = None;
                    cached = None;
                    current = None;
                    continue;
                }
            };
            current = Some((size, (candidate_id, wanted.color)));
            if let Some(membership) = membership.as_ref() {
                membership.set_size(size);
            }
            frame_metadata.clear();
            encode_errors = 0;
            keyframe.store(true, Ordering::Release);
        }
        if preroll {
            unsent_cache = true;
            tracing::debug!(?size, "host encoder prepared while connecting");
            continue;
        }
        let active_encoder = encoder.as_mut().context("缺少编码器")?;
        let needs_transfer = device != *source_device;
        let transfer_started = Instant::now();
        let prepared = (|| -> Result<Option<crate::features::host::transfer::Delivery>> {
            if !needs_transfer {
                return Ok(None);
            }
            if transfer
                .as_ref()
                .is_none_or(|t| !t.matches(source_device, &device, &frame))
            {
                transfer = Some(crate::features::host::transfer::Transfer::new(
                    source_device,
                    &device,
                    &frame,
                )?);
            }
            transfer
                .as_mut()
                .unwrap()
                .copy(&frame, || !encode_cancel.load(Ordering::Acquire))
        })();
        if needs_transfer {
            metrics.transfer.record(transfer_started.elapsed());
        }
        if needs_transfer && prepared.as_ref().is_ok_and(|frame| frame.is_none()) {
            metrics.transfer_busy += 1;
            continue;
        }
        // Any member's keyframe need (a newcomer, PLI/FIR, lost frames) is
        // this encoder's.
        let shared_keyframe = membership.as_ref().is_some_and(|m| m.take_keyframe());
        let encoded = (|| -> Result<_> {
            // Delivery failures share the source-loss/candidate-recovery exits.
            let delivery = prepared?;
            let rate_started = Instant::now();
            let reconfigured = active_encoder.configure_rate(crate::features::host::format::Rate {
                target: media_rate,
                peak,
                fps: wanted.fps,
                quality: wanted.quality,
                quality_target,
                smooth: wanted.cinema,
            });
            metrics.rate.record(rate_started.elapsed());
            if reconfigured? {
                keyframe.store(true, Ordering::Release);
            }
            // Network requests are rate-limited at their RTCP entry. Explicit
            // quality/source controls and failed inputs must not wait 600ms.
            let own_keyframe = keyframe.swap(false, Ordering::AcqRel);
            let (held, requests) = {
                let (held, request) = feedback.take();
                isolated = viewer.update(
                    Instant::now(),
                    held.as_ref(),
                    feedback.take_keyframe_requests(),
                    membership.as_ref().is_some_and(|m| m.has_others()),
                );
                // A repair the transport refused will not arrive: recover
                // now rather than when the viewer gives up on it.
                let refused = if transport.take_repair_refused() {
                    viewer.refused(Instant::now(), held.as_ref(), feedback.browser())
                } else {
                    crate::features::host::recovery::Refused::Nothing
                };
                if refused == crate::features::host::recovery::Refused::Keyframe {
                    tracing::debug!(target: "farwing::recovery", ?held, "repair refused; keyframe for the browser");
                    keyframe.store(true, Ordering::Release);
                }
                let request = request.or(match refused {
                    crate::features::host::recovery::Refused::Recover(request) => Some(request),
                    _ => None,
                });
                if request.as_ref().is_some_and(|r| r.request == 0) {
                    tracing::debug!(target: "farwing::recovery", "repair refused; sending a recovery frame");
                }
                match membership.as_ref() {
                    Some(membership) => {
                        membership.report(held, request);
                        membership.take_feedback()
                    }
                    None => (held.into_iter().collect(), request.into_iter().collect()),
                }
            };
            let plan = decider.plan(
                Instant::now(),
                own_keyframe | shared_keyframe,
                frame_ids.epoch(),
                &held,
                &requests,
                feedback.rtt().unwrap_or(Duration::from_millis(100)),
            );
            if !requests.is_empty() {
                tracing::debug!(target: "farwing::recovery", ?requests, control = ?plan.control,
                    "recovery requested");
            }
            let force = plan.control.keyframe;
            if force && membership.as_ref().is_some_and(|m| m.has_others()) {
                tracing::info!(
                    own = own_keyframe,
                    member = shared_keyframe,
                    "shared encoder encodes a keyframe for its group"
                );
            }
            let encode_started = Instant::now();
            let timestamp = (frame.captured.duration_since(started).as_nanos() / 100) as i64;
            frame_metadata.insert(
                timestamp,
                (
                    frame.captured,
                    encode_started,
                    frame.is_new,
                    frame.hdr_metadata,
                ),
            );
            while frame_metadata.len() > 2048 {
                frame_metadata.pop_first();
            }
            let encoded = active_encoder.encode_with(
                delivery
                    .as_ref()
                    .map_or(&frame.texture, |d| &d.frame.texture),
                timestamp,
                plan.control,
                &encode_cancel,
            );
            metrics.encode.record(encode_started.elapsed());
            let encoded = encoded?;
            for output in &encoded {
                if let Some(actual) = crate::media::video_format::parse_stream_format(
                    output.format.codec.media(),
                    &output.data,
                ) {
                    anyhow::ensure!(
                        actual.chroma_format_idc == output.format.chroma
                            && actual.bit_depth_luma == output.format.depth
                            && actual.bit_depth_chroma == output.format.depth
                            && (actual.visible_width, actual.visible_height) == size,
                        "编码器实际输出偏离协商格式"
                    );
                }
            }
            Ok((encoded, plan, Instant::now()))
        })();
        metrics.work.record(iteration_started.elapsed());
        if cancel.is_cancelled() || !handle.requested() {
            break;
        }
        let (encoded, plan, encode_finished) = match encoded {
            Ok(encoded) => {
                encode_errors = 0;
                encoded
            }
            Err(error) => {
                if shared_keyframe {
                    // The members still wait for that keyframe.
                    keyframe.store(true, Ordering::Release);
                }
                if unsafe { source_device.GetDeviceRemovedReason() }.is_err() {
                    tracing::warn!(%error,"capture graphics device was removed; recreating selected source");
                    encoder = None;
                    encoder_info = None;
                    current = None;
                    cached = None;
                    desktop = None;
                    transfer = None;
                    encoding_device = None;
                    last_frame = None;
                    publication.send_if_modified(|state| {
                        if state.visible {
                            state.visible = false;
                            true
                        } else {
                            false
                        }
                    });
                    keyframe.store(true, Ordering::Release);
                    continue;
                }
                encode_errors += 1;
                tracing::warn!(%error,hardware,consecutive=encode_errors,"host encoding failed");
                keyframe.store(true, Ordering::Release);
                next = Instant::now() + Duration::from_secs_f64(1.0 / f64::from(wanted.fps));
                if encode_errors >= 10
                    || error.downcast_ref::<encoder::SwitchCandidate>().is_some()
                    || unsafe { device.GetDeviceRemovedReason() }.is_err()
                {
                    // Disable the failed candidate. Do not endlessly
                    // reopen that same encoder after its consecutive failures.
                    failed.insert(candidate_id);
                    encoder = None;
                    encoder_info = None;
                    current = None;
                    cached = None;
                    last_frame = None;
                }
                continue;
            }
        };
        metrics.encoded_frames += encoded.len() as u64;
        let encoded_at = Instant::now();
        encoded_recently.extend(encoded.iter().map(|frame| (encoded_at, frame.data.len())));
        if plan.control.keyframe && !encoded.iter().any(|frame| frame.keyframe) {
            keyframe.store(true, Ordering::Release);
        }
        last_frame = Some(frame.captured);
        if !encoded.is_empty() {
            let diagnostics = super::super::ActiveEncoding {
                preferred_gpu: negotiated.preference().0,
                preferred_codec: negotiated.preference().1,
                selection: negotiated.selection_reason(&candidate.capability, !failed.is_empty()),
                backend: candidate.capability.backend,
                adapter: candidate.capability.adapter,
                format: wanted.format,
                size,
                fps: wanted.fps,
                target_bps: media_rate,
            };
            if last_diagnostics != Some(diagnostics) {
                handle.video(Some(diagnostics));
                last_diagnostics = Some(diagnostics);
            }
            let mut active = lock(&config);
            if active.revision == wanted.revision {
                active.reported_quality = wanted.quality;
            }
            drop(active);
            let capture_name = desktop.as_ref().map_or("DXGI", |d| d.backend_name()).to_owned();
            let state = Published {
                screen: screen.clone(),
                capturing: true,
                visible: true,
                quality: wanted.quality,
                fps: wanted.fps,
                format: wanted.format,
                budget_size,
                encoder: Some(candidate.capability.backend),
                capture: capture_name.clone(),
            };
            publication.send_if_modified(|old| {
                if *old != state {
                    *old = state;
                    true
                } else {
                    false
                }
            });
            if let Some(membership) = membership.as_ref()
                && let Some((implementation, maximum_size)) = encoder_info
            {
                membership.set_output(encode_group::Output {
                    implementation,
                    maximum_size,
                    capture: capture_name,
                    screen: screen.clone(),
                    quality: wanted.quality,
                    fps: wanted.fps,
                    format: wanted.format,
                    budget_size,
                    backend: candidate.capability.backend,
                    video: diagnostics,
                });
            }
        }
        let followers = membership.as_ref().filter(|m| m.has_others());
        for mut frame in encoded {
            if let Some((captured, input_started, is_new, metadata)) =
                frame_metadata.remove(&frame.timestamp_100ns)
            {
                frame.is_new = is_new;
                frame.color.hdr_metadata = if frame.color.transfer == 16 {
                    metadata
                } else {
                    None
                };
                frame.timing = Some(encoder::FrameTiming {
                    captured,
                    encode_started: input_started,
                    encode_finished,
                });
            }
            let mut meta = frame_ids.next(frame.keyframe);
            if decider.encoded(&plan, &mut meta, frame.keyframe, frame.ltr, Instant::now()) {
                tracing::warn!(target: "farwing::recovery",
                    "encoder did not code the recovery frame; sending a keyframe");
                keyframe.store(true, Ordering::Release);
            }
            if meta.recovery() {
                tracing::debug!(target: "farwing::recovery", frame = meta.frame_id,
                    reference = meta.ref_id, bytes = frame.data.len(), "recovery frame encoded");
            }
            frame.meta = Some(meta);
            // No B frames: older metadata cannot belong to a later output.
            while frame_metadata
                .first_key_value()
                .is_some_and(|(&timestamp, _)| timestamp < frame.timestamp_100ns)
            {
                frame_metadata.pop_first();
            }
            if let Some(membership) = followers {
                membership.publish(encode_group::Shared::new(&frame, started));
            }
            if own_active {
                sender.blocking_send(frame).context("画面发送队列已关闭")?;
            }
        }
    }
    Ok(())
}
