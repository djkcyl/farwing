//! Explicit remote display topology actions; ordinary capture settings remain independent.
use super::requests::Waiting;
use super::wire::{DisplayKind, DisplayOp, DisplayRequest, SessionBody};
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
    /// A mode the display cannot show itself: a super screen of that size.
    Resolution {
        screen_id: i32,
        choice: DisplayResolution,
    },
    /// A frame rate above the display's refresh rate: the rate, and a super
    /// screen of this size.
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
            Self::Create { .. } => t().features.stream_add_virtual_screen,
            Self::Remove { .. } => t().features.stream_remove_virtual_screen,
            Self::Exit => t().features.stream_exit_super_screen,
            _ => t().features.stream_switch_super_screen,
        }
    }
    fn progress(self) -> &'static str {
        match self {
            Self::Create { .. } => t().features.stream_adding_virtual_screen,
            Self::Remove { .. } => t().features.stream_removing_virtual_screen,
            Self::Exit => t().features.stream_exiting_super_screen,
            _ => t().features.stream_switching_super_screen,
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
}

#[derive(Clone, Debug, Default)]
pub struct DisplayTopologyStatus {
    /// Names the operation (a new one, a new number).
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
    action: DisplayTopologyAction,
    before: Vec<ScreenBaseline>,
    /// The host carried it out; the next lists show its effect.
    answered: bool,
    report_at: Option<Instant>,
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
            self.status.message = t().features.stream_disconnected.into();
        }
    }

    /// The display the carried-out action leads to, once a list shows it.
    pub fn observe(&mut self, screens: &[ScreenBaseline]) {
        let Some(pending) = &mut self.current else {
            return;
        };
        if !pending.answered {
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
        let primary = || screens.iter().find(|s| s.primary).or(screens.first()).map(|s| s.id);
        let exclusive = screens
            .iter()
            .find(|s| s.display.kind == DisplayKind::Exclusive)
            .map(|s| s.id);
        let selected = match pending.action {
            DisplayTopologyAction::Create { .. } => screens
                .iter()
                .find(|s| {
                    s.display.kind == DisplayKind::Virtual
                        && !pending.before.iter().any(|old| old.id == s.id)
                })
                .map(|s| s.id),
            DisplayTopologyAction::Remove { screen_id } => {
                (!screens.iter().any(|s| s.id == screen_id)).then(primary).flatten()
            }
            DisplayTopologyAction::Exit => exclusive.is_none().then(primary).flatten(),
            _ => exclusive,
        };
        let Some(id) = selected else {
            return;
        };
        if matches!(pending.action, DisplayTopologyAction::Create { .. }) {
            // Bridge one creation report, not normal hot-unplug events.
            self.status.transient_screens = pending
                .before
                .iter()
                .filter(|old| {
                    old.display.kind == DisplayKind::Physical
                        && !screens.iter().any(|s| s.id == old.id)
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
            self.status.message = if matches!(pending.action, DisplayTopologyAction::Remove { .. }) {
                t().features.stream_display_removed
            } else {
                t().features.stream_restoring_video
            }
            .into();
        }
    }
}

pub(super) fn support(state: &StreamControlState) -> DisplayTopologySupport {
    let available = display_settings::supported(state)
        && state.peer.has(crate::protocol::peer::feature::DISPLAY_VIRTUAL);
    let super_screen = state
        .screens
        .iter()
        .any(|s| s.display.kind == DisplayKind::Exclusive);
    let create_unavailable = if !available {
        Some(t().features.stream_virtual_screens_unavailable)
    } else if super_screen {
        Some(t().features.stream_unavailable_in_super_screen)
    } else if state.screens.len() >= 5 {
        Some(t().features.stream_display_limit)
    } else if state
        .screens
        .iter()
        .filter(|s| s.display.kind == DisplayKind::Virtual)
        .count()
        >= 3
    {
        Some(t().features.stream_virtual_screen_limit)
    } else if state
        .screens
        .iter()
        .filter(|s| s.video_track_index >= 0)
        .count()
        >= crate::features::host::peer::screens::TRACK_COUNT
    {
        Some(t().features.stream_no_video_track)
    } else {
        None
    };
    DisplayTopologySupport {
        create: create_unavailable.is_none(),
        create_visible: display_settings::supported(state) && !super_screen,
        create_unavailable,
        enter: available && !super_screen && feature_supported(state, Feature::Host),
        exit: available && super_screen,
        resolution_conversion: available,
        fps_conversion: available && !super_screen && feature_supported(state, Feature::Host),
    }
}

impl StreamControlHandle {
    pub(crate) fn display_input_blocked(&self, track: i32, binding: u64) -> bool {
        let state = lock(&self.shared);
        state.topology.status.blocks(binding as u32 as i32)
            || state.topology.status.error && !state.topology.status.dismissed
            || !state.screens.iter().any(|screen| {
                screen.video_track_index == track && screen.binding() == binding
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
            state.topology.status.message = t().features.stream_video_restored.into();
            tracing::info!(screen_id, "display topology presentation resumed");
            drop(state);
            self.mouse.repaint();
        }
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
    pub fn apply_display_topology(&self, origin: i32, action: DisplayTopologyAction) -> Result<()> {
        let mut state = lock(&self.shared);
        ensure_ready(&state)?;
        let capabilities = support(&state);
        anyhow::ensure!(
            capabilities.resolution_conversion,
            "{}",
            t().features.stream_virtual_displays_unavailable
        );
        anyhow::ensure!(
            !state.topology.status.pending || state.topology.status.dismissed,
            "{}",
            t().features.stream_display_operation_pending
        );
        anyhow::ensure!(
            state.screens.iter().any(|s| s.id == origin),
            "{}",
            t().features.stream_origin_display_disconnected
        );
        let exclusive = |width: u32, height: u32, scale: u32| {
            anyhow::ensure!(
                width > 0 && height > 0,
                "{}",
                t().features.stream_invalid_display_size
            );
            Ok(DisplayOp::EnterExclusive(wire::AddVirtual {
                width,
                height,
                scale,
            }))
        };
        let mut rollback = None;
        let op = match action {
            DisplayTopologyAction::Create { width, height } => {
                anyhow::ensure!(
                    capabilities.create,
                    "{}",
                    capabilities
                        .create_unavailable
                        .unwrap_or(t().features.stream_cannot_add_virtual_screen)
                );
                anyhow::ensure!(
                    width > 0 && height > 0,
                    "{}",
                    t().features.stream_invalid_display_size
                );
                DisplayOp::AddVirtual(wire::AddVirtual {
                    width,
                    height,
                    scale: 0,
                })
            }
            DisplayTopologyAction::Remove { screen_id } => {
                anyhow::ensure!(
                    state
                        .screens
                        .iter()
                        .any(|s| s.id == screen_id && s.display.kind == DisplayKind::Virtual),
                    "{}",
                    t().features.stream_only_farwing_virtual_screens
                );
                anyhow::ensure!(
                    state.screens.len() > 1,
                    "{}",
                    t().features.stream_cannot_remove_last_display
                );
                DisplayOp::Remove(wire::RemoveDisplay { id: screen_id })
            }
            DisplayTopologyAction::Enter { width, height, dpi } => {
                anyhow::ensure!(
                    capabilities.enter,
                    "{}",
                    t().features.stream_cannot_enter_super_screen
                );
                exclusive(width, height, dpi)?
            }
            DisplayTopologyAction::Exit => {
                anyhow::ensure!(
                    capabilities.exit,
                    "{}",
                    t().features.stream_not_in_super_screen
                );
                DisplayOp::ExitExclusive(wire::ExitExclusive {})
            }
            DisplayTopologyAction::Resolution { screen_id, choice } => {
                let mode = state
                    .screens
                    .iter()
                    .find(|s| s.id == screen_id)
                    .ok_or_else(|| anyhow!("{}", t().features.stream_display_disconnected))?
                    .display
                    .mode_of(choice)
                    .ok_or_else(|| anyhow!("{}", t().features.stream_no_initial_resolution))?;
                exclusive(mode.width, mode.height, 0)?
            }
            DisplayTopologyAction::FrameRate {
                settings,
                width,
                height,
                dpi,
            } => {
                anyhow::ensure!(
                    capabilities.fps_conversion,
                    "{}",
                    t().features.stream_cannot_switch_super_screen_by_fps
                );
                let op = exclusive(width, height, dpi)?;
                // Confirmation may remain open while another window changes quality.
                // This action owns FPS only, never a stale full settings snapshot.
                let settings = StreamControlSettings {
                    frame_rate: settings.frame_rate,
                    ..state.settings
                };
                let old = state.settings.frame_rate;
                let fps_sequence = self.apply_settings_locked(&mut state, settings)?;
                rollback = Some((old, settings.frame_rate, fps_sequence));
                op
            }
        };
        state.mouse.pause_layout();
        let sequence = self.send_request(
            &mut state,
            SessionBody::Display(DisplayRequest { op: Some(op) }),
            Waiting::Topology,
            display_settings::DISPLAY_REQUEST_TIMEOUT,
        )?;
        state.topology = DisplayTopology {
            current: Some(Pending {
                action,
                before: state.screens.clone(),
                answered: false,
                report_at: None,
                rollback,
            }),
            status: DisplayTopologyStatus {
                sequence: Some(sequence),
                origin_screen: origin,
                action: Some(action),
                pending: true,
                message: action.progress().into(),
                ..Default::default()
            },
        };
        tracing::info!(?action, origin, "explicit display topology operation sent");
        self.mouse.repaint();
        Ok(())
    }

    /// The host answered the topology operation.
    pub(super) fn topology_settled(
        &self,
        state: &mut StreamControlState,
        result: std::result::Result<(), String>,
    ) {
        let Some(pending) = state.topology.current.as_mut() else {
            return;
        };
        match result {
            Ok(()) => {
                pending.answered = true;
                pending.rollback = None;
                if !state.topology.status.reported {
                    state.topology.status.message =
                        t().features.stream_waiting_display_state.into();
                }
                let screens = state.screens.clone();
                state.topology.observe(&screens);
            }
            Err(error) => {
                state.topology.status.pending = false;
                state.topology.status.error = true;
                state.topology.status.message = error;
                self.rollback_topology_fps(state);
            }
        }
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
}
