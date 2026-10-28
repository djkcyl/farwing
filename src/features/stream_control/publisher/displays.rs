//! Display requests on the host. Host owners perform native operations;
//! message construction stays beside the wire contract.
use super::*;
use crate::features::host::lock;
use crate::features::host::peer::screens::{ScreenInfo, Screens};
use crate::features::stream_control::display_topology::PbDisplayResult;
use crate::features::stream_control::wire::{PbCaptureSettingRequest, SessionBody, setting_reason};
use std::collections::BTreeMap;

pub(crate) fn screen_states(
    screens: &[ScreenInfo],
    tracks: &BTreeMap<i32, usize>,
    current: i32,
) -> Vec<u8> {
    let entries =
        screens
            .iter()
            .map(|info| {
                let screen = &info.screen;
                let rect = |width: u32, height: u32| PbWinRect {
                    left: screen.left,
                    top: screen.top,
                    width: width as i32,
                    height: height as i32,
                };
                let mut sizes: Vec<_> = info
                    .target
                    .as_ref()
                    .map(|t| t.modes.iter().map(|m| (m.width, m.height)).collect())
                    .unwrap_or_default();
                sizes.push((screen.width, screen.height));
                sizes.sort_unstable();
                sizes.dedup();
                PbScreen {
                    // A virtual monitor can also retain a 16:9 preferred mode
                    // while a fullscreen game uses a 4:3 source. Report the
                    // driver's preferred mode consistently.
                    display_geometry: info.target.as_ref().and_then(|t| t.preferred_mode).map(
                        |(width, height)| super::super::wire::PbDisplayGeometry { width, height },
                    ),
                    id: screen.id,
                    fps: screen.fps.min(i32::MAX as u32) as i32,
                    resolutions: sizes.into_iter().map(|(w, h)| rect(w, h)).collect(),
                    current_resolution: Some(rect(screen.width, screen.height)),
                    init_resolution: Some(PbWinRect {
                        left: info.initial.left,
                        top: info.initial.top,
                        ..rect(info.initial.width, info.initial.height)
                    }),
                    screen_type: info.kind,
                    is_primary_screen: screen.primary,
                    dpi_scale: info.target.as_ref().and_then(|t| t.dpi.as_ref()).map(|d| {
                        PbDpiScale {
                            current_dpi: d.current as i32,
                            recommended_dpi: d.recommended as i32,
                            dpis: d.supported.iter().map(|v| *v as i32).collect(),
                        }
                    }),
                    display_name: info
                        .target
                        .as_ref()
                        .and_then(|target| crate::media::capture::monitor_name(&target.name))
                        .or_else(|| crate::media::capture::monitor_name(&screen.display_name))
                        .unwrap_or_default()
                        .to_owned(),
                    resolution_type: info.resolution_type,
                    video_track_index: tracks.get(&screen.id).map_or(-1, |i| *i as i32),
                }
            })
            .collect();
    crate::features::stream_control::wire::push(SessionBody::Screens(PbScreenSources {
        current_screen_id: current,
        screens: entries,
    }))
}

fn response(request_id: u32, body: SessionBody) -> Received {
    vec![crate::features::stream_control::wire::response(request_id, body)].into()
}

fn display_result(request_id: u32, error_code: i32) -> Received {
    response(request_id, SessionBody::DisplayResult(PbDisplayResult { error_code }))
}

fn capture_result(request_id: u32, errors: Vec<PbError>) -> Received {
    response(
        request_id,
        SessionBody::CaptureSettingResult(PbCaptureSettingResponse { errors }),
    )
}

/// Tell the other participants that this one changed the displays.
fn display_changed(session: &Screens) {
    let lease = session.authorization();
    crate::features::host::participants::display_changed(
        lease.participant(),
        &lease.profile().nickname,
    );
}

