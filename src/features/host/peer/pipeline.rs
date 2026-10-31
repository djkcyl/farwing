//! The encoder pipeline of one stream ([`super::encode_group`]): a thread
//! that captures a source (a screen), encodes it at its members' allowance
//! and hands every frame to them. Driver objects stay on this thread; it
//! ends when its last member leaves, or reports why it cannot go on.
use super::super::{capture, encoder, output_size};
use crate::platform::source::{self, FrameSource};
use super::encode_group::{End, Group, Output};
use crate::features::host::format::{Backend, Color, QualityTarget, Rate};
use crate::features::host::parameters::{self, MIN_ENCODER_RATE};
use crate::i18n::t;
use anyhow::{Context as _, Result};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

mod metrics;

/// How long one slow member among several holds the stream back.
const PATIENCE: Duration = Duration::from_millis(100);

/// How a pipeline starts: the source and the GPU scheduling of the member
/// that opened it.
#[derive(Clone)]
pub(super) struct Start {
    pub source: capture::Source,
    pub gpu_compatibility: bool,
}

#[cfg(test)]
impl Start {
    pub(super) fn test() -> Self {
        Self {
            source: capture::Source::Screen(capture::Screen {
                id: 0,
                device_name: String::new(),
                display_name: String::new(),
                width: 1920,
                height: 1080,
                left: 0,
                top: 0,
                primary: true,
                fps: 60,
                dpi_scale: None,
                hdr: false,
                adapter: 0,
                render_adapter: None,
                identity: None,
            }),
            gpu_compatibility: false,
        }
    }
}

/// Run `group`'s pipeline on its own thread.
pub(super) fn spawn(group: Arc<Group>, start: Start) {
    let running = group.clone();
    let spawned = std::thread::Builder::new()
        .name("host-encode".into())
        .spawn(move || {
            let end = match run(&running, start) {
                Ok(None) => return,
                Ok(Some(end)) => end,
                Err(error) if error.is::<capture::SourceGone>() => End::SourceGone,
                Err(error) => End::Failed(format!("{error:#}")),
            };
            if !running.stopped() {
                tracing::warn!(source = %running.key().source, ?end, "encoder pipeline ended");
            }
            running.end(end);
        });
    if let Err(error) = spawned {
        group.end(End::Failed(error.to_string()));
    }
}

/// The device the last encoder used: when the pipeline ends it keeps the
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

