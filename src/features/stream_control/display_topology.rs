//! Explicit remote display topology actions; ordinary capture settings remain independent.
use super::wire::PbRpcRequestPayload;
use super::*;
use crate::account::feature_ability::Feature;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayTopologyAction {
    Create {
        width: u32,
        height: u32,
    },
    Remove {
        screen_id: i32,
    },
    Enter {
        width: u32,
        height: u32,
        dpi: u32,
    },
    Exit,
    Resolution {
        screen_id: i32,
        choice: DisplayResolution,
    },
    FrameRate {
        settings: StreamControlSettings,
        width: u32,
        height: u32,
        dpi: u32,
    },
}

impl DisplayTopologyAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Create { .. } => "添加虚拟屏",
            Self::Remove { .. } => "删除虚拟屏",
            Self::Exit => "退出超级屏",
            _ => "切换超级屏",
        }
    }
    pub fn global(self) -> bool {
        !matches!(self, Self::Create { .. } | Self::Remove { .. })
    }
}

#[derive(Clone, Debug, Default)]
pub struct DisplayTopologySupport {
    pub create: bool,
    pub create_visible: bool,
    pub create_unavailable: Option<&'static str>,
    pub enter: bool,
    pub exit: bool,
    pub resolution_conversion: bool,
    pub fps_conversion: bool,
    pub super_screen: bool,
}

#[derive(Clone, Debug, Default)]
pub struct DisplayTopologyStatus {
    pub sequence: Option<i64>,
    pub origin_screen: i32,
    pub action: Option<DisplayTopologyAction>,
    pub pending: bool,
    pub dismissed: bool,
    pub reported: bool,
    pub selected_screen: Option<i32>,
    pub transient_screens: Vec<RemoteScreen>,
    pub message: String,
    pub error: bool,
}

impl DisplayTopologyStatus {
    pub fn blocks(&self, screen: i32) -> bool {
        self.pending && !self.dismissed && self.action.is_some_and(|action| {
            action.global()
                || matches!(action, DisplayTopologyAction::Create { .. }
                    if screen == self.origin_screen || self.selected_screen == Some(screen))
                || matches!(action, DisplayTopologyAction::Remove { screen_id } if screen_id == screen)
        })
    }
}

struct Pending {
    sequence: i64,
    generation: u64,
    report: u64,
    report_at: Option<Instant>,
    action: DisplayTopologyAction,
    before: Vec<ScreenBaseline>,
    rollback: Option<(FrameRateChoice, FrameRateChoice, i64)>,
}

#[derive(Default)]
pub(super) struct DisplayTopology {
    current: Option<Pending>,
    pub status: DisplayTopologyStatus,
}

impl DisplayTopology {
    pub fn disconnect(&mut self) {
        self.current = None;
        self.status.transient_screens.clear();
        if self.status.pending {
            self.status.pending = false;
            self.status.error = true;
            self.status.message = "连接已断开，请重新连接后查看实际显示状态".into();
        }
    }

