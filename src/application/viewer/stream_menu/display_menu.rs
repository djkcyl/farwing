use super::*;
use crate::features::stream_control::{
    DisplayChangeRequest, DisplayChangeStatus, DisplayResolution, RemoteDisplayInfo,
    StreamControlSnapshot,
};
use crate::i18n::t;
use crate::ui::{controls, theme};

#[derive(Default)]
pub(super) struct DisplayMenu {
    screen: Option<i32>,
    // Only differences from the displayed baseline are unapplied edits.
    request: DisplayChangeRequest,
    error: Option<String>,
    /// A topology action was requested this frame; the menu closes.
    pub(super) topology_requested: bool,
}

impl DisplayMenu {
    pub fn select_screen(&mut self, id: i32) {
        if self.screen != Some(id) {
            self.reset();
            self.screen = Some(id);
        }
    }
    pub fn reset(&mut self) {
        self.request = DisplayChangeRequest::default();
        self.error = None;
    }
    fn apply(
        &mut self,
        ui: &egui::Ui,
        handle: &StreamControlHandle,
        screen_id: i32,
        request: DisplayChangeRequest,
    ) {
        if let Some(choice) = request.resolution
            && super::topology_menu::resolution_needs_confirmation(handle, screen_id, choice)
        {
            super::topology_menu::request(
                ui.ctx(),
                handle,
                screen_id,
                crate::features::stream_control::DisplayTopologyAction::Resolution {
                    screen_id,
                    choice,
                },
            );
            return;
        }
        match handle.apply_display_change(screen_id, request) {
            Ok(()) => {
                if request.resolution.is_some() {
                    self.request.resolution = None;
                }
                if request.dpi.is_some() {
                    self.request.dpi = None;
                }
                self.error = None;
                ui.ctx().request_repaint();
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }
    fn resolution_baseline(
        info: &RemoteDisplayInfo,
        status: Option<&DisplayChangeStatus>,
        local: Option<(u32, u32)>,
    ) -> DisplayResolution {
        if let Some(status) = status.filter(|s| s.pending) {
            if let Some(choice) = status.requested_resolution_choice {
                return choice;
            }
            if let Some(mode) = status.requested_resolution {
                return DisplayResolution::Mode(mode);
            }
        }
        match info.resolution_type {
            3 if info.initial == Some(info.current) => DisplayResolution::Initial,
            2 if local == Some((info.current.width, info.current.height)) => {
                DisplayResolution::FollowLocal {
                    width: info.current.width,
                    height: info.current.height,
                }
            }
            _ => DisplayResolution::Mode(info.current),
        }
    }
    fn dpi_baseline(info: &RemoteDisplayInfo, status: Option<&DisplayChangeStatus>) -> Option<u32> {
        status
            .filter(|s| s.pending)
            .and_then(|s| s.requested_dpi)
            .or_else(|| (info.current_dpi > 0).then_some(info.current_dpi))
    }
    fn resolution_label(choice: DisplayResolution, info: &RemoteDisplayInfo) -> String {
        match choice {
            DisplayResolution::Initial => {
                (t().viewer.resolution_initial)(&info.initial.unwrap_or(info.current).label())
            }
            DisplayResolution::FollowLocal { width, height } => {
                (t().viewer.resolution_follow_local)(width, height)
            }
            DisplayResolution::Mode(mode) => mode.label(),
        }
    }
    fn reconcile(&mut self, resolution: DisplayResolution, dpi: Option<u32>) {
        if self.request.resolution == Some(resolution) {
            self.request.resolution = None;
        }
        if self.request.dpi == dpi {
            self.request.dpi = None;
        }
    }
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        handle: &StreamControlHandle,
        snapshot: &StreamControlSnapshot,
        screen_id: i32,
        local_size: Option<(u32, u32)>,
    ) {
        let Some(screen) = snapshot.screens.iter().find(|s| s.id == screen_id) else {
            ui.label(t().viewer.display_disconnected);
            return;
        };
        let info = &screen.display;
        let status = snapshot.display_changes.get(&screen_id);
        let resolution_base = Self::resolution_baseline(info, status, local_size);
        let dpi_base = Self::dpi_baseline(info, status);
        self.reconcile(resolution_base, dpi_base);
        let resolution_pending =
            status.is_some_and(|s| s.pending && s.requested_resolution.is_some());
        let dpi_pending = status.is_some_and(|s| s.pending && s.requested_dpi.is_some());
        let name = screen.label(&snapshot.screens);
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            ui.label(RichText::new(name).size(theme::SMALL).color(MUTED));
        });
        ui.add_space(theme::MENU_GROUP_GAP);
        ui.add_enabled_ui(
            snapshot.ready && snapshot.display_settings_supported,
            |ui| {
                let selected = self.request.resolution.unwrap_or(resolution_base);
                let (response, apply_resolution, cancel_resolution) = controls::setting_row(
                    ui,
                    t().viewer.resolution,
                    &Self::resolution_label(selected, info),
                    self.request.resolution.is_some(),
                    resolution_pending,
                );
                egui::Popup::menu(&response).width(WIDTH).show(|ui| {
                    menu_style(ui);
                    if let Some(choice) = resolution_options(
                        ui,
                        info,
                        local_size,
                        selected,
                        snapshot.topology_support.resolution_conversion,
                    ) {
                        self.request.resolution = (choice != resolution_base).then_some(choice);
                        self.error = None;
                        ui.close();
                    }
                });
                if self.request.resolution.is_some() {
                    if cancel_resolution {
                        self.request.resolution = None;
                        self.error = None;
                    } else if apply_resolution {
                        self.apply(
                            ui,
                            handle,
                            screen_id,
                            DisplayChangeRequest {
                                resolution: self.request.resolution,
                                dpi: None,
                            },
                        );
                    }
                } else if cancel_resolution {
                    handle.cancel_display_change(screen_id);
                }
                ui.add_space(theme::MENU_GROUP_GAP);
                ui.add_enabled_ui(
                    snapshot.dpi_settings_supported && !info.dpis.is_empty(),
                    |ui| {
                        let selected = self.request.dpi.or(dpi_base);
                        let label = selected.map_or_else(
                            || t().viewer.dpi_not_reported.into(),
                            |d| format!("{d}%"),
                        );
                        let (response, apply_dpi, cancel_dpi) = controls::setting_row(
                            ui,
                            t().viewer.dpi_scaling,
                            &label,
                            self.request.dpi.is_some(),
                            dpi_pending,
                        );
                        egui::Popup::menu(&response).width(WIDTH).show(|ui| {
                            menu_style(ui);
                            for dpi in &info.dpis {
                                if menu_row(
                                    ui,
                                    &format!("{dpi}%"),
                                    if *dpi == info.recommended_dpi {
                                        t().viewer.recommended
                                    } else {
                                        ""
                                    },
                                    Some(selected == Some(*dpi)),
                                    true,
                                    false,
                                )
                                .clicked()
                                {
                                    self.request.dpi = (Some(*dpi) != dpi_base).then_some(*dpi);
                                    self.error = None;
                                    ui.close();
                                }
                            }
                        });
                        if self.request.dpi.is_some() {
                            if cancel_dpi {
                                self.request.dpi = None;
                                self.error = None;
                            } else if apply_dpi {
                                self.apply(
                                    ui,
                                    handle,
                                    screen_id,
                                    DisplayChangeRequest {
                                        resolution: None,
                                        dpi: self.request.dpi,
                                    },
                                );
                            }
                        } else if cancel_dpi {
                            handle.cancel_display_change(screen_id);
                        }
                    },
                );
            },
        );
        if let Some(status) = status {
            crate::ui::controls::observe_notice(
                ui.ctx(),
                ("display-settings-remote", screen_id),
                t().viewer.display_settings_unconfirmed,
                crate::ui::controls::DialogIcon::Error,
                status.error.then_some(status.message.as_str()),
            );
            if dpi_pending {
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100));
            }
        }
        self.topology_requested = super::topology_menu::entries(ui, handle, screen_id, local_size);
        crate::ui::controls::observe_notice(
            ui.ctx(),
            "display-settings-local",
            t().viewer.display_settings_failed,
            crate::ui::controls::DialogIcon::Error,
            self.error.as_deref(),
        );
    }
}

