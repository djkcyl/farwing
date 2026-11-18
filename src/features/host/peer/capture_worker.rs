//! Capture/encoder worker lifecycle; owns driver objects on one thread.
use super::super::Lease;
use super::super::VideoConfig;
use super::super::capture;
use super::super::encoder;
use super::super::lock;
use super::super::output_size;
use super::Published;
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
mod pipeline;

pub(super) fn capture_loop(
    mut screen: capture::Screen,
    handle: Lease,
    cancel: CancellationToken,
    encode_cancel: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    business_ready: Arc<AtomicBool>,
    config: Arc<Mutex<VideoConfig>>,
    keyframe: Arc<AtomicBool>,
    sender: mpsc::Sender<encoder::Encoded>,
    started: Instant,
    transport: crate::features::host::transport::Transport,
    negotiated: Arc<crate::features::host::format::Negotiated>,
    publication: tokio::sync::watch::Sender<Published>,
    pointer: Arc<Mutex<Option<crate::platform::windows::cursor_shape::Snapshot>>>,
) -> Result<()> {
    let _activity = crate::platform::windows::host_service::activity::Work::new();
    let mut runtime = encoder::Runtime::new()?;
    let mut desktop = None;
    let mut encoder = None::<encoder::Encoder>;
    let mut current: Option<((u32, u32), ((u64, Backend, Format), Color))> = None;
    let mut cached = None::<capture::Frame>;
    let mut last_frame = None::<Instant>;
    let mut failed = std::collections::HashSet::new();
    let mut encode_errors = 0u32;
    let mut requested = None;
    let mut automatic = None::<crate::features::host::parameters::AutoQuality>;
    let mut initial_auto = true;
    let mut admission = crate::features::host::parameters::WindowAdmission::default();
    let mut generation = 0;
    let mut encoding_device = None::<(u64, windows::Win32::Graphics::Direct3D11::ID3D11Device)>;
    let mut transfer = None::<crate::features::host::transfer::Transfer>;
    let mut frame_metadata = std::collections::BTreeMap::new();
    let mut last_diagnostics = None;
    let mut metrics = metrics::Metrics::default();
    let mut paused_capture_at = None::<Instant>;
    // 采集线程注册 MMCSS，避免被同机编码/后台任务抢占；失败不影响采集。
    #[cfg(windows)]
    let _capture_priority = match crate::platform::windows::mmcss::CapturePriority::enter() {
        Ok(priority) => Some(priority),
        Err(error) => {
            tracing::warn!(%error, "host capture MMCSS registration failed");
            None
        }
    };
    let mut pipeline = None::<pipeline::Pipeline>;
    let mut pipeline_key = None::<((u32, u32), ((u64, Backend, Format), Color))>;
    // 共享纹理环在这台机器上建过一次没成就别再试：退回串行编码，
    // 而不是让采集每帧都重建一次失败。
    let mut pipeline_unavailable = false;
    // 队列满丢的是已经编好的帧，主控看不到 RTP 丢包；等队列腾出位置再补一次
    // 关键帧把参考关系接回去。
    let mut send_resync = false;
    // 目标帧率低于刷新率时的下一个放行时刻，按周期累加而不是按上一帧加固定
    // 时长，否则 144Hz 屏上的 60fps 会被卡成 48fps。
    let mut next_frame_at = None::<Instant>;

    while !cancel.is_cancelled() && handle.requested() {
        let iteration_started = Instant::now();
        let mut wanted = *lock(&config);
        if !negotiated.permits_format(wanted.format) {
            let previous = wanted.revision;
            let chroma = wanted.format.chroma;
            let hdr = wanted.color.is_hdr();
            negotiated.apply(
                &mut wanted,
                None,
                chroma,
                hdr,
                (screen.width, screen.height),
            )?;
            let mut active = lock(&config);
            if active.revision != previous {
                continue;
            }
            wanted.revision = previous.wrapping_add(1);
            *active = wanted;
        }

        let requested_hdr = wanted.color.is_hdr();
        if desktop.is_none() && transport.media_ready() && wanted.sending && wanted.capturing {
            // The authorized track is already negotiated. Prepare its native
            // capture while ICE/DTLS is still connecting; the gate below still
            // forbids frame acquisition/encoding/sending until transport is up.
            // Nothing is kept warm outside this track's lease/cancellation scope.
            let opened_at = Instant::now();
            let connecting = !connected.load(Ordering::Acquire);
            runtime.begin_gpu_work(negotiated.gpu_compatibility);
            desktop = Some(capture::Desktop::open_selected(&screen)?);
            generation = desktop.as_ref().unwrap().generation;
            tracing::info!(
                elapsed_ms = opened_at.elapsed().as_millis(),
                connecting,
                "host capture preparation completed"
            );
            // Native setup may overlap incoming CaptureSetting changes. Read
            // the current revision before configuring or encoding any frame.
            continue;
        }
        if let Some(desktop) = desktop.as_ref() {
            // T CCDCA0/CF3450 gate HDR on the actual source, not only the
            // requested setting. Keep the request so same-source HDR recovery
            // can re-enable the negotiated format without rewriting user intent.
            negotiated.apply_source(
                &mut wanted,
                (screen.width, screen.height),
                desktop.hdr_available(),
            )?;
        }
        let settings = (
            wanted.revision,
            wanted.quality,
            wanted.auto_quality,
            wanted.bitrate,
            wanted.fps,
        );
        let changed = requested != Some(settings);
        if changed {
            automatic = (wanted.quality == 5).then(|| {
                crate::features::host::parameters::AutoQuality::new(
                    wanted.auto_quality,
                    wanted.maximum_quality,
                )
            });
            initial_auto = true;
            requested = Some(settings);
        }
        let candidate = negotiated
            .candidate(
                &wanted,
                (screen.width, screen.height),
                screen.render_adapter.unwrap_or(screen.adapter),
                &failed,
            )
            .context("本会话的协商编码候选已全部失败")?;
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
        if let Some(active) = pipeline.as_mut() {
            let stats = active.stats();
            metrics.absorb(stats);
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
        transport.fps(wanted.fps);
        let bounds = if automatic.is_some() {
            crate::features::host::parameters::automatic(
                wanted.format,
                wanted.quality,
                budget_size,
                wanted.fps,
                initial_auto,
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
        let quality_target = crate::features::host::format::QualityTarget {
            bitrate: bounds.maximum,
            fps: wanted.fps,
        };
        if wanted.sending && wanted.capturing {
            transport.quality(automatic.is_some(), wanted.quality, changed);
            transport.configure(bounds);
        } else {
            transport.pause();
        }
        if !connected.load(Ordering::Acquire)
            || !business_ready.load(Ordering::Acquire)
            || !transport.media_ready()
            || !wanted.sending
            || !wanted.capturing
        {
            if !wanted.capturing {
                if last_diagnostics.take().is_some() {
                    handle.video(None);
                }
                desktop = None;
                encoder = None;
                pipeline = None;
                pipeline_key = None;
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
            cached = None;
            if !wanted.capturing {
                runtime.end_gpu_work();
            }
            last_frame = None;
            keyframe.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        if desktop.is_none() {
            runtime.begin_gpu_work(negotiated.gpu_compatibility);
            desktop = Some(capture::Desktop::open_selected(&screen)?);
            generation = desktop.as_ref().unwrap().generation;
            // Readiness can change after the preparation gate above. Apply the
            // same policy and reread settings after either native open path.
            continue;
        }
        if transport.media_rate().min(bounds.maximum)
            < crate::features::host::parameters::MIN_ENCODER_RATE
        {
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
        let source_device = desktop.as_ref().context("缺少采集源")?.device.clone();
        // NVENC 即使和采集同卡也用独立的 D3D11 上下文：采集/预处理在采集上下文
        // 上按 fence 交付，不依赖 NVENC 自己怎么处理采集上下文。
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
        // 共享环和编码线程只依赖采集源和候选，跟这一帧的内容无关，所以在第一帧
        // 之前就建好：先抓帧再建会把第一个 present 直接吞掉，静态桌面上那意味着
        // 主控一直收不到画面。
        // 建的时候码率估计可能还停在暂停值，编码器会话不接受太小的目标码率，
        // 用一个能建起来的下限起步；真正的码率每帧由 submit 时的 rate 重配。
        let initial_peak = transport
            .media_rate()
            .min(bounds.maximum)
            .max(crate::features::host::parameters::MIN_ENCODER_RATE);
        let pipeline_target = desktop
            .as_ref()
            .and_then(|desktop| desktop.raw_desc())
            .filter(|_| {
                cfg!(windows)
                    && !pipeline_unavailable
                    && hardware
                    && candidate.capability.backend
                        == crate::features::host::format::Backend::Nvidia
                    && candidate.capability.adapter == screen.adapter
                    && !wanted.cursor_capture
            })
            .map(|_| {
                // 缩放/上限由预处理做，编码尺寸必须和它算出来的完全一致，不能只按
                // 画质档位算：档位尺寸被协商上限压小时两者会差一截。
                let (width, height) = output_size(screen.width, screen.height, wanted.quality);
                (
                    crate::media::geometry::fit_size(width, height, wanted.maximum),
                    (candidate_id, wanted.color),
                )
            });
        if pipeline_target != pipeline_key {
            drop(pipeline.take());
            pipeline_key = None;
            if let Some(key) = pipeline_target {
                let rebuilding = Instant::now();
                let raw_desc = desktop
                    .as_ref()
                    .context("缺少采集源")?
                    .raw_desc()
                    .context("采集源不再提供DXGI原始纹理")?;
                let created = pipeline::Pipeline::new(
                    &source_device,
                    device.clone(),
                    raw_desc,
                    key.0,
                    wanted.format,
                    crate::features::host::format::Rate {
                        target: initial_peak,
                        peak: initial_peak,
                        fps: wanted.fps,
                        quality: wanted.quality,
                        quality_target,
                    },
                    wanted.color,
                    sender.clone(),
                    keyframe.clone(),
                    encode_cancel.clone(),
                    started,
                );
                match created {
                    Ok(created) => {
                        tracing::info!(
                            size = ?key.0,
                            ?candidate_id,
                            elapsed_ms = rebuilding.elapsed().as_secs_f64() * 1000.,
                            "host encode pipeline replacement completed"
                        );
                        handle.encoder(
                            created.implementation(),
                            created.maximum_size(),
                            &screen,
                            desktop.as_ref().context("缺少采集源")?.backend_name(),
                        );
                        pipeline = Some(created);
                        pipeline_key = Some(key);
                        frame_metadata.clear();
                        encode_errors = 0;
                        keyframe.store(true, Ordering::Release);
                    }
                    Err(error) => {
                        tracing::warn!(%error,?candidate_id,"host encode pipeline unavailable; falling back to the serial path");
                        pipeline_unavailable = true;
                        encoder = None;
                        current = None;
                    }
                }
            }
        }
        let wait_ms = ((1000.0 / f64::from(wanted.fps)).round() as u32).max(1);
        let capture_started = Instant::now();
        // AcquireNextFrame already blocks until the next desktop present (or
        // this timeout). Sleeping to a separate deadline first makes the loop
        // wake just after that present and wait a whole extra refresh.
        if pipeline.is_some() {
            let stream = desktop.as_mut().context("缺少采集源")?;
            let generation_before = stream.generation;
            let raw = pipeline.as_mut().expect("raw pipeline exists").next_raw(
                stream,
                // 目标 60Hz 时 17ms 超时会让偶发的 present 抖动直接变成
                // 33ms 的空档；让 Acquire 多等一会，帧一到就返回。
                wait_ms.max(50),
                wanted.quality,
                wanted.color.is_hdr(),
                wanted.maximum,
            );
            metrics.capture.record(capture_started.elapsed());
            // 采集源被换掉（分辨率/后端/设备变了）时共享环作废：重建流水线，
            // 别把旧设备的纹理环继续往里灌。
            if desktop.as_ref().map(|d| d.generation) != Some(generation_before) {
                pipeline = None;
                pipeline_key = None;
                encoder = None;
                current = None;
                transfer = None;
                encoding_device = None;
                keyframe.store(true, Ordering::Release);
                continue;
            }
            // 画面不在采集线程合成光标，指针通道必须继续喂；否则内嵌光标关掉
            // 时主控的光标位置会停住。
            let sampled = desktop.as_ref().context("缺少采集源")?.cursor.clone();
            *lock(&pointer) = sampled;
            // 采集源自己刷新过 screen（分辨率/方向变了）时，下一轮的编码尺寸要
            // 跟着重算，否则流水线会拿旧尺寸的编码器去收新尺寸的画面。
            screen = desktop.as_ref().context("缺少采集源")?.screen.clone();
            let Some(raw) = raw? else {
                continue;
            };
            metrics.new_frames += 1;
            let size = pipeline_key.context("流水线缺少编码尺寸")?.0;
            // 目标帧率低于刷新率时按截止时间丢帧，截止时间每次只推进一个周期：
            // 144Hz 屏上 60fps 才会 2/3 帧交替取（60fps），而不是每帧都等到
            // 下一个 ≥0.9 周期（48fps）。
            if wanted.fps < screen.fps {
                let period = Duration::from_secs_f64(1.0 / f64::from(wanted.fps));
                if next_frame_at.is_some_and(|at| raw.captured < at) {
                    continue;
                }
                // 落后超过一整个周期说明中间断过（卡顿/丢帧），重新对齐，
                // 否则会把攒下的额度一次性放完。
                next_frame_at = Some(match next_frame_at {
                    Some(at) if at + period > raw.captured => at + period,
                    _ => raw.captured + period,
                });
            }
            let peak = transport.media_rate().min(bounds.maximum);
            let codec_minimum = crate::features::host::parameters::MIN_ENCODER_RATE;
            if peak < codec_minimum {
                continue;
            }
            let (media_rate, discard_frame) =
                admission.next(peak, codec_minimum, transport.cwnd_ratio());
            if discard_frame {
                metrics.admission_drops += 1;
                continue;
            }
            let rate = crate::features::host::format::Rate {
                target: media_rate,
                peak,
                fps: wanted.fps,
                quality: wanted.quality,
                quality_target,
            };
            let captured_at = raw.captured;
            let submit_started = Instant::now();
            let accepted = pipeline
                .as_mut()
                .expect("raw pipeline exists")
                .submit_raw(raw, rate);
            metrics.transfer.record(submit_started.elapsed());
            metrics.work.record(iteration_started.elapsed());
            match accepted {
                Ok(true) => {
                    last_frame = Some(captured_at);
                    let diagnostics = super::super::ActiveEncoding {
                        preferred_gpu: negotiated.preference().0,
                        preferred_codec: negotiated.preference().1,
                        selection: negotiated
                            .selection_reason(&candidate.capability, !failed.is_empty()),
                        backend: candidate.capability.backend,
                        adapter: candidate.capability.adapter,
                        format: wanted.format,
                        size,
                        fps: wanted.fps,
                        target_bps: media_rate,
                    };
                    if last_diagnostics.as_ref() != Some(&diagnostics) {
                        handle.video(Some(diagnostics.clone()));
                        last_diagnostics = Some(diagnostics);
                    }
                    let mut active = lock(&config);
                    if active.revision == wanted.revision {
                        active.reported_quality = wanted.quality;
                    }
                    let state = Published {
                        screen: screen.clone(),
                        capturing: true,
                        visible: true,
                        quality: wanted.quality,
                        fps: wanted.fps,
                        format: wanted.format,
                        budget_size,
                        encoder: Some(candidate.capability.backend),
                        capture: desktop.as_ref().map_or("DXGI", |d| d.backend_name()).into(),
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
                Ok(false) => metrics.transfer_busy += 1,
                Err(error) => {
                    let detail = pipeline
                        .as_ref()
                        .and_then(|pipeline| pipeline.error())
                        .unwrap_or_default();
                    tracing::warn!(%error, %detail, ?candidate_id, "host encode pipeline submit failed; rebuilding");
                    failed.insert(candidate_id);
                    pipeline = None;
                    pipeline_key = None;
                    encoder = None;
                    current = None;
                    keyframe.store(true, Ordering::Release);
                }
            }
            continue;
        }
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
        if generation != capture_state.generation {
            generation = capture_state.generation;
            encoder = None;
            pipeline = None;
            pipeline_key = None;
            cached = None;
            current = None;
            last_frame = None;
            next_frame_at = None;
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
                frame
            }
            None => {
                metrics.empty_captures += 1;
                let Some(mut frame) = cached.clone() else {
                    continue;
                };
                if last_frame.is_some_and(|at| {
                    at.elapsed() < Duration::from_secs_f64(1.0 / f64::from(wanted.fps.min(30)))
                }) {
                    continue;
                }
                frame.captured = Instant::now();
                frame.is_new = false;
                frame
            }
        };
        // AcquireNextFrame 按桌面刷新率交付，目标帧率更低时在这里按时间戳丢弃，
        // 不能靠抓帧前 sleep 来限速，否则会稳定错过 vblank。截止时间按周期累加，
        // 144Hz 屏上 60fps 才不会掉到 48fps。
        if wanted.fps < screen.fps {
            let period = Duration::from_secs_f64(1.0 / f64::from(wanted.fps));
            if next_frame_at.is_some_and(|at| frame.captured < at) {
                continue;
            }
            next_frame_at = Some(match next_frame_at {
                Some(at) if at + period > frame.captured => at + period,
                _ => frame.captured + period,
            });
        }
        let peak = transport.media_rate().min(bounds.maximum);
        // A zero/tiny allocation is a transport pause, not an encoder failure.
        // Do not fabricate bandwidth by clamping it back to a quality floor.
        let codec_minimum = crate::features::host::parameters::MIN_ENCODER_RATE;
        if peak < codec_minimum {
            continue;
        }
        let (media_rate, discard_frame) =
            admission.next(peak, codec_minimum, transport.cwnd_ratio());
        if discard_frame {
            metrics.admission_drops += 1;
            continue;
        }
        let size = output_size(frame.width, frame.height, wanted.quality);
        if current.is_some_and(|(old_size, old_candidate)| {
            (old_size != size || old_candidate != (candidate_id, wanted.color))
                && old_candidate.0.0 == candidate_id.0
                && old_candidate.0.1 == candidate_id.1
        }) && let Some(active) = encoder.as_mut()
        {
            let rate = crate::features::host::format::Rate {
                target: media_rate,
                peak,
                fps: wanted.fps,
                quality: wanted.quality,
                quality_target,
            };
            match active.reconfigure_stream(&device, size, wanted.format, rate, wanted.color) {
                Ok(true) => {
                    handle.encoder(
                        active.implementation(),
                        active.maximum_size(),
                        &screen,
                        desktop.as_ref().context("缺少采集源")?.backend_name(),
                    );
                    current = Some((size, (candidate_id, wanted.color)));
                    frame_metadata.clear();
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
            // session solely for a size/candidate transition (T C2D840).
            drop(encoder.take());
            let close_ms = rebuilding.elapsed().as_secs_f64() * 1000.;
            let created = if hardware {
                encoder::Encoder::hardware_format(
                    &device,
                    size,
                    wanted.format,
                    crate::features::host::format::Rate {
                        target: media_rate,
                        peak,
                        fps: wanted.fps,
                        quality: wanted.quality,
                        quality_target,
                    },
                    wanted.color,
                )
            } else {
                encoder::Encoder::software_format(
                    &device,
                    size,
                    wanted.format,
                    crate::features::host::format::Rate {
                        target: media_rate,
                        peak,
                        fps: wanted.fps,
                        quality: wanted.quality,
                        quality_target,
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
                    Some(created)
                }
                Err(error) => {
                    tracing::warn!(%error,?candidate_id,"host encoder initialization failed; disabling this candidate");
                    failed.insert(candidate_id);
                    encoder = None;
                    cached = None;
                    current = None;
                    continue;
                }
            };
            current = Some((size, (candidate_id, wanted.color)));
            frame_metadata.clear();
            encode_errors = 0;
            keyframe.store(true, Ordering::Release);
        }
        let active_encoder = encoder.as_mut().context("缺少编码器")?;
        let needs_transfer = device != source_device;
        let transfer_started = Instant::now();
        let prepared = (|| -> Result<Option<crate::features::host::transfer::Delivery>> {
            if !needs_transfer {
                return Ok(None);
            }
            if transfer
                .as_ref()
                .is_none_or(|t| !t.matches(&source_device, &device, &frame))
            {
                transfer = Some(crate::features::host::transfer::Transfer::new(
                    &source_device,
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
            });
            metrics.rate.record(rate_started.elapsed());
            if reconfigured? {
                keyframe.store(true, Ordering::Release);
            }
            // 队列满时丢掉的帧已经编好了，主控那边看不到 RTP 丢包、只会一直拿到
            // 引用不到的画面；等队列重新腾出位置再补一个关键帧接回去。
            if send_resync && sender.capacity() > 0 {
                send_resync = false;
                keyframe.store(true, Ordering::Release);
            }
            // Network requests are rate-limited at their RTCP entry. Explicit
            // quality/source controls and failed inputs must not wait 600ms.
            let force = keyframe.swap(false, Ordering::AcqRel);
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
            let encoded = active_encoder.encode_cancellable(
                delivery
                    .as_ref()
                    .map_or(&frame.texture, |d| &d.frame.texture),
                timestamp,
                force,
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
                    // An aligned-only AV1 encoder pads the picture and only
                    // declares its real size; a receiver that cannot crop
                    // would present the padding and misplace the pointer.
                    if output.format.codec == crate::features::host::format::Codec::Av1
                        && !negotiated.av1_visible_size
                        && (actual.coded_width, actual.coded_height) != size
                    {
                        return Err(encoder::SwitchCandidate(format!(
                            "对端不支持裁剪 AV1 对齐补边（编码 {}×{}，画面 {}×{}）",
                            actual.coded_width, actual.coded_height, size.0, size.1
                        ))
                        .into());
                    }
                }
            }
            Ok((encoded, force, Instant::now()))
        })();
        metrics.work.record(iteration_started.elapsed());
        if cancel.is_cancelled() || !handle.requested() {
            break;
        }
        let (encoded, force, encode_finished) = match encoded {
            Ok(encoded) => {
                encode_errors = 0;
                encoded
            }
            Err(error) => {
                if unsafe { source_device.GetDeviceRemovedReason() }.is_err() {
                    tracing::warn!(%error,"capture graphics device was removed; recreating selected source");
                    encoder = None;
                    pipeline = None;
                    pipeline_key = None;
                    current = None;
                    cached = None;
                    desktop = None;
                    transfer = None;
                    encoding_device = None;
                    last_frame = None;
                    next_frame_at = None;
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
                std::thread::sleep(Duration::from_secs_f64(1.0 / f64::from(wanted.fps)));
                if encode_errors >= 10
                    || error.downcast_ref::<encoder::SwitchCandidate>().is_some()
                    || unsafe { device.GetDeviceRemovedReason() }.is_err()
                {
                    // T C2C610: disable the failed candidate. Do not endlessly
                    // reopen that same encoder after its consecutive failures.
                    failed.insert(candidate_id);
                    encoder = None;
                    pipeline = None;
                    pipeline_key = None;
                    current = None;
                    cached = None;
                    last_frame = None;
                }
                continue;
            }
        };
        metrics.encoded_frames += encoded.len() as u64;
        if force && !encoded.iter().any(|frame| frame.keyframe) {
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
            let state = Published {
                screen: screen.clone(),
                capturing: true,
                visible: true,
                quality: wanted.quality,
                fps: wanted.fps,
                format: wanted.format,
                budget_size,
                encoder: Some(candidate.capability.backend),
                capture: desktop.as_ref().map_or("DXGI", |d| d.backend_name()).into(),
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
            // No B frames: older metadata cannot belong to a later output.
            while frame_metadata
                .first_key_value()
                .is_some_and(|(&timestamp, _)| timestamp < frame.timestamp_100ns)
            {
                frame_metadata.pop_first();
            }
            // 发送端过慢时丢这一帧，让采集/编码继续跑；关键帧等队列腾出位置
            // 再补（见 `send_resync`），队列满的时候再插 IDR 只会让码率越丢越高。
            match sender.try_send(frame) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(frame)) => {
                    metrics.send_drops += 1;
                    send_resync = true;
                    drop(frame);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    anyhow::bail!("画面发送队列已关闭");
                }
            }
        }
    }
    Ok(())
}