    pub fn observe(&mut self, report: u64, screens: &[ScreenBaseline]) {
        let Some(pending) = &mut self.current else {
            return;
        };
        if report <= pending.report {
            return;
        }
        self.status.transient_screens.clear();
        if self.status.reported {
            if !self.status.pending
                || self
                    .status
                    .selected_screen
                    .is_some_and(|id| screens.iter().any(|s| s.id == id))
            {
                return;
            }
            self.status.reported = false;
            self.status.selected_screen = None;
            pending.report_at = None;
        }
        let super_screen = screens.iter().find(|s| s.display.screen_type == 2);
        let selected = match pending.action {
            DisplayTopologyAction::Create { .. } => screens
                .iter()
                .find(|s| {
                    s.display.screen_type == 1
                        && !pending
                            .before
                            .iter()
                            .any(|old| old.id == s.id && matches!(old.display.screen_type, 1 | 2))
                })
                .map(|s| s.id),
            DisplayTopologyAction::Remove { screen_id } => (!screens.is_empty()
                && !screens.iter().any(|s| s.id == screen_id))
            .then(|| screens.iter().find(|s| s.primary).unwrap_or(&screens[0]).id),
            DisplayTopologyAction::Exit => (!screens.is_empty() && super_screen.is_none())
                .then(|| screens.iter().find(|s| s.primary).unwrap_or(&screens[0]).id),
            DisplayTopologyAction::Resolution { screen_id, choice } => {
                super_screen.map(|s| s.id).or_else(|| {
                    screens
                        .iter()
                        .find(|s| {
                            s.id == screen_id
                                && display_settings::expand_resolution(&s.display, choice)
                                    .is_ok_and(|(mode, _)| mode == s.display.current)
                        })
                        .map(|s| s.id)
                })
            }
            _ => super_screen.map(|s| s.id),
        };
        if let Some(id) = selected {
            if matches!(pending.action, DisplayTopologyAction::Create { .. }) {
                // G BCE860: bridge one creation report, not normal hot-unplug events.
                self.status.transient_screens = pending
                    .before
                    .iter()
                    .filter(|old| {
                        old.display.screen_type == 0 && !screens.iter().any(|s| s.id == old.id)
                    })
                    .map(|old| RemoteScreen {
                        id: old.id,
                        name: old.name.clone(),
                        primary: old.primary,
                        video_track_index: old.video_track_index,
                        width: old.width,
                        height: old.height,
                        refresh_hz: old.fps,
                        display: old.display.clone(),
                    })
                    .collect();
            }
            self.status.reported = true;
            self.status.selected_screen = Some(id);
            pending.report_at = Some(Instant::now());
            if matches!(pending.action, DisplayTopologyAction::Remove { .. }) {
                self.status.pending = false;
            }
            if !self.status.error {
                self.status.message =
                    if matches!(pending.action, DisplayTopologyAction::Remove { .. }) {
                        "显示器已删除"
                    } else {
                        "显示状态已更新，正在恢复画面…"
                    }
                    .into();
            }
            // A late error remains meaningful, but must not prevent consuming actual state.
        }
    }
}

pub(super) fn support(state: &StreamControlState) -> DisplayTopologySupport {
    let available = display_settings::supported(state) && state.peer_capture_setting >= 5;
    let super_screen = state.screens.iter().any(|s| s.display.screen_type == 2);
    let create_unavailable = if !available {
        Some("对端未开放虚拟屏")
    } else if super_screen {
        Some("请先退出超级屏")
    } else if state.assistance {
        Some("远程协助不开放添加虚拟屏")
    } else if state.screens.len() >= 5 {
        Some("最多支持5个显示器")
    } else if state
        .screens
        .iter()
        .filter(|s| matches!(s.display.screen_type, 1 | 2))
        .count()
        >= 3
    {
        Some("最多支持3个UU虚拟屏")
    } else if state.registered_video_tracks.is_empty() {
        Some("正在登记视频轨道")
    } else if state
        .registered_video_tracks
        .iter()
        .all(|t| state.screens.iter().any(|s| s.video_track_index == *t))
    {
        Some("没有可用视频轨道")
    } else {
        None
    };
    DisplayTopologySupport {
        create: create_unavailable.is_none(),
        create_visible: display_settings::supported(state) && !super_screen && !state.assistance,
        create_unavailable,
        enter: available
            && !super_screen
            && !state.assistance
            && feature_supported(state, Feature::ManualSuperScreen),
        exit: available && super_screen,
        resolution_conversion: available,
        fps_conversion: available
            && !super_screen
            && !state.assistance
            && feature_supported(state, Feature::FpsSuperScreen),
        super_screen,
    }
}

impl StreamControlHandle {
    pub(crate) fn display_input_blocked(&self, track: i32, binding: u64) -> bool {
        let state = lock(&self.shared);
        state.topology.status.blocks(binding as u32 as i32)
            || state.topology.status.error && !state.topology.status.dismissed
            || !state.screens.iter().any(|screen| {
                screen.video_track_index == track
                    && (screen.id as u32 as u64
                        | ((screen.display.screen_type as u32 as u64) << 32))
                        == binding
            })
    }

    pub(crate) fn topology_frame_presented(&self, screen_id: i32, received_at: Instant) {
        let mut state = lock(&self.shared);
        if state.topology.status.pending
            && state.topology.status.reported
            && state.topology.status.selected_screen == Some(screen_id)
            && state
                .topology
                .current
                .as_ref()
                .and_then(|p| p.report_at)
                .is_some_and(|at| received_at >= at)
        {
            state.topology.status.pending = false;
            state.topology.status.message = "画面已恢复".into();
            tracing::info!(screen_id, sequence = ?state.topology.status.sequence, "display topology presentation resumed");
            drop(state);
            self.mouse.repaint();
        }
    }