/// Display and capture messages of the session channel (`SESSION`).
pub(crate) async fn receive_session(session: &mut Screens, bytes: &[u8]) -> Result<Received> {
    anyhow::ensure!(session.authorization().requested(), "被控许可已失效");
    let Some(received) = crate::features::stream_control::wire::decode(bytes)? else {
        return Ok(Received::default());
    };
    if received.response {
        return Ok(Received::default());
    }
    let id = received.request_id;
    let Some(body) = received.body else {
        return Ok(Received::default());
    };
    tracing::debug!(message = body.name(), id, "host session message");
    let needs_id = !matches!(body, SessionBody::Capture(_) | SessionBody::ReceiverStats(_));
    anyhow::ensure!(!needs_id || id != 0, "会话请求缺少标识");
    match body {
        SessionBody::Capture(capture) => {
            tracing::debug!(
                start = capture.start,
                screen_id = capture.screen_id,
                "host capture action"
            );
            // A files or ports session, or one whose `view` was withdrawn,
            // never gets the picture.
            anyhow::ensure!(
                !capture.start || session.authorization().granted(farwing_protocol::caps::VIEW),
                "本次会话不允许观看画面"
            );
            if !capture.start {
                session.stop(capture.screen_id).await?;
            } else if capture.screen_id == -1 {
                session.resume_capture().await?;
            } else {
                session.start(capture.screen_id).await?;
            }
            Ok(Received {
                refresh_state: true,
                media_active: capture.start.then_some(true),
                ..Default::default()
            })
        }
        SessionBody::CreateVirtualDisplay(create) => {
            let result = async {
                anyhow::ensure!(
                    session.authorization().display_expansion_allowed(),
                    "本次会话不允许添加虚拟屏"
                );
                let resolutions = create
                    .local_resolution
                    .into_iter()
                    .map(|r| Ok((u32::try_from(r.width)?, u32::try_from(r.height)?)))
                    .collect::<Result<Vec<_>>>()?;
                session.create_virtual(resolutions).await
            }
            .await;
            if result.is_ok() {
                display_changed(session);
            }
            let code = if result.is_ok() {
                0
            } else if session.available_slot().is_none() && session.registered.len() != 1 {
                -2
            } else {
                -1
            };
            report_display_result(&result);
            Ok(display_result(id, code))
        }
        SessionBody::RemoveVirtualDisplay(remove) => {
            let result = async {
                anyhow::ensure!(
                    session.authorization().display_allowed(),
                    "本次会话不允许修改显示设置"
                );
                let id = *remove.screen_id.first().context("缺少待删除屏幕")?;
                session.remove_virtual(id).await
            }
            .await;
            if result.is_ok() {
                display_changed(session);
            }
            report_display_result(&result);
            Ok(display_result(id, if result.is_ok() { 0 } else { -1 }))
        }
        SessionBody::EnterExclusive(enter) => {
            let mut code = 502;
            let result = async {
                anyhow::ensure!(matches!(enter.reason, 1 | 2), "无效超级屏原因");
                let size = enter.resolution.context("缺少超级屏尺寸")?;
                anyhow::ensure!(size.width > 0 && size.height > 0, "无效超级屏尺寸");
                let dpi = if enter.dpi_scale <= 0 {
                    0
                } else {
                    enter.dpi_scale as u32
                };
                if enter.reason == 2 {
                    let fps = enter
                        .enter_fps
                        .as_ref()
                        .context("缺少超级屏帧率")?
                        .fps_count;
                    anyhow::ensure!((1..=144).contains(&fps), "无效超级屏帧率");
                }
                code = 501;
                // Manual/FPS expansion needs the `display` capability,
                // checked before stop(-1) or any native topology mutation.
                anyhow::ensure!(
                    session.authorization().display_expansion_allowed(),
                    "本次会话不允许切换超级屏"
                );
                session
                    .enter_super(size.width as u32, size.height as u32, dpi, true)
                    .await?;
                if let Some(fps) = enter.enter_fps.filter(|_| enter.reason == 2) {
                    session.set_fps_limit(fps.fps_count as u32);
                }
                Ok(())
            }
            .await;
            if result.is_ok() {
                display_changed(session);
            }
            report_display_result(&result);
            Ok(display_result(id, if result.is_ok() { 0 } else { code }))
        }
        SessionBody::QuitExclusive(_) => {
            let result = if session.authorization().display_allowed()
            {
                session.quit_super().await
            } else {
                Err(anyhow::anyhow!("本次会话不允许修改显示设置"))
            };
            if result.is_ok() {
                display_changed(session);
            }
            report_display_result(&result);
            Ok(display_result(id, if result.is_ok() { 0 } else { -1 }))
        }
        SessionBody::VideoTracks(tracks) => {
            let result = session.register(&tracks.video_track_index).await;
            Ok(display_result(id, if result.is_ok() { 0 } else { -1 }))
        }
        SessionBody::CaptureSetting(mut setting) => {
            tracing::info!(?setting, "host capture setting received");
            // Resolution, DPI and display-step requests change the host's
            // displays; stream settings do not.
            let display_change = setting.resolution_width > 0
                || setting.resolution_height > 0
                || setting.dpi_scale > 0
                || matches!(setting.resolution_type, 2..=4);
            if display_change
                && !session.authorization().display_allowed()
            {
                return Ok(capture_result(
                    id,
                    vec![PbError {
                        error_code: -1,
                        error_message: setting_reason::DISPLAY_NOT_ALLOWED.into(),
                        ..Default::default()
                    }],
                ));
            }
            if session.authorization().updating() {
                // An update snapshots under this same screen owner lock. Do
                // not acknowledge settings that cannot survive it.
                return Ok(capture_result(
                    id,
                    vec![PbError {
                        error_code: -2,
                        error_message: setting_reason::UPDATING.into(),
                        ..Default::default()
                    }],
                ));
            }
            session.refresh()?;
            // -1 addresses the only source of a single-track session.
            if setting.screen_id == -1 {
                let mut sources = session.slots.iter().filter_map(|slot| {
                    slot.screen
                        .as_ref()
                        .or(slot.suspended.as_ref())
                        .map(|s| s.id)
                });
                if let Some(id) = sources.next()
                    && sources.next().is_none()
                {
                    setting.screen_id = id;
                }
            }
            let ids: Vec<_> = if setting.screen_id == EXISTING_SESSION_TRACKS {
                session
                    .slots
                    .iter()
                    .filter_map(|s| s.screen.as_ref().or(s.suspended.as_ref()).map(|s| s.id))
                    .collect()
            } else {
                vec![setting.screen_id]
            };
            let mut errors = Vec::new();
            for screen_id in ids {
                match apply_setting(session, screen_id, setting.clone()).await {
                    Ok(mut result) => errors.append(&mut result),
                    Err(error) => {
                        tracing::warn!(screen_id, error = %format!("{error:#}"), "host capture setting rejected");
                        errors.push(PbError {
                            error_code: -2,
                            error_message: setting_reason::NOT_APPLIED.into(),
                            ..Default::default()
                        });
                    }
                }
            }
            if errors.is_empty() {
                errors.push(PbError::default());
            }
            if errors.iter().all(|e| {
                matches!(e.error_code, 0 | -3 | -6)
                    || (e.error_code == -5
                        && serde_json::from_str::<serde_json::Value>(&e.error_detail)
                            .ok()
                            .is_some_and(|v| {
                                v.get("error_code").and_then(|c| c.as_i64()) == Some(0)
                            }))
            }) {
                if display_change {
                    display_changed(session);
                }
                // A successful request also updates the connection's capture
                // defaults, even for a single screen. Other existing tracks
                // keep their settings; newly attached captures inherit these.
                let config = session
                    .slots
                    .iter()
                    .find(|s| {
                        s.screen
                            .as_ref()
                            .or(s.suspended.as_ref())
                            .is_some_and(|s| s.id == setting.screen_id)
                    })
                    .or_else(|| {
                        session
                            .slots
                            .iter()
                            .find(|s| s.screen.is_some() || s.suspended.is_some())
                    })
                    .map(|s| *lock(&s.config));
                if let Some(config) = config {
                    session.set_media_default(config);
                }
            }
            Ok(capture_result(id, errors))
        }
        SessionBody::MicrophonePolicy(_) => Ok(Received::default()),
        SessionBody::ReceiverStats(stats) => {
            session.receiver_stats(&stats);
            Ok(Received::default())
        }
        other if id != 0 => {
            tracing::debug!(message = other.name(), "unsupported session request");
            Ok(vec![crate::features::stream_control::wire::error_response(
                id,
                farwing_protocol::peer::error_code::UNSUPPORTED,
                (t().features.stream_unsupported_session_request)(other.name()),
            )]
            .into())
        }
        other => {
            tracing::debug!(message = other.name(), "ignored session message on the host");
            Ok(Received::default())
        }
    }
}

