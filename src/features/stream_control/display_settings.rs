//! Per-screen display mutations. RPC acceptance and observed mode are independent.
use super::*;
use anyhow::Context;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteDisplayMode {
    pub width: u32,
    pub height: u32,
}

impl RemoteDisplayMode {
    fn parse(rect: &PbWinRect) -> Option<Self> {
        Some(Self {
            width: u32::try_from(rect.width).ok().filter(|v| *v > 0)?,
            height: u32::try_from(rect.height).ok().filter(|v| *v > 0)?,
        })
    }
    pub fn label(self) -> String {
        format!("{} × {}", self.width, self.height)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemoteDisplayInfo {
    pub preferred: Option<(u32, u32)>,
    pub current: RemoteDisplayMode,
    pub initial: Option<RemoteDisplayMode>,
    pub modes: Vec<RemoteDisplayMode>,
    pub current_dpi: u32,
    pub recommended_dpi: u32,
    pub dpis: Vec<u32>,
    pub screen_type: i32,
    pub resolution_type: i32,
    pub left: i32,
    pub top: i32,
}

impl RemoteDisplayInfo {
    pub(super) fn from_screen(screen: &PbScreen) -> Option<Self> {
        let rect = screen.current_resolution.as_ref()?;
        let mut modes: Vec<_> = screen
            .resolutions
            .iter()
            .filter_map(RemoteDisplayMode::parse)
            .collect();
        modes.sort_by_key(|m| (m.width, m.height));
        modes.dedup();
        let dpi = screen.dpi_scale.as_ref();
        let mut dpis: Vec<_> = dpi
            .into_iter()
            .flat_map(|d| &d.dpis)
            .filter_map(|v| u32::try_from(*v).ok().filter(|v| *v > 0))
            .collect();
        dpis.sort_unstable();
        dpis.dedup();
        Some(Self {
            preferred: screen
                .display_geometry
                .as_ref()
                .filter(|g| (1..=16384).contains(&g.width) && (1..=16384).contains(&g.height))
                .map(|g| (g.width, g.height)),
            current: RemoteDisplayMode::parse(rect)?,
            initial: screen
                .init_resolution
                .as_ref()
                .and_then(RemoteDisplayMode::parse),
            modes,
            current_dpi: dpi
                .and_then(|d| u32::try_from(d.current_dpi).ok())
                .unwrap_or(0),
            recommended_dpi: dpi
                .and_then(|d| u32::try_from(d.recommended_dpi).ok())
                .unwrap_or(0),
            dpis,
            screen_type: screen.screen_type,
            resolution_type: screen.resolution_type,
            left: rect.left,
            top: rect.top,
        })
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

#[derive(Clone, Copy)]
enum Step {
    Resolution(RemoteDisplayMode, i32),
    Dpi(u32),
}

/// A display request that neither failed nor produced a matching screen
/// report within this time is ended, so the transition overlay never sticks.
const DISPLAY_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

struct Pending {
    sent: std::time::Instant,
    generation: u64,
    sequence: i64,
    screens_generation: u64,
    step: Step,
    target_mode: Option<RemoteDisplayMode>,
    target_choice: Option<DisplayResolution>,
    target_dpi: Option<u32>,
}

struct Observation {
    generation: u64,
    last_report: u64,
    sequence: i64,
    resolution: Option<RemoteDisplayMode>,
    dpi: Option<u32>,
    reason: String,
}

#[derive(Default)]
pub(super) struct DisplayChanges {
    pending: BTreeMap<i32, Pending>,
    observations: BTreeMap<i32, Observation>,
    report_generation: u64,
    pub(super) status: BTreeMap<i32, DisplayChangeStatus>,
}

impl DisplayChanges {
    fn finish(&mut self, id: i32, pending: &Pending, error: Option<String>) {
        let message = match &error {
            Some(error) => {
                let reason = error.clone();
                self.observations.insert(
                    id,
                    Observation {
                        generation: pending.generation,
                        last_report: self.report_generation,
                        sequence: pending.sequence,
                        resolution: pending.target_mode,
                        dpi: pending.target_dpi,
                        reason: reason.clone(),
                    },
                );
                tracing::warn!(screen_id = id, sequence = pending.sequence, %reason,
                    "display request ended without full confirmation; observing later reports");
                reason
            }
            None => {
                self.observations.remove(&id);
                t().features.stream_display_applied.into()
            }
        };
        self.status.insert(
            id,
            DisplayChangeStatus {
                pending: false,
                message,
                error: error.is_some(),
                requested_resolution: pending.target_mode,
                requested_resolution_choice: pending.target_choice,
                requested_dpi: pending.target_dpi,
            },
        );
    }

    // Failed/cancelled operations may have changed the desktop before failing.
    // Observe subsequent facts without reviving an RPC or sending its next stage.
    fn observe_reports(&mut self, generation: u64, report: u64, screens: &[ScreenBaseline]) {
        self.observations
            .retain(|id, o| o.generation == generation && screens.iter().any(|s| s.id == *id));
        for (id, observation) in &mut self.observations {
            if report <= observation.last_report {
                continue;
            }
            let Some(screen) = screens.iter().find(|s| s.id == *id) else {
                continue;
            };
            observation.last_report = report;
            let mode_matches = observation
                .resolution
                .is_none_or(|mode| mode == screen.display.current);
            let dpi_matches = observation
                .dpi
                .is_none_or(|dpi| dpi == screen.display.current_dpi);
            let actual = match (mode_matches, dpi_matches) {
                (true, true) => t().features.stream_display_reached,
                (true, false) if observation.resolution.is_some() => {
                    t().features.stream_display_dpi_not_reached
                }
                _ => t().features.stream_display_not_fully_reached,
            };
            if let Some(status) = self.status.get_mut(id) {
                let message = (t().features.stream_display_result)(&observation.reason, actual);
                if status.message != message {
                    tracing::info!(
                        screen_id = *id,
                        sequence = observation.sequence,
                        width = screen.width,
                        height = screen.height,
                        dpi = screen.dpi_scale,
                        "late screen state reconciled after display request failure"
                    );
                }
                status.message = message;
            }
        }
    }
    fn refresh_completed(&mut self, screens: &[ScreenBaseline]) {
        for (id, status) in &mut self.status {
            if status.pending || status.error || self.observations.contains_key(id) {
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
            let message = if matches {
                t().features.stream_display_applied
            } else {
                t().features.stream_display_changed_remotely
            };
            if status.message != message {
                status.message = message.into();
            }
        }
    }

    pub(super) fn cancel_all(&mut self, reason: &str) {
        for (id, pending) in std::mem::take(&mut self.pending) {
            self.finish(id, &pending, Some(reason.into()));
        }
    }
    pub(super) fn send_failed(&mut self, sequence: i64, reason: &str) -> bool {
        let Some(id) = self
            .pending
            .iter()
            .find(|(_, p)| p.sequence == sequence)
            .map(|(id, _)| *id)
        else {
            return false;
        };
        let pending = self.pending.remove(&id).expect("matched display operation");
        self.finish(id, &pending, Some((t().features.stream_send_failed)(reason)));
        true
    }
    pub(super) fn ack(&mut self, sequence: i64, response: &PbCaptureSettingResponse) -> bool {
        let Some(id) = self
            .pending
            .iter()
            .find(|(_, p)| p.sequence == sequence)
            .map(|(id, _)| *id)
        else {
            return false;
        };
        // Success only says the host accepted the request; the screen report
        // still decides the observed state. A rejection ends the request now
        // with the host's reason instead of waiting for a report that the
        // host will never send.
        let failures = response
            .errors
            .iter()
            .filter(|error| !display_reply_succeeded(error))
            .map(|error| format_pb_error(error.clone()))
            .collect::<Vec<_>>();
        for error in &response.errors {
            tracing::debug!(sequence, code = error.error_code, detail = %format_pb_error(error.clone()),
                "display reply");
        }
        if !failures.is_empty() {
            let pending = self.pending.remove(&id).expect("matched display operation");
            self.finish(
                id,
                &pending,
                Some((t().features.stream_display_apply_failed)(
                    &failures.join(t().features.stream_separator),
                )),
            );
        }
        true
    }

    /// End requests whose host neither failed nor reported the new state. A
    /// target that already equals the last report needs no new report.
    pub(super) fn expire(&mut self, screens: &[ScreenBaseline]) -> bool {
        let expired = self
            .pending
            .iter()
            .filter(|(_, p)| p.sent.elapsed() >= DISPLAY_REQUEST_TIMEOUT)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in &expired {
            let pending = self.pending.remove(id).expect("expired display operation");
            let reached = screens.iter().find(|s| s.id == *id).is_some_and(|screen| {
                match pending.step {
                    Step::Resolution(mode, _) => screen.display.current == mode,
                    Step::Dpi(dpi) => screen.display.current_dpi == dpi,
                }
            });
            let error =
                (!reached).then(|| t().features.stream_display_report_timeout.to_owned());
            self.finish(*id, &pending, error);
        }
        !expired.is_empty()
    }
}

pub(super) fn supported(state: &StreamControlState) -> bool {
    state.features.as_ref().is_some_and(|p| p.supports(crate::account::feature_ability::Feature::Host))
        && protocol(state) == StreamControlProtocol::CaptureSetting
}

fn resolve(
    info: &RemoteDisplayInfo,
    choice: DisplayResolution,
) -> Result<(RemoteDisplayMode, i32)> {
    let (mode, kind) = expand_resolution(info, choice)?;
    anyhow::ensure!(
        info.modes.contains(&mode),
        "{}",
        t().features.stream_resolution_needs_super_screen
    );
    Ok((mode, kind))
}

pub(super) fn expand_resolution(
    info: &RemoteDisplayInfo,
    choice: DisplayResolution,
) -> Result<(RemoteDisplayMode, i32)> {
    let (mode, kind) = match choice {
        DisplayResolution::Mode(mode) => (mode, 4),
        DisplayResolution::Initial => (
            info.initial
                .ok_or_else(|| anyhow!("{}", t().features.stream_no_initial_resolution))?,
            3,
        ),
        DisplayResolution::FollowLocal { width, height } => {
            let mode = info
                .modes
                .iter()
                .find(|m| m.width == width && m.height == height)
                .copied()
                .unwrap_or(RemoteDisplayMode { width, height });
            (mode, 2)
        }
    };
    Ok((mode, kind))
}

fn validate_dpi(state: &StreamControlState, info: &RemoteDisplayInfo, dpi: u32) -> Result<()> {
    anyhow::ensure!(
        state.peer.has(crate::protocol::peer::feature::DISPLAY_CONFIG),
        "{}",
        t().features.stream_dpi_settings_unavailable
    );
    anyhow::ensure!(
        info.dpis.contains(&dpi),
        "{}",
        t().features.stream_dpi_unsupported
    );
    Ok(())
}

impl StreamControlHandle {
    pub(crate) fn display_change_pending(&self, screen_id: i32) -> bool {
        lock(&self.shared)
            .display_changes
            .pending
            .contains_key(&screen_id)
    }

    pub(crate) fn pending_display_resolution(&self, screen_id: i32) -> Option<RemoteDisplayMode> {
        let state = lock(&self.shared);
        state.display_changes.pending.get(&screen_id)?.target_mode
    }

    /// Stop waiting locally. Already transmitted physical changes are not rolled back.
    pub(crate) fn cancel_display_change(&self, screen_id: i32) {
        let mut state = lock(&self.shared);
        if let Some(pending) = state.display_changes.pending.remove(&screen_id) {
            state.display_changes.finish(
                screen_id,
                &pending,
                Some(t().features.stream_display_wait_stopped.into()),
            );
            if let Some(status) = state.display_changes.status.get_mut(&screen_id) {
                status.error = false;
            }
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
        anyhow::ensure!(
            request.resolution.is_some() || request.dpi.is_some(),
            "{}",
            t().features.stream_display_nothing_selected
        );
        anyhow::ensure!(
            request.resolution.is_none() || request.dpi.is_none(),
            "{}",
            t().features.stream_resolution_and_dpi_together
        );
        let screen = state
            .screens
            .iter()
            .find(|s| s.id == screen_id && s.id >= 0)
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
        let resolution = request
            .resolution
            .map(|r| resolve(&screen.display, r))
            .transpose()?;
        if let Some(dpi) = request.dpi {
            validate_dpi(&state, &screen.display, dpi)?;
        }
        // An explicit choice always sends.
        // Comparing only a cached value could otherwise suppress a recovery.
        let step = match resolution {
            Some((mode, kind)) => Step::Resolution(mode, kind),
            None => Step::Dpi(request.dpi.expect("one display action required")),
        };
        let sequence = self.send_display_step(&mut state, screen_id, step)?;
        state.display_changes.observations.remove(&screen_id);
        let pending = Pending {
            sent: std::time::Instant::now(),
            generation: state.protocol_generation,
            sequence,
            screens_generation: state.screens_generation,
            step,
            target_mode: resolution.map(|(mode, _)| mode),
            target_choice: request.resolution,
            target_dpi: request.dpi,
        };
        if let Some(previous) = state.display_changes.pending.insert(screen_id, pending) {
            tracing::debug!(
                screen_id,
                previous_sequence = previous.sequence,
                sequence,
                "explicit display choice replaces the previous observed target"
            );
        }
        state.display_changes.status.insert(
            screen_id,
            DisplayChangeStatus {
                pending: true,
                message: match step {
                    Step::Resolution(..) => t().features.stream_changing_resolution,
                    Step::Dpi(_) => t().features.stream_changing_dpi,
                }
                .into(),
                error: false,
                requested_resolution: resolution.map(|(mode, _)| mode),
                requested_resolution_choice: request.resolution,
                requested_dpi: request.dpi,
            },
        );
        Ok(())
    }

    fn send_display_step(
        &self,
        state: &mut StreamControlState,
        id: i32,
        step: Step,
    ) -> Result<i64> {
        let mut request = capture_setting_request(state.baseline)?;
        request.screen_id = id;
        // Both ordinary actions start
        // with the current screen rectangle, including pixel dimensions.
        let screen = state
            .screens
            .iter()
            .find(|screen| screen.id == id)
            .context(t().features.stream_display_disconnected)?;
        request.resolution_width = i32::try_from(screen.display.current.width)?;
        request.resolution_height = i32::try_from(screen.display.current.height)?;
        match step {
            Step::Resolution(mode, kind) => {
                request.resolution_width = i32::try_from(mode.width)?;
                request.resolution_height = i32::try_from(mode.height)?;
                request.resolution_type = kind;
            }
            Step::Dpi(dpi) => request.dpi_scale = i32::try_from(dpi)?,
        }
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1);
        tracing::info!(
            screen_id = id,
            sequence,
            width = request.resolution_width,
            height = request.resolution_height,
            dpi = request.dpi_scale,
            resolution_type = request.resolution_type,
            "display setting requested"
        );
        self.outgoing
            .send(OutgoingControlMessage {
                annotation_generation: None,
                sequence,
                payload: encode_capture_request(sequence, request),
                protocol: protocol(state),
                completion: None,
            })
            .map_err(|_| anyhow!("{}", t().features.stream_display_sender_stopped))?;
        Ok(sequence)
    }

    pub(super) fn drive_display_changes(&self, state: &mut StreamControlState) {
        state.display_changes.report_generation = state.screens_generation;
        let operations = std::mem::take(&mut state.display_changes.pending);
        for (id, pending) in operations {
            let screen = state.screens.iter().find(|s| s.id == id);
            let failure = if pending.generation != state.protocol_generation
                || ensure_ready(state).is_err()
            {
                Some(t().features.stream_display_connection_changed.into())
            } else if screen.is_none() {
                Some(t().features.stream_display_disconnected.into())
            } else {
                None
            };
            if let Some(failure) = failure {
                state.display_changes.finish(id, &pending, Some(failure));
                continue;
            }
            let info = &screen.expect("screen checked").display;
            let matches = match pending.step {
                Step::Resolution(mode, _) => info.current == mode,
                Step::Dpi(dpi) => info.current_dpi == dpi,
            };
            if state.screens_generation > pending.screens_generation && matches {
                tracing::info!(
                    screen_id = id,
                    sequence = pending.sequence,
                    "display setting confirmed by remote screen state"
                );
                state.display_changes.finish(id, &pending, None);
            } else {
                state.display_changes.pending.insert(id, pending);
            }
        }
        state.display_changes.observe_reports(
            state.protocol_generation,
            state.screens_generation,
            &state.screens,
        );
        state.display_changes.refresh_completed(&state.screens);
        state
            .display_changes
            .status
            .retain(|id, _| state.screens.iter().any(|s| s.id == *id));
    }
}

/// Host display replies: 0 ok, -3/-6 adjusted notices, -5 carries a nested
/// result whose `error_code` 0 means success. Anything else is a rejection.
fn display_reply_succeeded(error: &PbError) -> bool {
    matches!(error.error_code, 0 | -3 | -6)
        || (error.error_code == -5
            && serde_json::from_str::<serde_json::Value>(&error.error_detail)
                .ok()
                .is_some_and(|v| v.get("error_code").and_then(|c| c.as_i64()) == Some(0)))
}