// Use the reported desktop dimensions. Video output size and DPI do not
// change which aspect-ratio group a physical display mode belongs to.
fn aspect_ratio(width: u32, height: u32) -> (u32, u32) {
    let (mut a, mut b) = (width, height);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    if a == 0 {
        return (width, height);
    }
    let ratio = (width / a, height / a);
    match ratio {
        (8, 5) => (16, 10),
        (5, 8) => (10, 16),
        (7, 3) => (21, 9),
        _ => ratio,
    }
}

fn resolution_groups(
    info: &RemoteDisplayInfo,
) -> Vec<(
    (u32, u32),
    Vec<crate::features::stream_control::RemoteDisplayMode>,
)> {
    let mut groups = std::collections::BTreeMap::<_, Vec<_>>::new();
    for mode in info.modes.iter().rev() {
        groups
            .entry(aspect_ratio(mode.width, mode.height))
            .or_default()
            .push(*mode);
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    // Common desktop ratios first; remaining ratios retain their exact value.
    groups.sort_by_key(|(ratio, _)| {
        let rank = match ratio {
            (16, 9) => 0,
            (16, 10) => 1,
            (4, 3) => 2,
            (3, 2) => 3,
            (5, 4) => 4,
            (21, 9) => 5,
            (32, 9) => 6,
            _ => 7,
        };
        (rank, *ratio)
    });
    groups
}

fn resolution_options(
    ui: &mut egui::Ui,
    info: &RemoteDisplayInfo,
    local_size: Option<(u32, u32)>,
    selected: DisplayResolution,
    allow_conversion: bool,
) -> Option<DisplayResolution> {
    let mut chosen = None;
    egui::ScrollArea::vertical()
        .max_height(320.0)
        .show(ui, |ui| {
            let mut shortcuts = Vec::new();
            if info
                .initial
                .is_some_and(|m| allow_conversion || info.modes.contains(&m))
            {
                shortcuts.push(DisplayResolution::Initial);
            }
            if let Some((width, height)) = local_size.filter(|(w, h)| {
                allow_conversion || info.modes.iter().any(|m| m.width == *w && m.height == *h)
            }) {
                shortcuts.push(DisplayResolution::FollowLocal { width, height });
            }
            for choice in shortcuts {
                if menu_row(
                    ui,
                    &DisplayMenu::resolution_label(choice, info),
                    "",
                    Some(selected == choice),
                    true,
                    false,
                )
                .clicked()
                {
                    chosen = Some(choice);
                }
            }
            for (ratio, modes) in resolution_groups(info) {
                ui.add_space(theme::MENU_GROUP_GAP);
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(format!("{}:{}", ratio.0, ratio.1))
                            .size(theme::SMALL)
                            .color(MUTED),
                    );
                });
                for mode in modes {
                    let choice = DisplayResolution::Mode(mode);
                    if menu_row(ui, &mode.label(), "", Some(selected == choice), true, false)
                        .clicked()
                    {
                        chosen = Some(choice);
                    }
                }
            }
        });
    chosen
}