    pub(crate) fn set_display_connection_type(
        &self,
        kind: crate::session::negotiation::ControlConnectType,
    ) {
        lock(&self.shared).assistance = matches!(
            kind,
            crate::session::negotiation::ControlConnectType::Assistance
        );
    }

    pub fn dismiss_display_topology(&self) {
        lock(&self.shared).topology.status.dismissed = true;
    }

    pub fn frame_rate_needs_super_screen(
        &self,
        screen_id: i32,
        settings: StreamControlSettings,
    ) -> bool {
        let state = lock(&self.shared);
        support(&state).fps_conversion
            && settings.frame_rate != state.settings.frame_rate
            && state
                .screens
                .iter()
                .find(|s| s.id == screen_id)
                .is_some_and(|s| {
                    settings
                        .frame_rate
                        .requires_faster_screen(state.local_display, s.fps)
                })
    }

    /// Caller has shown the specific topology effect and received explicit confirmation.
    pub fn apply_display_topology(
        &self,
        origin: i32,
        action: DisplayTopologyAction,
    ) -> Result<i64> {
        let mut state = lock(&self.shared);
        ensure_ready(&state)?;
        let capabilities = support(&state);
        anyhow::ensure!(capabilities.resolution_conversion, "对端未开放虚拟显示功能");
        anyhow::ensure!(
            !state.topology.status.pending || state.topology.status.dismissed,
            "正在处理显示器操作"
        );
        anyhow::ensure!(
            state.screens.iter().any(|s| s.id == origin),
            "发起操作的显示器已断开"
        );
        let mut request = PbRpcRequest::default();
        let mut rollback = None;
        match action {
            DisplayTopologyAction::Create { width, height } => {
                super::entitlements::require(&state, "screen_virtual", None, "虚拟屏")?;
                anyhow::ensure!(
                    capabilities.create,
                    "{}",
                    capabilities.create_unavailable.unwrap_or("无法添加虚拟屏")
                );
                request.payload = Some(PbRpcRequestPayload::CreateVirtualDisplay(
                    PbCreateVirtualDisplay {
                        local_resolution: vec![resolution(width, height)?],
                        virtual_display_count: 1,
                    },
                ));
            }
            DisplayTopologyAction::Remove { screen_id } => {
                anyhow::ensure!(
                    state
                        .screens
                        .iter()
                        .any(|s| s.id == screen_id && s.display.screen_type == 1),
                    "只能删除当前UU虚拟屏"
                );
                anyhow::ensure!(
                    state.screens.len() > 1,
                    "最后一个显示器不能删除，请关闭观看窗口"
                );
                request.payload = Some(PbRpcRequestPayload::RemoveVirtualDisplay(
                    PbRemoveVirtualDisplay {
                        screen_id: vec![screen_id],
                    },
                ));
            }
            DisplayTopologyAction::Enter { width, height, dpi } => {
                super::entitlements::require(&state, "screen_super", None, "超级屏")?;
                anyhow::ensure!(capabilities.enter, "当前会话不能手动进入超级屏");
                request.payload = Some(PbRpcRequestPayload::EnterSuperScreen(PbEnterSuperScreen {
                    reason: 1,
                    resolution: Some(resolution(width, height)?),
                    dpi_scale: i32::try_from(dpi)?,
                    enter_fps: None,
                }));
            }
            DisplayTopologyAction::Exit => {
                anyhow::ensure!(capabilities.exit, "当前未处于超级屏");
                request.payload = Some(PbRpcRequestPayload::QuitSuperScreen(PbQuitSuperScreen {}));
            }
            DisplayTopologyAction::Resolution { screen_id, choice } => {
                super::entitlements::require(&state, "screen_super", None, "超级屏")?;
                let screen = state
                    .screens
                    .iter()
                    .find(|s| s.id == screen_id)
                    .ok_or_else(|| anyhow!("显示器已断开"))?;
                let (mode, kind) = display_settings::expand_resolution(&screen.display, choice)?;
                resolution(mode.width, mode.height)?;
                let mut capture = capture_setting_request(state.baseline)?;
                capture.screen_id = screen_id;
                capture.resolution_width = i32::try_from(mode.width)?;
                capture.resolution_height = i32::try_from(mode.height)?;
                capture.resolution_pixel_width = i32::try_from(mode.pixel_width)?;
                capture.resolution_pixel_height = i32::try_from(mode.pixel_height)?;
                capture.resolution_type = kind;
                request.payload = Some(PbRpcRequestPayload::CaptureSetting(capture));
            }
            DisplayTopologyAction::FrameRate {
                settings,
                width,
                height,
                dpi,
            } => {
                anyhow::ensure!(
                    capabilities.fps_conversion,
                    "当前会话不能通过帧率切换超级屏"
                );
                super::entitlements::require(&state, "screen_super", None, "超级屏")?;
                super::entitlements::require(
                    &state,
                    "screen_refresh_rate",
                    Some(serde_json::json!(
                        settings.frame_rate.value(state.local_display)
                    )),
                    "所选帧率",
                )?;
                let target = resolution(width, height)?;
                let dpi = i32::try_from(dpi)?;
                // Confirmation may remain open while another window changes quality.
                // This action owns FPS only, never a stale full settings snapshot.
                let settings = StreamControlSettings {
                    frame_rate: settings.frame_rate,
                    ..state.settings
                };
                let old = state.settings.frame_rate;
                let fps_sequence = self.apply_settings_locked(&mut state, settings)?;
                rollback = Some((old, settings.frame_rate, fps_sequence));
                request.payload = Some(PbRpcRequestPayload::EnterSuperScreen(PbEnterSuperScreen {
                    reason: 2,
                    resolution: Some(target),
                    dpi_scale: dpi,
                    enter_fps: Some(PbSuperScreenFps {
                        fps_count: i32::try_from(state.baseline.fps_count)?,
                    }),
                }));
            }
        }
        state.mouse.pause_layout();
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1);
        request.request_header = Some(PbRequestHeader {
            request_id: sequence,
        });
        state.topology = DisplayTopology {
            current: Some(Pending {
                sequence,
                generation: state.protocol_generation,
                report: state.screens_generation,
                report_at: None,
                action,
                before: state.screens.clone(),
                rollback,
            }),
            status: DisplayTopologyStatus {
                sequence: Some(sequence),
                origin_screen: origin,
                action: Some(action),
                pending: true,
                message: format!("正在{}…", action.label()),
                ..Default::default()
            },
        };
        let sent = self.outgoing.send(OutgoingControlMessage {
            annotation_generation: None,
            sequence,
            payload: encode_envelope(sequence, PbPayload::RpcRequest(request.encode_to_vec())),
            protocol: protocol(&state),
            completion: None,
        });
        if sent.is_err() {
            self.topology_send_failed(&mut state, sequence, "显示操作发送任务已停止");
            bail!("显示操作发送任务已停止");
        }
        tracing::info!(
            sequence,
            ?action,
            origin,
            "explicit display topology operation sent"
        );
        self.mouse.repaint();
        Ok(sequence)
    }

    pub(super) fn topology_send_failed(
        &self,
        state: &mut StreamControlState,
        sequence: i64,
        error: &str,
    ) -> bool {
        if !state
            .topology
            .current
            .as_ref()
            .is_some_and(|p| p.sequence == sequence)
        {
            return false;
        }
        state.topology.status.pending = false;
        state.topology.status.error = true;
        state.topology.status.message = error.into();
        self.rollback_topology_fps(state);
        true
    }

    fn rollback_topology_fps(&self, state: &mut StreamControlState) {
        let rollback = state
            .topology
            .current
            .as_mut()
            .and_then(|p| p.rollback.take());
        if let Some((old, requested, sequence)) = rollback
            && state.settings.frame_rate == requested
            && state.latest_requested_sequence == Some(sequence)
        {
            let mut restored = state.settings;
            restored.frame_rate = old;
            if let Err(error) = self.apply_settings_locked(state, restored) {
                tracing::warn!(%error, "restore FPS after super-screen failure failed");
            }
        }
    }

    pub(super) fn handle_topology_response(
        &self,
        state: &mut StreamControlState,
        sequence: i64,
        payload: Option<&PbRpcResponsePayload>,
    ) -> bool {
        let Some(pending) = &state.topology.current else {
            return false;
        };
        if pending.sequence != sequence || pending.generation != state.protocol_generation {
            return false;
        }
        let mut reported_color = None;
        let code = match (pending.action, payload) {
            (
                DisplayTopologyAction::Create { .. },
                Some(PbRpcResponsePayload::CreateVirtualDisplayRsp(bytes)),
            )
            | (
                DisplayTopologyAction::Remove { .. },
                Some(PbRpcResponsePayload::RemoveVirtualDisplayRsp(bytes)),
            )
            | (DisplayTopologyAction::Exit, Some(PbRpcResponsePayload::QuitSuperScreen(bytes)))
            | (
                DisplayTopologyAction::Enter { .. } | DisplayTopologyAction::FrameRate { .. },
                Some(PbRpcResponsePayload::EnterSuperScreenRep(bytes)),
            ) => PbDisplayResult::decode(bytes.as_slice())
                .map(|r| r.error_code)
                .map_err(|_| "显示操作回应无法解析".to_owned()),
            (
                DisplayTopologyAction::Resolution { .. },
                Some(PbRpcResponsePayload::CaptureSetting(result)),
            ) => {
                // Only this confirmed conversion registers the special -5/-6 result path.
                let mut error = None;
                for item in &result.errors {
                    if item.error_code == -5 {
                        match serde_json::from_str::<serde_json::Value>(&item.error_detail)
                            .ok()
                            .and_then(|v| v.get("error_code").and_then(|x| x.as_i64()))
                        {
                            Some(0) => {}
                            Some(code) => error = Some(format!("超级屏转换未完成（{code}）")),
                            None => error = Some("超级屏转换回应缺少有效结果".into()),
                        }
                    } else if item.error_code == -6 {
                        tracing::info!(sequence, detail = %item.error_detail, "super-screen conversion chroma result");
                        reported_color =
                            serde_json::from_str::<serde_json::Value>(&item.error_detail)
                                .ok()
                                .and_then(|v| v.get("error_code").and_then(|c| c.as_i64()))
                                .filter(|code| (0..=5).contains(code))
                                .map(|code| matches!(code, 0 | 3 | 4));
                    }
                    // Ordinary -2/-3 do not become a new recovery/retry policy here.
                }
                error.map_or(Ok(0), Err)
            }
            _ => return false,
        };
        if let Some(enabled) = reported_color
            && state
                .latest_requested_sequence
                .is_none_or(|latest| sequence >= latest)
        {
            apply_reported_color(state, enabled, true);
        }
        match code {
            Ok(0) => {
                if let Some(pending) = state.topology.current.as_mut() {
                    pending.rollback = None;
                }
                if !state.topology.status.reported {
                    state.topology.status.message = "请求已受理，等待显示状态更新…".into();
                }
            }
            error => {
                let message = match error {
                    Ok(code) => format!("显示操作未完成（{code}），请以实际显示状态为准"),
                    Err(message) => message,
                };
                self.topology_send_failed(state, sequence, &message);
            }
        }
        true
    }
}

