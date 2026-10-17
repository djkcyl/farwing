//! Display RPC direction for the controlled role. Host owners perform native
//! operations; protobuf construction stays beside the existing wire contract.
use super::*;
use crate::features::host::lock;
use crate::features::host::peer::screens::{ScreenInfo, Screens};
use crate::features::stream_control::display_topology::PbDisplayResult;
use crate::features::stream_control::wire::PbCaptureSettingRequest;
use std::collections::BTreeMap;

pub(crate) fn screen_states(
    screens: &[ScreenInfo],
    tracks: &BTreeMap<i32, usize>,
    current: i32,
    display_geometry: bool,
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
                    pixel_width: width as i32,
                    pixel_height: height as i32,
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
                    openuuyc_geometry: display_geometry
                        .then(|| {
                            // A virtual monitor can also retain a 16:9 preferred
                            // mode while a fullscreen game uses a 4:3 source.
                            // Report the driver's preferred mode consistently.
                            let size = info.target.as_ref().and_then(|t| t.preferred_mode);
                            size.map(|(width, height)| super::super::wire::PbDisplayGeometry {
                                version: 1,
                                width,
                                height,
                            })
                        })
                        .flatten(),
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
                    dpr: 1.0,
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
                    builtin_screen_type: 0,
                }
            })
            .collect();
    PbControlMessage {
        payload: Some(PbPayload::Screens(PbScreenSources {
            current_screen_id: current,
            screens: entries,
        })),
        ..Default::default()
    }
    .encode_to_vec()
}

fn response(
    message: &PbControlMessage,
    header: Option<PbResponseHeader>,
    payload: PbRpcResponsePayload,
) -> Received {
    vec![
        PbControlMessage {
            seq: message.seq,
            timestamp: message.timestamp,
            payload: Some(PbPayload::RpcResponse(PbRpcResponse {
                response_header: header,
                payload: Some(payload),
            })),
        }
        .encode_to_vec(),
    ]
    .into()
}

