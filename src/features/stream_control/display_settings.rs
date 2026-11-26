//! The host's displays as the controller sees them (SESSION `Displays`) and
//! per-display mode and scale changes. A request is settled by its answer,
//! which carries the display list it left.
use super::*;
use super::wire::{DisplayKind, DisplayOp, DisplayRequest, SessionBody};
use anyhow::Context;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteDisplayMode {
    pub width: u32,
    pub height: u32,
}

impl RemoteDisplayMode {
    fn of(size: &wire::Size) -> Option<Self> {
        (size.width > 0 && size.height > 0).then_some(Self {
            width: size.width,
            height: size.height,
        })
    }
    pub fn label(self) -> String {
        format!("{} × {}", self.width, self.height)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemoteDisplayInfo {
    pub kind: DisplayKind,
    pub preferred: Option<(u32, u32)>,
    pub current: RemoteDisplayMode,
    /// The mode before the session changed it; `None` while it has not.
    pub initial: Option<RemoteDisplayMode>,
    pub modes: Vec<RemoteDisplayMode>,
    pub current_dpi: u32,
    pub recommended_dpi: u32,
    pub dpis: Vec<u32>,
    pub hdr: bool,
    /// How the viewer turns the picture (a camera's mirror and rotation).
    pub orientation: crate::media::geometry::Orientation,
}

impl RemoteDisplayInfo {
    pub fn is_camera(&self) -> bool {
        self.kind == DisplayKind::Camera
    }
    fn of(display: &wire::Display) -> Option<Self> {
        let current = RemoteDisplayMode::of(&wire::Size {
            width: display.width,
            height: display.height,
        })?;
        let mut modes: Vec<_> = display.modes.iter().filter_map(RemoteDisplayMode::of).collect();
        modes.sort_by_key(|m| (m.width, m.height));
        modes.dedup();
        let mut dpis: Vec<_> = display.scales.iter().copied().filter(|v| *v > 0).collect();
        dpis.sort_unstable();
        dpis.dedup();
        Some(Self {
            kind: display.kind(),
            preferred: display
                .preferred
                .as_ref()
                .filter(|g| (1..=16384).contains(&g.width) && (1..=16384).contains(&g.height))
                .map(|g| (g.width, g.height)),
            current,
            initial: display.initial.as_ref().and_then(RemoteDisplayMode::of),
            modes,
            current_dpi: display.scale,
            recommended_dpi: display.recommended_scale,
            dpis,
            hdr: display.hdr,
            orientation: crate::media::geometry::Orientation {
                mirror: display.mirror,
                quarter_turns: (display.quarter_turns % 4) as u8,
            },
        })
    }
    /// Any size can be asked of it (a virtual display); others take their
    /// listed modes.
    pub fn resizable(&self) -> bool {
        matches!(self.kind, DisplayKind::Virtual | DisplayKind::Exclusive)
    }
    /// The mode `choice` asks for.
    pub(crate) fn mode_of(&self, choice: DisplayResolution) -> Option<RemoteDisplayMode> {
        match choice {
            DisplayResolution::Mode(mode) => Some(mode),
            DisplayResolution::Initial => self.initial,
            DisplayResolution::FollowLocal { width, height } => {
                Some(RemoteDisplayMode { width, height })
            }
        }
    }
    /// Whether the display can show `choice` itself (without a super
    /// screen).
    pub fn shows(&self, choice: DisplayResolution) -> bool {
        self.mode_of(choice)
            .is_some_and(|mode| self.resizable() || self.modes.contains(&mode))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayResolution {
    Mode(RemoteDisplayMode),
    Initial,
    FollowLocal { width: u32, height: u32 },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisplayChangeRequest {
    pub resolution: Option<DisplayResolution>,
    // An explicit percentage, including the menu's currently recommended percentage.
    pub dpi: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct DisplayChangeStatus {
    pub pending: bool,
    pub message: String,
    pub error: bool,
    pub requested_resolution: Option<RemoteDisplayMode>,
    pub requested_resolution_choice: Option<DisplayResolution>,
    pub requested_dpi: Option<u32>,
}

#[derive(Default)]
pub(super) struct DisplayChanges {
    pub(super) status: BTreeMap<i32, DisplayChangeStatus>,
}

impl DisplayChanges {
    /// The host answered the change of display `id`.
    pub(super) fn settle(&mut self, id: i32, error: Option<String>) {
        let Some(status) = self.status.get_mut(&id) else {
            return;
        };
        if let Some(error) = &error {
            tracing::warn!(screen_id = id, %error, "display change refused");
        }
        status.pending = false;
        status.error = error.is_some();
        status.message = error.unwrap_or_else(|| t().features.stream_display_applied.into());
    }
    pub(super) fn cancel_all(&mut self, reason: &str) {
        for status in self.status.values_mut().filter(|s| s.pending) {
            status.pending = false;
            status.error = true;
            status.message = reason.into();
        }
    }
    /// A later list that no longer shows a settled change says so.
    fn observe(&mut self, screens: &[ScreenBaseline]) {
        self.status
            .retain(|id, _| screens.iter().any(|s| s.id == *id));
        for (id, status) in &mut self.status {
            if status.pending || status.error {
                continue;
            }
            let Some(screen) = screens.iter().find(|s| s.id == *id) else {
                continue;
            };
            let matches = status
                .requested_resolution
                .is_none_or(|mode| mode == screen.display.current)
                && status
                    .requested_dpi
                    .is_none_or(|dpi| dpi == screen.display.current_dpi);
            status.message = if matches {
                t().features.stream_display_applied
            } else {
                t().features.stream_display_changed_remotely
            }
            .into();
        }
    }
}

/// A refused display request in the interface language, from its reason.
pub(super) fn refusal_text(reason: &str) -> String {
    use super::wire::display_reason as r;
    let text = &t().features;
    match reason {
        r::NOT_ALLOWED => text.stream_display_change_not_allowed,
        r::UPDATING => text.stream_host_updating_settings_locked,
        r::UNSUPPORTED_MODE => text.stream_resolution_needs_super_screen,
        r::NO_FREE_TRACK => text.stream_no_video_track,
        r::LIMIT => text.stream_display_limit,
        r::NOT_REMOVABLE => text.stream_only_farwing_virtual_screens,
        r::EXCLUSIVE => text.stream_unavailable_in_super_screen,
        r::NOT_FOUND => text.stream_display_unavailable,
        r::INVALID => text.stream_invalid_display_size,
        _ => text.stream_display_operation_failed,
    }
    .into()
}

/// The host's displays can be changed in this session: its own kind of
/// device, and the `display` capability in effect.
pub(super) fn supported(state: &StreamControlState) -> bool {
    state.features.as_ref().is_some_and(|p| p.supports(crate::account::feature_ability::Feature::Host))
        && negotiated(state)
        && state
            .grants
            .as_ref()
            .is_some_and(|grants| grants.iter().any(|cap| cap == farwing_protocol::caps::DISPLAY))
}

/// How long a display request may take on the host (adding a display
/// waits for Windows to bring it up).
pub(super) const DISPLAY_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

impl StreamControlHandle {
    /// Apply a push or request answer, ignoring lists older than the current one.
    pub(super) fn apply_displays(&self, state: &mut StreamControlState, list: wire::Displays) {
        if list.revision < state.displays_revision {
            return;
        }
        state.displays_revision = list.revision;
        let previous_screens = std::mem::take(&mut state.screens);
        state.screens_generation = state.screens_generation.wrapping_add(1);
        state.current_screen_id = list.active;
        state.screens = list
            .displays
            .into_iter()
            .filter_map(|display| {
                Some(ScreenBaseline {
                    display: RemoteDisplayInfo::of(&display)?,
                    id: display.id,
                    name: display.name,
                    primary: display.primary,
                    video_track_index: display.track,
                    fps: display.refresh_hz,
                    width: display.width,
                    height: display.height,
                })
            })
            .collect();
        if previous_screens.iter().any(|old| {
            !state.screens.iter().any(|new| {
                old.id == new.id
                    && old.video_track_index == new.video_track_index
                    && old.display.kind == new.display.kind
            })
        }) {
            state.mouse.pause_layout();
        }
        for screen in &state.screens {
            tracing::debug!(screen_id = screen.id, name = %screen.name,
                kind = ?screen.display.kind, primary = screen.primary,
                track = screen.video_track_index, width = screen.width, height = screen.height,
                preferred = ?screen.display.preferred, "remote screen mapping");
        }
        state.topology.observe(&state.screens);
        state.display_changes.observe(&state.screens);
        super::settings::refresh_active_screen(state);
        self.protocol_changed.notify_waiters();
    }

    pub(crate) fn display_change_pending(&self, screen_id: i32) -> bool {
        lock(&self.shared)
            .display_changes
            .status
            .get(&screen_id)
            .is_some_and(|s| s.pending)
    }

    pub(crate) fn pending_display_resolution(&self, screen_id: i32) -> Option<RemoteDisplayMode> {
        let state = lock(&self.shared);
        state
            .display_changes
            .status
            .get(&screen_id)
            .filter(|s| s.pending)?
            .requested_resolution
    }

    /// Stop waiting locally. An already sent change is not rolled back.
    pub(crate) fn cancel_display_change(&self, screen_id: i32) {
        let mut state = lock(&self.shared);
        if let Some(status) = state
            .display_changes
            .status
            .get_mut(&screen_id)
            .filter(|s| s.pending)
        {
            status.pending = false;
            status.error = false;
            status.message = t().features.stream_display_wait_stopped.into();
        }
    }

    pub fn apply_display_change(
        &self,
        screen_id: i32,
        request: DisplayChangeRequest,
    ) -> Result<()> {
        let mut state = lock(&self.shared);
        ensure_ready(&state)?;
        anyhow::ensure!(
            supported(&state),
            "{}",
            t().features.stream_display_windows_only
        );
        let screen = state
            .screens
            .iter()
            .find(|s| s.id == screen_id)
            .context(t().features.stream_display_disconnected)?;
        // Tab menus address the clicked screen directly, without
        // selecting it or starting a capture track first.
        anyhow::ensure!(
            screen_id == state.current_screen_id
                || feature_supported(
                    &state,
                    crate::account::feature_ability::Feature::Host
                ),
            "{}",
            t().features.stream_multi_display_settings_unavailable
        );
        let op = match (request.resolution, request.dpi) {
            (Some(choice), None) => {
                let mode = screen
                    .display
                    .mode_of(choice)
                    .context(t().features.stream_no_initial_resolution)?;
                anyhow::ensure!(
                    screen.display.shows(choice),
                    "{}",
                    t().features.stream_resolution_needs_super_screen
                );
                DisplayOp::SetMode(wire::SetMode {
                    id: screen_id,
                    width: mode.width,
                    height: mode.height,
                })
            }
            (None, Some(dpi)) => {
                anyhow::ensure!(
                    state.peer.has(crate::protocol::peer::feature::DISPLAY_CONFIG),
                    "{}",
                    t().features.stream_dpi_settings_unavailable
                );
                anyhow::ensure!(
                    screen.display.dpis.contains(&dpi),
                    "{}",
                    t().features.stream_dpi_unsupported
                );
                DisplayOp::SetScale(wire::SetScale {
                    id: screen_id,
                    scale: dpi,
                })
            }
            (None, None) => bail!("{}", t().features.stream_display_nothing_selected),
            (Some(_), Some(_)) => bail!("{}", t().features.stream_resolution_and_dpi_together),
        };
        let requested_resolution = match &op {
            DisplayOp::SetMode(mode) => Some(RemoteDisplayMode {
                width: mode.width,
                height: mode.height,
            }),
            _ => None,
        };
        tracing::info!(screen_id, ?op, "display setting requested");
        self.send_request(
            &mut state,
            SessionBody::Display(DisplayRequest { op: Some(op) }),
            super::requests::Waiting::Display(screen_id),
            DISPLAY_REQUEST_TIMEOUT,
        )?;
        state.display_changes.status.insert(
            screen_id,
            DisplayChangeStatus {
                pending: true,
                message: if requested_resolution.is_some() {
                    t().features.stream_changing_resolution
                } else {
                    t().features.stream_changing_dpi
                }
                .into(),
                error: false,
                requested_resolution,
                requested_resolution_choice: request.resolution,
                requested_dpi: request.dpi,
            },
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(kind: DisplayKind) -> RemoteDisplayInfo {
        let mode = |width, height| RemoteDisplayMode { width, height };
        RemoteDisplayInfo {
            kind,
            preferred: None,
            current: mode(1920, 1080),
            initial: Some(mode(3840, 2160)),
            modes: vec![mode(1920, 1080), mode(3840, 2160)],
            current_dpi: 100,
            recommended_dpi: 150,
            dpis: vec![100, 150],
            hdr: false,
            orientation: Default::default(),
        }
    }

    #[test]
    fn only_virtual_displays_take_unlisted_sizes() {
        let local = DisplayResolution::FollowLocal {
            width: 2560,
            height: 1600,
        };
        assert!(!info(DisplayKind::Physical).shows(local));
        assert!(info(DisplayKind::Physical).shows(DisplayResolution::Initial));
        assert!(info(DisplayKind::Virtual).shows(local));
        assert!(info(DisplayKind::Exclusive).shows(local));
        assert!(!info(DisplayKind::Fallback).shows(local));
    }

    #[test]
    fn pushed_display_list_wakes_playback() {
        use std::task::{Context, Poll, Waker};

        let profile = crate::media::ConnectionMediaOptions::default()
            .resolve(LocalDisplayInfo::FALLBACK)
            .unwrap();
        let (control, _, _) = StreamControlHandle::new(profile, PerformanceMonitor::new(""));
        let changed = control.protocol_notifications();
        let mut handshake = std::pin::pin!(changed.notified());
        let mut playback = std::pin::pin!(changed.notified());
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(handshake.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(playback.as_mut().poll(&mut cx), Poll::Pending);
        assert!(control.snapshot().screens.is_empty());

        control
            .handle_protocol_message(&wire::push(SessionBody::Displays(wire::Displays {
                revision: 1,
                active: 7,
                displays: vec![wire::Display {
                    id: 7,
                    primary: true,
                    width: 1920,
                    height: 1080,
                    ..Default::default()
                }],
            })))
            .unwrap();

        assert_eq!(control.snapshot().screens[0].id, 7);
        assert_eq!(playback.as_mut().poll(&mut cx), Poll::Ready(()));
    }
}