/// `Ok(None)`: stopped (no members left); `Ok(Some(_))`: why the members
/// must go elsewhere.
fn run(group: &Group, start: Start) -> Result<Option<End>> {
    // Declared first, dropped last: after the encoder and transfer that use
    // the device.
    let mut reconnect = Reconnect(None);
    let _activity = crate::platform::windows::host_service::activity::Work::new();
    // Source, encoder and format stay; the rest may be retuned.
    let initial = group.key();
    let stop = group.stop_flag();
    let format = initial.format;
    let hardware = initial.backend != Backend::Software;
    let mut runtime = encoder::Runtime::new()?;
    let mut described = start.source;
    let mut frames = None::<Box<dyn FrameSource>>;
    let mut encoder = None::<encoder::Encoder>;
    // `frame-meta` numbering and the long-term reference decisions.
    let mut frame_ids = crate::media::frame_meta::Counter::default();
    let mut decider = crate::features::host::recovery::Decider::new(0);
    let mut current = None::<((u32, u32), Color)>;
    let mut encoder_info = None::<(i32, (u32, u32))>;
    let mut next = Instant::now();
    let mut paced_fps = 0;
    let mut cached = None::<capture::Frame>;
    let mut last_frame = None::<Instant>;
    let mut encode_errors = 0u32;
    let mut admission = parameters::WindowAdmission::default();
    let mut generation = 0;
    let mut encoding_device = None::<(u64, windows::Win32::Graphics::Direct3D11::ID3D11Device)>;
    let mut transfer = None::<crate::features::host::transfer::Transfer>;
    let mut frame_metadata = std::collections::BTreeMap::new();
    let mut metrics = metrics::Metrics::default();
    let mut paused_capture_at = None::<Instant>;
    // A new duplication can first report only the pointer; until its first
    // image, wait for the next present rather than the next frame time.
    let mut quick_retries = 0u8;
    // The cached image was captured before anyone took frames and is still
    // unsent: the first frame, not a repeat.
    let mut unsent_cache = false;
    let mut keyframe = true;
    // The pipeline's own clock; members get frames on theirs.
    let clock = Instant::now();

    while !group.stopped() {
        let iteration_started = Instant::now();
        let key = group.key();
        let tier = key.target.tier();
        // What the members can take: the weakest active one decides.
        let vote = group.aggregate();
        let wanted = vote.is_some();
        let peak = vote.map_or(0, |v| v.peak);
        let ratio = vote.map_or(0.0, |v| v.ratio);
        let fps = vote
            .map(|v| v.fps)
            .filter(|fps| *fps > 0)
            .map_or(key.fps, |fps| fps.min(key.fps))
            .max(1);
        metrics.report(fps, described.adapter(), key.adapter);
        let (source_width, source_height) = described.size();
        let budget_size = crate::media::geometry::fit_size(source_width, source_height, key.maximum);
        let bounds = parameters::fixed(format, key.target, budget_size, fps);
        let quality_target = QualityTarget {
            bitrate: bounds.maximum,
            fps,
        };
        let period = Duration::from_secs_f64(1.0 / f64::from(fps));
        if paced_fps != fps {
            paced_fps = fps;
            next = Instant::now();
        }

        if frames.is_none() {
            // Prepared while the members' connections are still being made;
            // nothing is encoded or sent before one of them takes frames.
            let opened_at = Instant::now();
            runtime.begin_gpu_work(start.gpu_compatibility);
            let opened = source::open(&described)?;
            generation = opened.generation();
            frames = Some(opened);
            quick_retries = 3;
            tracing::info!(
                elapsed_ms = opened_at.elapsed().as_millis(),
                "host capture preparation completed"
            );
            continue;
        }
        // Nobody takes frames yet: open the encoder from the first image,
        // so the first frame only has to be encoded.
        let preroll = !wanted && encoder.is_none();
        if !wanted && !preroll {
            if !unsent_cache {
                cached = None;
            }
            last_frame = None;
            keyframe = true;
            next = Instant::now();
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        if Instant::now() < next {
            std::thread::sleep(
                next.saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(20)),
            );
            continue;
        }
        if wanted && group.backpressure(Instant::now(), PATIENCE) {
            metrics.backpressure_waits += 1;
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        if peak < MIN_ENCODER_RATE && !preroll {
            // Keep source/cursor observations alive during a transport
            // pause, without preprocessing the desktop at the full video
            // rate. Do not advance WindowAdmission: it counts actual
            // candidate frames.
            if paused_capture_at.is_some_and(|at| at.elapsed() < Duration::from_millis(100)) {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            paused_capture_at = Some(Instant::now());
        } else {
            paused_capture_at = None;
        }
        let hdr = key.hdr
            && format.depth == 10
            && frames.as_ref().is_some_and(|f| f.hdr_available());
        let wait_ms = ((1000.0 / f64::from(fps)).round() as u32).max(1);
        let capture_started = Instant::now();
        // Advance the planned deadline, not the (usually late) wake-up time.
        // Otherwise sleep/dispatch overhead is added to every frame period.
        // A whole missed period resets the clock instead of replaying a backlog.
        next = if capture_started.saturating_duration_since(next) >= period {
            capture_started + period
        } else {
            next + period
        };
        let captured = frames
            .as_mut()
            .context(t().features.peer_capture_source_missing)?
            .next(wait_ms, tier, key.cursor, hdr, key.maximum);
        metrics.capture.record(capture_started.elapsed());
        let captured = captured?;
        let capture_state = frames
            .as_ref()
            .context(t().features.peer_capture_source_missing)?;
        group.set_cursor(capture_state.cursor());
        if generation != capture_state.generation() {
            generation = capture_state.generation();
            encoder = None;
            encoder_info = None;
            cached = None;
            current = None;
            last_frame = None;
            transfer = None;
            keyframe = true;
        }
        described = capture_state.descriptor();
        if hdr != (key.hdr && capture_state.hdr_available() && format.depth == 10) {
            // Capture recovery or source refresh changed the effective
            // format: rebuild preprocessing and encoder before this frame.
            cached = None;
            continue;
        }
        if !capture_state.available() {
            cached = None;
            group.set_visible(false);
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
                if last_frame
                    .is_some_and(|at| at.elapsed() < Duration::from_secs_f64(1.0 / f64::from(fps.min(30))))
                {
                    continue;
                }
                frame.captured = Instant::now();
                frame.is_new = std::mem::take(&mut unsent_cache);
                frame
            }
        };
        // A zero/tiny allowance is a transport pause, not an encoder
        // failure. Do not fabricate bandwidth by clamping it to a floor.
        if peak < MIN_ENCODER_RATE && !preroll {
            continue;
        }
        let (media_rate, discard_frame) = if preroll {
            (peak, false)
        } else {
            admission.next(peak, MIN_ENCODER_RATE, ratio)
        };
        if discard_frame {
            metrics.admission_drops += 1;
            continue;
        }
        let size = output_size(frame.width, frame.height, tier);
        let color = if hdr { Color::Hdr } else { Color::Sdr };
        let source_device = frames
            .as_ref()
            .context(t().features.peer_capture_source_missing)?
            .device();
        // NVENC owns a separate D3D11 context even on the capture adapter.
        // Transfer explicitly orders capture/preprocessing before encoding,
        // instead of relying on NVENC's handling of the capture context.
        let isolated_nvenc = hardware && key.backend == Backend::Nvidia;
        let device = if !hardware || (!isolated_nvenc && key.adapter == described.adapter()) {
            source_device.clone()
        } else {
            if encoding_device
                .as_ref()
                .is_none_or(|(adapter, _)| *adapter != key.adapter)
            {
                match capture::create_device(key.adapter) {
                    Ok((device, _)) => {
                        tracing::info!(
                            source_adapter = described.adapter(),
                            encoder_adapter = key.adapter,
                            isolated_nvenc,
                            "host encoding device created independently of capture"
                        );
                        encoding_device = Some((key.adapter, device));
                    }
                    Err(error) => {
                        tracing::warn!(%error, ?key, "encoder adapter failed");
                        return Ok(Some(End::Candidate));
                    }
                }
            }
            encoding_device.as_ref().unwrap().1.clone()
        };
        let initial = Rate {
            target: bounds.initial.max(300_000),
            peak: bounds.initial.max(300_000),
            fps,
            tier,
            quality_target,
            smooth: key.cinema,
        };
        if current.is_some_and(|(old_size, old_color)| old_size != size || old_color != color)
            && let Some(active) = encoder.as_mut()
        {
            match active.reconfigure_stream(&device, size, format, initial, color) {
                Ok(true) => {
                    encoder_info = Some((active.implementation(), active.maximum_size()));
                    current = Some((size, color));
                    frame_metadata.clear();
                    frame_ids.restart();
                    decider.reset(active.ltr_slots());
                    encode_errors = 0;
                    keyframe = true;
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%error, ?size, "host encoder stream reconfiguration rejected; rebuilding")
                }
            }
        }
        if current.is_none_or(|(old_size, old_color)| old_size != size || old_color != color) {
            // Its members may have left while the source opened.
            if group.stopped() {
                break;
            }
            let rebuilding = Instant::now();
            // Release before replacement to avoid consuming a second driver
            // session solely for a size transition.
            drop(encoder.take());
            let close_ms = rebuilding.elapsed().as_secs_f64() * 1000.;
            let created = if hardware {
                encoder::Encoder::hardware_format(&device, size, format, initial, color)
            } else {
                encoder::Encoder::software_format(&device, size, format, initial)
            };
            match created {
                Ok(created) => {
                    tracing::info!(
                        ?size,
                        ?key,
                        close_ms,
                        elapsed_ms = rebuilding.elapsed().as_secs_f64() * 1000.,
                        "host encoder replacement completed"
                    );
                    encoder_info = Some((created.implementation(), created.maximum_size()));
                    frame_ids.restart();
                    decider.reset(created.ltr_slots());
                    reconnect.0 = Some((key.adapter, device.clone()));
                    encoder = Some(created);
                }
                Err(error) => {
                    tracing::warn!(%error, ?key, "host encoder initialization failed; disabling this candidate");
                    return Ok(Some(End::Candidate));
                }
            }
            current = Some((size, color));
            group.set_size(size);
            frame_metadata.clear();
            encode_errors = 0;
            keyframe = true;
        }
        if preroll {
            unsent_cache = true;
            tracing::debug!(?size, "host encoder prepared while connecting");
            continue;
        }
        let active_encoder = encoder.as_mut().context(t().features.peer_encoder_missing)?;
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
                .copy(&frame, || !stop.load(Ordering::Acquire))
        })();
        if needs_transfer {
            metrics.transfer.record(transfer_started.elapsed());
        }
        if needs_transfer && prepared.as_ref().is_ok_and(|frame| frame.is_none()) {
            metrics.transfer_busy += 1;
            continue;
        }
        // Any member's keyframe need (a newcomer, PLI/FIR, lost frames).
        let shared_keyframe = group.take_keyframe(Instant::now());
        let encoded = (|| -> Result<_> {
            // Delivery failures share the source-loss/candidate-recovery exits.
            let delivery = prepared?;
            let rate_started = Instant::now();
            let reconfigured = active_encoder.configure_rate(Rate {
                target: media_rate,
                peak,
                fps,
                tier,
                quality_target,
                smooth: key.cinema,
            });
            metrics.rate.record(rate_started.elapsed());
            if reconfigured? {
                keyframe = true;
            }
            let own_keyframe = std::mem::take(&mut keyframe);
            let (held, requests, rtt) = group.take_feedback();
            let plan = decider.plan(
                Instant::now(),
                own_keyframe | shared_keyframe,
                frame_ids.epoch(),
                &held,
                &requests,
                rtt.unwrap_or(Duration::from_millis(100)),
            );
            if !requests.is_empty() {
                tracing::debug!(target: "farwing::recovery", ?requests, control = ?plan.control,
                    "recovery requested");
            }
            if plan.control.keyframe && shared_keyframe && group.shared() {
                tracing::info!("shared encoder encodes a keyframe for its members");
            }
            let encode_started = Instant::now();
            let timestamp = (frame.captured.duration_since(clock).as_nanos() / 100) as i64;
            frame_metadata.insert(
                timestamp,
                (frame.captured, encode_started, frame.is_new, frame.hdr_metadata),
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
                &stop,
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
                        "{}",
                        t().features.peer_encoder_output_mismatch
                    );
                }
            }
            Ok((encoded, plan, Instant::now()))
        })();
        metrics.work.record(iteration_started.elapsed());
        if group.stopped() {
            break;
        }
        let (encoded, plan, encode_finished) = match encoded {
            Ok(encoded) => {
                encode_errors = 0;
                encoded
            }
            Err(error) => {
                // The members still wait for a keyframe.
                keyframe = true;
                if unsafe { source_device.GetDeviceRemovedReason() }.is_err() {
                    tracing::warn!(%error, "capture graphics device was removed; recreating the source");
                    encoder = None;
                    encoder_info = None;
                    current = None;
                    cached = None;
                    frames = None;
                    transfer = None;
                    encoding_device = None;
                    last_frame = None;
                    group.set_visible(false);
                    continue;
                }
                encode_errors += 1;
                tracing::warn!(%error, hardware, consecutive = encode_errors, "host encoding failed");
                next = Instant::now() + period;
                if encode_errors >= 10
                    || error.downcast_ref::<encoder::SwitchCandidate>().is_some()
                    || unsafe { device.GetDeviceRemovedReason() }.is_err()
                {
                    // Do not endlessly reopen an encoder after its
                    // consecutive failures: the members pick another.
                    return Ok(Some(End::Candidate));
                }
                continue;
            }
        };
        metrics.encoded_frames += encoded.len() as u64;
        if plan.control.keyframe && !encoded.iter().any(|frame| frame.keyframe) {
            keyframe = true;
        }
        last_frame = Some(frame.captured);
        if !encoded.is_empty()
            && let Some((implementation, maximum_size)) = encoder_info
        {
            let capture = frames.as_ref().map_or("DXGI", |f| f.backend_name()).to_owned();
            group.set_output(Output {
                implementation,
                maximum_size,
                capture,
                source: described.clone(),
                target: key.target,
                fps,
                format,
                budget_size,
                backend: key.backend,
                visible: true,
                video: crate::features::host::ActiveEncoding {
                    preferred_gpu: None,
                    preferred_codec: None,
                    selection: Default::default(),
                    backend: key.backend,
                    adapter: key.adapter,
                    format,
                    size,
                    fps,
                    target_bps: media_rate,
                },
            });
        }
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
                keyframe = true;
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
            group.publish(&frame, clock);
        }
    }
    Ok(None)
}