pub(crate) async fn receive_session(
    session: &mut Screens,
    bytes: &[u8],
    control: bool,
) -> Result<Received> {
    anyhow::ensure!(session.authorization().requested(), "被控许可已失效");
    if bytes.first() == Some(&b'{') {
        return Ok(Received::default());
    }
    let message = PbControlMessage::decode(bytes)?;
    if !control {
        if let Some(PbPayload::SimpleAction(action)) = &message.payload {
            if action.action == 21 {
                // 4.41 C140DB9A10 sends quitCapture when viewing ends while
                // another business (files/ports) still owns the connection.
                // S140560F60 -> S140452530 stops all capture without dropping
                // the shared peer. Action 7 remains video-only (audio mode).
                session.stop(-1).await?;
                tracing::info!("host media stopped; shared business connection retained");
                return Ok(Received {
                    refresh_state: true,
                    media_active: Some(false),
                    ..Default::default()
                });
            }
            if matches!(action.action, 7 | 8) {
                let args: serde_json::Value = if action.args.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::from_str(&action.args)?
                };
                anyhow::ensure!(
                    action.action == 7 || args.get("scene").is_none_or(|v| v.as_str() == Some("")),
                    "不支持的采集重启场景"
                );
                let id = args
                    .get("screen_id")
                    .map(|v| {
                        v.as_i64()
                            .and_then(|id| i32::try_from(id).ok())
                            .context("无效屏幕标识")
                    })
                    .transpose()?
                    .unwrap_or(-1);
                tracing::debug!(
                    action = action.action,
                    screen_id = id,
                    "host capture action"
                );
                if action.action == 7 {
                    session.stop(id).await?;
                } else if id == -1 {
                    session.resume_capture().await?;
                } else {
                    session.start(id).await?;
                }
                return Ok(Received {
                    refresh_state: true,
                    media_active: (action.action == 8).then_some(true),
                    ..Default::default()
                });
            }
        }
        if let Some(PbPayload::RpcRequest(bytes)) = &message.payload {
            let request = PbRpcRequest::decode(bytes.as_slice())?;
            let header = request.request_header.clone().map(|h| PbResponseHeader {
                request_id: h.request_id,
            });
            match request.payload {
                Some(PbRpcRequestPayload::TerminalCheck(bytes)) => {
                    // `terminal_check_env_req` asks whether this machine can
                    // serve a terminal at all. The client retries on timeout
                    // and shows `componentUnavailable` when it never hears
                    // back, so this must answer even when the grant is off.
                    #[derive(Clone, PartialEq, prost::Message)]
                    struct CheckEnvReq {
                        #[prost(uint64, tag = "1")]
                        retry: u64,
                    }
                    let _ = CheckEnvReq::decode(bytes.as_slice())?;
                    // A host that cannot serve a terminal says so; otherwise
                    // the answer is the host's permission.
                    let refusal = crate::features::host::terminal::unavailable().or_else(|| {
                        (!session.authorization().terminal_allowed()).then_some("本机未允许终端")
                    });
                    let allowed = refusal.is_none();
                    let resp = CheckEnvResp::result(refusal);
                    // The controller only gets a reason code, so the readable
                    // reason stays in the host's log.
                    tracing::info!(
                        allowed,
                        reason = refusal.unwrap_or_default(),
                        code = resp.code,
                        "host terminal environment check answered"
                    );
                    return Ok(response(
                        &message,
                        header,
                        PbRpcResponsePayload::TerminalCheckEnvRsp(resp.encode_to_vec()),
                    ));
                }
                Some(PbRpcRequestPayload::TerminalRename(bytes)) => {
                    #[derive(Clone, PartialEq, prost::Message)]
                    struct RenameReq {
                        // The controller sends the handle as a number; the
                        // capture showed a varint where a string was expected.
                        #[prost(uint32, tag = "1")]
                        session_id: u32,
                        #[prost(string, tag = "2")]
                        new_name: String,
                    }
                    let request = RenameReq::decode(bytes.as_slice())?;
                    // Renaming reaches the same sessions the frame handler
                    // guards, so it checks the grant and the owner too, and a
                    // refusal is answered so the controller is not left
                    // waiting.
                    let lease = session.authorization();
                    let account = lease.account().to_owned();
                    tracing::info!(
                        session = request.session_id,
                        "host terminal rename requested"
                    );
                    let result = if lease.terminal_allowed() {
                        let sessions = session.terminal();
                        let mut sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
                        // The shells opened on this machine are renamed only
                        // by a controller allowed to reach them.
                        let owner = crate::features::host::terminal::Owner::new(
                            &account,
                            lease.terminal_local_allowed(),
                        );
                        sessions
                            .rename(owner, request.session_id, &request.new_name)
                            .map_err(|error| error.to_string())
                    } else {
                        Err("本机未允许终端".to_owned())
                    };
                    let (code, detail) = match &result {
                        Ok(()) => (0, String::new()),
                        Err(error) => (1, error.clone()),
                    };
                    if let Err(error) = &result {
                        tracing::warn!(%error, "host terminal rename rejected");
                    }
                    return Ok(response(
                        &message,
                        header,
                        PbRpcResponsePayload::TerminalRenameSessionRsp(
                            RenameSessionResp {
                                session_id: request.session_id,
                                code,
                                message: detail,
                            }
                            .encode_to_vec(),
                        ),
                    ));
                }
                Some(PbRpcRequestPayload::MouseSwitch(bytes)) => {
                    #[derive(Clone, PartialEq, prost::Message)]
                    struct Switch {
                        #[prost(bool, tag = "1")]
                        enable: bool,
                    }
                    let _ = Switch::decode(bytes.as_slice())?;
                    // S4D01A0 -> IInputActions+56 -> S49C4A0 -> S5E5810:
                    // current Windows returns supported and performs no install,
                    // removal or persistent preference mutation for this request.
                    return Ok(response(
                        &message,
                        header,
                        PbRpcResponsePayload::MouseSwitchResponse(
                            PbDisplayResult { error_code: 0 }.encode_to_vec(),
                        ),
                    ));
                }
                Some(PbRpcRequestPayload::CreateVirtualDisplay(create)) => {
                    let result = async {
                        anyhow::ensure!(
                            create.virtual_display_count == 1,
                            "每次只能添加一块虚拟屏"
                        );
                        let resolutions = create
                            .local_resolution
                            .into_iter()
                            .map(|r| Ok((u32::try_from(r.width)?, u32::try_from(r.height)?)))
                            .collect::<Result<Vec<_>>>()?;
                        session.create_virtual(resolutions).await
                    }
                    .await;
                    let code = if result.is_ok() {
                        0
                    } else if session.available_slot().is_none() && session.registered.len() != 1 {
                        -2
                    } else {
                        -1
                    };
                    report_display_result(&result);
                    return Ok(response(
                        &message,
                        header,
                        PbRpcResponsePayload::CreateVirtualDisplayRsp(
                            PbDisplayResult { error_code: code }.encode_to_vec(),
                        ),
                    ));
                }
                Some(PbRpcRequestPayload::RemoveVirtualDisplay(remove)) => {
                    let result = async {
                        let id = *remove.screen_id.first().context("缺少待删除屏幕")?;
                        session.remove_virtual(id).await
                    }
                    .await;
                    report_display_result(&result);
                    return Ok(response(
                        &message,
                        header,
                        PbRpcResponsePayload::RemoveVirtualDisplayRsp(
                            PbDisplayResult {
                                error_code: if result.is_ok() { 0 } else { -1 },
                            }
                            .encode_to_vec(),
                        ),
                    ));
                }
                Some(PbRpcRequestPayload::EnterSuperScreen(enter)) => {
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
                        // The official controller omits manual/FPS expansion
                        // in assistance. Enforce our matching session policy
                        // before stop(-1) or any native topology mutation.
                        // CaptureSetting's resolution-conversion path below
                        // and QuitSuperScreen are deliberately independent.
                        anyhow::ensure!(
                            session.authorization().display_expansion_allowed(),
                            "本次会话不允许手动进入超级屏或通过超级屏提高帧率"
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
                    report_display_result(&result);
                    return Ok(response(
                        &message,
                        header,
                        PbRpcResponsePayload::EnterSuperScreenRep(
                            PbDisplayResult {
                                error_code: if result.is_ok() { 0 } else { code },
                            }
                            .encode_to_vec(),
                        ),
                    ));
                }
                Some(PbRpcRequestPayload::QuitSuperScreen(_)) => {
                    let result = session.quit_super().await;
                    report_display_result(&result);
                    return Ok(response(
                        &message,
                        header,
                        PbRpcResponsePayload::QuitSuperScreen(
                            PbDisplayResult {
                                error_code: if result.is_ok() { 0 } else { -1 },
                            }
                            .encode_to_vec(),
                        ),
                    ));
                }
                Some(PbRpcRequestPayload::SendVideoTrack(tracks)) => {
                    let result = session.register(&tracks.video_track_index).await;
                    return Ok(response(
                        &message,
                        header,
                        PbRpcResponsePayload::SendVideoTrackRsp(PbSendVideoTrackResponse {
                            error_code: if result.is_ok() { 0 } else { -1 },
                        }),
                    ));
                }
                Some(PbRpcRequestPayload::CaptureSetting(mut setting)) => {
                    tracing::info!(?setting, "host capture setting received");
                    if session.authorization().updating() {
                        // PrepareUpdate snapshots under this same screen owner
                        // lock. Do not acknowledge newer settings that cannot
                        // survive the already-started update transaction.
                        return Ok(response(
                            &message,
                            header,
                            PbRpcResponsePayload::CaptureSetting(PbCaptureSettingResponse {
                                errors: vec![PbError {
                                    error_code: -2,
                                    error_message: "正在更新，暂时无法修改画面设置".into(),
                                    ..Default::default()
                                }],
                            }),
                        ));
                    }
                    session.refresh()?;
                    // Preserve the original target before normalizing -1. Only
                    // an explicit media-only request may select a new source.
                    let selects_screen = setting.screen_id >= 0
                        && matches!(setting.resolution_type, 0 | 1)
                        && setting.resolution_width <= 0
                        && setting.resolution_height <= 0
                        && setting.resolution_pixel_width <= 0
                        && setting.resolution_pixel_height <= 0
                        && setting.dpi_scale == 0;
                    // S456070 -> S456550: a single desktop track accepts -1
                    // for its current source. Android uses this for ordinary
                    // media settings. Do not guess a target in a multi-track session.
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
                            .filter_map(|s| {
                                s.screen.as_ref().or(s.suspended.as_ref()).map(|s| s.id)
                            })
                            .collect()
                    } else {
                        vec![setting.screen_id]
                    };
                    let mut errors = Vec::new();
                    for id in ids {
                        match apply_setting(
                            session,
                            id,
                            setting.clone(),
                            selects_screen,
                            request.request_header.clone(),
                            &message,
                        )
                        .await
                        {
                            Ok(mut result) => errors.append(&mut result),
                            Err(error) => {
                                tracing::warn!(screen_id=id, %error, "host capture setting rejected");
                                errors.push(PbError {
                                    error_code: -2,
                                    error_message: format!("{error:#}"),
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
                        // S456550/5419C0 update the connection's capture defaults
                        // even for a single-screen request. Existing other tracks
                        // keep their settings; newly attached captures inherit it.
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
                    return Ok(response(
                        &message,
                        header,
                        PbRpcResponsePayload::CaptureSetting(PbCaptureSettingResponse { errors }),
                    ));
                }
                _ => (),
            }
        }
    }
    let screen = session
        .slots
        .iter()
        .find_map(|s| s.screen.clone())
        .or_else(|| session.handshake_source());
    let slot = &session.slots[screen
        .as_ref()
        .and_then(|s| session.active_slot(s.id))
        .unwrap_or(0)];
    super::receive(
        bytes,
        control,
        screen.as_ref(),
        &mut lock(&slot.config),
        &slot.negotiated,
    )
}

async fn apply_setting(
    session: &mut Screens,
    id: i32,
    setting: PbCaptureSettingRequest,
    selects_screen: bool,
    header: Option<PbRequestHeader>,
    message: &PbControlMessage,
) -> Result<Vec<PbError>> {
    let mut info = session.info(id)?;
    // S456070 routes an explicit target to the sole registered track. With a
    // multi-track pool it requires an existing screen-to-track mapping.
    // A stopped viewing session still requires the explicit resume action.
    if selects_screen
        && session.active_slot(id).is_none()
        && session.registered.len() == 1
        && session.slots[session.registered[0]].screen.is_some()
    {
        let index = session.registered[0];
        let slot = &session.slots[index];
        let mut config = session.update_media_intent();
        // Reject invalid media settings before replacing the currently watched
        // source. Start with the validated configuration, not the previous one.
        let errors = configure_media(
            setting,
            &info.screen,
            header,
            message,
            &mut config,
            &slot.negotiated,
        )?;
        if errors.iter().any(|e| matches!(e.error_code, -1 | -2)) {
            return Ok(errors);
        }
        session
            .start_configured(info.screen, config)
            .await
            .context("启动指定显示器采集失败")?;
        return Ok(errors);
    }
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
        .context("显示器尚未开始采集")?;
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
    anyhow::ensure!((0..=4).contains(&setting.resolution_type), "无效分辨率策略");
    anyhow::ensure!(
        (setting.resolution_width > 0) == (setting.resolution_height > 0),
        "不完整的分辨率参数"
    );
    anyhow::ensure!(
        setting.screen_id != EXISTING_SESSION_TRACKS || (size.is_none() && setting.dpi_scale == 0),
        "显示设置需要具体屏幕标识"
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
        header.clone(),
        message,
        &mut validation,
        &slot.negotiated,
    )?;
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
                        return Ok(vec![PbError {
                            error_code: -5,
                            error_message: format!("{error:#}"),
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
                info.screen.identity.clone().context("缺少显示目标身份")?,
                setting.resolution_type,
            )
            .await?;
    }
    if setting.dpi_scale != 0 {
        let target = info.target.clone().context("缺少DPI显示目标")?;
        let dpi = if setting.dpi_scale == -1 {
            target.dpi.as_ref().context("DPI不可用")?.recommended
        } else {
            u32::try_from(setting.dpi_scale).context("无效DPI")?
        };
        session.displays.set_dpi(target, dpi).await?;
    }
    session.refresh()?;
    let screen = session.info(current_id)?.screen;
    anyhow::ensure!(session.authorization().requested(), "被控许可已失效");
    let slot = &session.slots[session.active_slot(current_id).unwrap_or(index)];
    let mut errors = configure_media(
        setting,
        &screen,
        header,
        message,
        &mut lock(&slot.config),
        &slot.negotiated,
    )?;
    if conversion {
        errors.push(PbError {
            error_code: -5,
            error_detail: serde_json::json!({"error_code":0}).to_string(),
            ..Default::default()
        });
    }
    Ok(errors)
}

/// The terminal environment check reply.
///
/// The controller requires `code == 0` **and** `message` to be exactly `"ok"`
/// (a length-2 byte comparison); replying with only `code = 0` always takes
/// its failure branch. The string is part of the wire contract, not a note
/// for humans.
#[derive(Clone, PartialEq, prost::Message)]
struct CheckEnvResp {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
}

impl CheckEnvResp {
    /// The three answers the reference host gives: terminal available, no
    /// terminal component on this machine, or the environment check failed.
    fn result(refusal: Option<&'static str>) -> Self {
        match refusal {
            None => Self {
                code: 0,
                message: "ok".to_owned(),
            },
            Some(_) if crate::features::host::terminal::unavailable().is_some() => Self {
                code: 5,
                message: "terminal_component_unavailable".to_owned(),
            },
            Some(_) => Self {
                code: 6,
                message: "environment_check_failed".to_owned(),
            },
        }
    }
}

/// The terminal rename reply, in the reference server's own descriptor:
///
/// ```text
/// message RenameSessionResp {
///   uint32 session_id = 1;
///   int32  code       = 2;
///   string message    = 3;
/// }
/// ```
#[derive(Clone, PartialEq, prost::Message)]
struct RenameSessionResp {
    #[prost(uint32, tag = "1")]
    session_id: u32,
    #[prost(int32, tag = "2")]
    code: i32,
    #[prost(string, tag = "3")]
    message: String,
}

fn report_display_result(result: &Result<()>) {
    if let Err(error) = result {
        tracing::warn!(%error,"host display operation rejected or failed");
    }
}

fn configure_media(
    mut setting: PbCaptureSettingRequest,
    screen: &Screen,
    header: Option<PbRequestHeader>,
    message: &PbControlMessage,
    config: &mut VideoConfig,
    negotiated: &crate::features::host::format::Negotiated,
) -> Result<Vec<PbError>> {
    // Native mode changes and their failure are handled above. Reuse only the
    // established codec/quality negotiation, with the new actual mode snapshot.
    setting.screen_id = screen.id;
    setting.resolution_width = screen.width as i32;
    setting.resolution_height = screen.height as i32;
    setting.resolution_pixel_width = screen.width as i32;
    setting.resolution_pixel_height = screen.height as i32;
    setting.resolution_type = 1;
    setting.dpi_scale = 0;
    let bytes = PbControlMessage {
        seq: message.seq,
        timestamp: message.timestamp,
        payload: Some(PbPayload::RpcRequest(
            PbRpcRequest {
                request_header: header,
                payload: Some(PbRpcRequestPayload::CaptureSetting(setting)),
            }
            .encode_to_vec(),
        )),
    }
    .encode_to_vec();
    let result = super::receive(&bytes, false, Some(screen), config, negotiated)?;
    let mut errors = Vec::new();
    for bytes in result.messages {
        if let Some(PbPayload::RpcResponse(response)) =
            PbControlMessage::decode(bytes.as_slice())?.payload
        {
            if let Some(PbRpcResponsePayload::CaptureSetting(response)) = response.payload {
                errors.extend(response.errors);
            }
        }
    }
    Ok(errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handle goes first, then the code and the message, as the reference
    /// descriptor numbers them.
    #[test]
    fn the_rename_reply_follows_the_reference_descriptor() {
        let encoded = RenameSessionResp {
            session_id: 7,
            code: 1,
            message: "x".to_owned(),
        }
        .encode_to_vec();
        assert_eq!(encoded, [0x08, 7, 0x10, 1, 0x1a, 1, b'x']);
    }

    /// proto3 drops the default `code = 0`, so the success reply is only the
    /// `message` field: `12 02 6f 6b`. The controller compares those two bytes
    /// literally and treats anything else as a failed environment check.
    #[test]
    fn the_environment_check_success_reply_is_the_literal_ok() {
        assert_eq!(
            CheckEnvResp::result(None).encode_to_vec(),
            [0x12, 0x02, b'o', b'k']
        );
    }

    /// A refusal carries a machine-readable reason. `code = 5` means this
    /// machine has no terminal backend; every other refusal is `code = 6`.
    #[test]
    fn a_refusal_names_the_reason() {
        let encoded = CheckEnvResp::result(Some("本机未允许终端")).encode_to_vec();
        if crate::features::host::terminal::unavailable().is_some() {
            assert_eq!(encoded[..2], [0x08, 5]);
            assert!(encoded.ends_with(b"terminal_component_unavailable"));
        } else {
            assert_eq!(encoded[..2], [0x08, 6]);
            assert!(encoded.ends_with(b"environment_check_failed"));
        }
    }
}