fn resolution(width: u32, height: u32) -> Result<PbScreenResolution> {
    anyhow::ensure!(width > 0 && height > 0, "显示尺寸无效");
    Ok(PbScreenResolution {
        width: i32::try_from(width)?,
        height: i32::try_from(height)?,
    })
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbScreenResolution {
    #[prost(int32, tag = "1")]
    pub(super) width: i32,
    #[prost(int32, tag = "2")]
    pub(super) height: i32,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbCreateVirtualDisplay {
    #[prost(message, repeated, tag = "1")]
    pub(super) local_resolution: Vec<PbScreenResolution>,
    #[prost(int32, tag = "2")]
    pub(super) virtual_display_count: i32,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbRemoveVirtualDisplay {
    #[prost(int32, repeated, tag = "1")]
    pub(super) screen_id: Vec<i32>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbQuitSuperScreen {}
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbEnterSuperScreen {
    #[prost(int32, tag = "1")]
    pub(super) reason: i32,
    #[prost(message, optional, tag = "2")]
    pub(super) resolution: Option<PbScreenResolution>,
    #[prost(int32, tag = "3")]
    pub(super) dpi_scale: i32,
    #[prost(message, optional, tag = "4")]
    pub(super) enter_fps: Option<PbSuperScreenFps>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbSuperScreenFps {
    #[prost(int32, tag = "1")]
    pub(super) fps_count: i32,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbDisplayResult {
    #[prost(int32, tag = "1")]
    pub(super) error_code: i32,
}