async fn apply_setting(
    session: &mut Screens,
    id: i32,
    setting: PbCaptureSettingRequest,
) -> Result<Vec<PbError>> {
    let mut info = session.info(id)?;
    let index = session
        .active_slot(id)
        .or_else(|| {
            (setting.screen_id == EXISTING_SESSION_TRACKS)
                .then(|| {
                    session
                        .slots
                        .iter()
                        .position(|s| s.suspended.as_ref().is_some_and(|s| s.id == id))
                })
                .flatten()
        })
        .context(t().features.stream_display_not_captured)?;
    let size = if setting.resolution_type == 3 {
        Some((info.initial.width, info.initial.height))
    } else if setting.resolution_width > 0 && setting.resolution_height > 0 {
        Some((
            setting.resolution_width as u32,
            setting.resolution_height as u32,
        ))
    } else {
        None
    };
    anyhow::ensure!(
        (0..=4).contains(&setting.resolution_type),
        "{}",
        t().features.stream_invalid_resolution_policy
    );
    anyhow::ensure!(
        (setting.resolution_width > 0) == (setting.resolution_height > 0),
        "{}",
        t().features.stream_incomplete_resolution
    );
    anyhow::ensure!(
        setting.screen_id != EXISTING_SESSION_TRACKS || (size.is_none() && setting.dpi_scale == 0),
        "{}",
        t().features.stream_display_setting_needs_screen
    );
    let mut proposed = info.screen.clone();
    if let Some((width, height)) = size {
        proposed.width = width;
        proposed.height = height;
    }
    let slot = &session.slots[index];
    let mut validation = *lock(&slot.config);
    let validation_errors = configure_media(
        setting.clone(),
        &proposed,
        &mut validation,
        &slot.negotiated,
    );
    if validation_errors
        .iter()
        .any(|e| matches!(e.error_code, -1 | -2))
    {
        return Ok(validation_errors);
    }
    let mut conversion = false;
    let mut current_id = id;
    if let Some(size) = size {
        if size != (info.screen.width, info.screen.height) {
            let supported = info.target.as_ref().is_some_and(|t| {
                t.modes
                    .iter()
                    .any(|m| m.width == size.0 && m.height == size.1)
            });
            if info.kind == 0 && !supported && setting.resolution_type == 2 {
                let result = session.enter_super(size.0, size.1, 0, false).await;
                match result {
                    Ok(id) => {
                        current_id = id;
                        conversion = true;
                    }
                    Err(error) => {
                        tracing::warn!(error = %format!("{error:#}"), "super display conversion failed");
                        return Ok(vec![PbError {
                            error_code: -5,
                            error_message: setting_reason::CONVERSION_FAILED.into(),
                            error_detail: serde_json::json!({"error_code":501}).to_string(),
                        }]);
                    }
                }
            } else {
                current_id = session.set_resolution(info.clone(), size.0, size.1).await?;
            }
            info = session.info(current_id)?;
        }
    }
    if size.is_some() && (2..=4).contains(&setting.resolution_type) {
        session
            .displays
            .resolution_choice(
                info.screen
                    .identity
                    .clone()
                    .context(t().features.stream_missing_display_target_id)?,
                setting.resolution_type,
            )
            .await?;
    }
    if setting.dpi_scale != 0 {
        let target = info
            .target
            .clone()
            .context(t().features.stream_missing_dpi_target)?;
        let dpi = if setting.dpi_scale == -1 {
            target
                .dpi
                .as_ref()
                .context(t().features.stream_dpi_unavailable)?
                .recommended
        } else {
            u32::try_from(setting.dpi_scale).context(t().features.stream_invalid_dpi)?
        };
        session.displays.set_dpi(target, dpi).await?;
    }
    session.refresh()?;
    let screen = session.info(current_id)?.screen;
    anyhow::ensure!(
        session.authorization().requested(),
        "{}",
        t().features.stream_host_permission_expired
    );
    let slot = &session.slots[session.active_slot(current_id).unwrap_or(index)];
    let mut errors = configure_media(
        setting,
        &screen,
        &mut lock(&slot.config),
        &slot.negotiated,
    );
    if conversion {
        errors.push(PbError {
            error_code: -5,
            error_detail: serde_json::json!({"error_code":0}).to_string(),
            ..Default::default()
        });
    }
    Ok(errors)
}

fn report_display_result(result: &Result<()>) {
    if let Err(error) = result {
        tracing::warn!(%error,"host display operation rejected or failed");
    }
}

fn configure_media(
    mut setting: PbCaptureSettingRequest,
    screen: &Screen,
    config: &mut VideoConfig,
    negotiated: &crate::features::host::format::Negotiated,
) -> Vec<PbError> {
    // Native mode changes and their failure are handled above. Reuse only the
    // established codec/quality negotiation, with the new actual mode snapshot.
    setting.screen_id = screen.id;
    setting.resolution_width = screen.width as i32;
    setting.resolution_height = screen.height as i32;
    setting.resolution_type = 1;
    setting.dpi_scale = 0;
    super::apply_capture_setting(&setting, screen, config, negotiated)
}