// The menu is bound to the clicked screen and sends directly from the
// choice callback. Opening/dismissing this menu is read-only.
pub(super) fn context_menu(
    ui: &mut egui::Ui,
    handle: &StreamControlHandle,
    screen_id: i32,
    local_size: Option<(u32, u32)>,
) -> Option<String> {
    let snapshot = handle.snapshot();
    let screen = snapshot.screens.iter().find(|s| s.id == screen_id);
    let enabled = snapshot.ready
        && snapshot.display_settings_supported
        && screen.is_some_and(|s| !s.display.modes.is_empty());
    let mut error = None;
    ui.add_enabled_ui(enabled, |ui| {
        let detail = screen
            .map(|s| format!("{} × {}", s.width, s.height))
            .unwrap_or_default();
        let response = menu_row(ui, t().viewer.resolution, &detail, None, enabled, true);
        egui::containers::menu::SubMenu::new().show(ui, &response, |ui| {
            menu_style(ui);
            ui.set_width(WIDTH);
            let Some(screen) = screen else {
                return;
            };
            let info = &screen.display;
            // Checked state describes the actual desktop, not an unconfirmed request.
            let selected = if local_size == Some((info.current.width, info.current.height)) {
                DisplayResolution::FollowLocal {
                    width: info.current.width,
                    height: info.current.height,
                }
            } else if info.initial == Some(info.current) {
                DisplayResolution::Initial
            } else {
                DisplayResolution::Mode(info.current)
            };
            if let Some(choice) = resolution_options(
                ui,
                info,
                local_size,
                selected,
                snapshot.topology_support.resolution_conversion,
            ) {
                if super::topology_menu::resolution_needs_confirmation(handle, screen_id, choice) {
                    super::topology_menu::request(
                        ui.ctx(),
                        handle,
                        screen_id,
                        crate::features::stream_control::DisplayTopologyAction::Resolution {
                            screen_id,
                            choice,
                        },
                    );
                    ui.close();
                    return;
                }
                error = handle
                    .apply_display_change(
                        screen_id,
                        DisplayChangeRequest {
                            resolution: Some(choice),
                            dpi: None,
                        },
                    )
                    .err()
                    .map(|e| e.to_string());
                ui.close();
            }
        });
    });
    // List supported percentages; the chosen DPI is sent independently
    // with this screen's current rectangle.
    let dpi_enabled = snapshot.ready
        && snapshot.dpi_settings_supported
        && screen.is_some_and(|s| !s.display.dpis.is_empty());
    ui.add_enabled_ui(dpi_enabled, |ui| {
        let detail = screen
            .map(|s| format!("{}%", s.display.current_dpi))
            .unwrap_or_default();
        let response = menu_row(ui, t().viewer.dpi_scaling, &detail, None, dpi_enabled, true);
        egui::containers::menu::SubMenu::new().show(ui, &response, |ui| {
            menu_style(ui);
            ui.set_width(WIDTH);
            let Some(screen) = screen else {
                return;
            };
            let info = &screen.display;
            egui::ScrollArea::vertical()
                .max_height(224.0)
                .show(ui, |ui| {
                    for dpi in &info.dpis {
                        if menu_row(
                            ui,
                            &format!("{dpi}%"),
                            if *dpi == info.recommended_dpi {
                                t().viewer.recommended
                            } else {
                                ""
                            },
                            Some(*dpi == info.current_dpi),
                            true,
                            false,
                        )
                        .clicked()
                        {
                            error = handle
                                .apply_display_change(
                                    screen_id,
                                    DisplayChangeRequest {
                                        resolution: None,
                                        dpi: Some(*dpi),
                                    },
                                )
                                .err()
                                .map(|e| e.to_string());
                            ui.close();
                        }
                    }
                });
        });
    });
    if super::topology_menu::entries(ui, handle, screen_id, local_size) {
        ui.close();
    }
    error
}
