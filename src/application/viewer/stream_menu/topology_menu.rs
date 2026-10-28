use crate::i18n::t;
use super::*;
use crate::features::stream_control::{
    DisplayResolution, DisplayTopologyAction as Action, RemoteScreen,
};

#[derive(Clone)]
struct Confirmation {
    action: Action,
    origin: i32,
    generation: u64,
    handle: StreamControlHandle,
    target: RemoteScreen,
}

fn confirmation_id() -> egui::Id {
    egui::Id::new("display-topology-confirmation")
}

pub(in crate::application::viewer) fn owns_input(ctx: &egui::Context) -> bool {
    ctx.data(|d| {
        d.get_temp::<Confirmation>(confirmation_id()).is_some()
            || d.get_temp::<String>(egui::Id::new("display-topology-local-error"))
                .is_some()
    })
}

pub(in crate::application::viewer) fn request(
    ctx: &egui::Context,
    handle: &StreamControlHandle,
    origin: i32,
    action: Action,
) {
    if let Some(target) = handle
        .snapshot()
        .screens
        .into_iter()
        .find(|s| s.id == origin)
    {
        ctx.data_mut(|d| {
            d.insert_temp(
                confirmation_id(),
                Confirmation {
                    action,
                    origin,
                    generation: handle.handshake_status().generation,
                    handle: handle.clone(),
                    target,
                },
            )
        });
        ctx.request_repaint();
    }
}

pub(in crate::application::viewer) fn local_parameters(
    ctx: &egui::Context,
    handle: &StreamControlHandle,
    size: Option<(u32, u32)>,
) -> (u32, u32, u32) {
    let display = handle.snapshot().local_display;
    let (w, h) = size.unwrap_or((display.width, display.height));
    let dpi = ctx.input(|i| i.viewport().native_pixels_per_point.unwrap_or(1.0));
    (w, h, (dpi * 100.0).round() as u32)
}

/// A mode the display cannot show itself takes a super screen.
pub(super) fn resolution_needs_confirmation(
    handle: &StreamControlHandle,
    id: i32,
    choice: DisplayResolution,
) -> bool {
    handle
        .snapshot()
        .screens
        .iter()
        .find(|s| s.id == id)
        .is_some_and(|s| !s.display.shows(choice))
}

/// The topology action for screen `id`; whether one was requested. The
/// caller closes its own menu: `ui.close()` inside a [`egui::Window`] leaves
/// it collapsed when it is opened again.
pub(super) fn entries(
    ui: &mut egui::Ui,
    handle: &StreamControlHandle,
    id: i32,
    local: Option<(u32, u32)>,
) -> bool {
    let snapshot = handle.snapshot();
    let Some(screen) = snapshot.screens.iter().find(|s| s.id == id) else {
        return false;
    };
    let ready = snapshot.ready && (!snapshot.topology.pending || snapshot.topology.dismissed);
    use crate::features::stream_control::wire::DisplayKind;
    let action = if screen.display.kind == DisplayKind::Exclusive {
        Some((
            t().viewer.exit_super_screen,
            Action::Exit,
            snapshot.topology_support.exit,
        ))
    } else if screen.display.kind == DisplayKind::Virtual {
        Some((
            t().viewer.remove_virtual_screen,
            Action::Remove { screen_id: id },
            snapshot.dpi_settings_supported && snapshot.screens.len() > 1,
        ))
    } else if snapshot.topology_support.enter {
        let (width, height, dpi) = local_parameters(ui.ctx(), handle, local);
        Some((
            t().viewer.enter_super_screen,
            Action::Enter { width, height, dpi },
            true,
        ))
    } else {
        None
    };
    if let Some((label, action, enabled)) = action {
        section_separator(ui);
        if menu_row(ui, label, "", None, ready && enabled, false).clicked() {
            request(ui.ctx(), handle, id, action);
            return true;
        }
    }
    false
}

pub(super) fn show(
    ctx: &egui::Context,
    handle: &StreamControlHandle,
    local_size: Option<(u32, u32)>,
) {
    let pending = ctx.data(|d| d.get_temp::<Confirmation>(confirmation_id()));
    let Some(pending) = pending else { return };
    let snapshot = handle.snapshot();
    let valid = snapshot.ready
        && pending.handle.mouse().same_session(handle.mouse())
        && pending.generation == handle.handshake_status().generation
        && snapshot.screens.iter().any(|s| {
            s.id == pending.target.id
                && s.name == pending.target.name
                && s.display.kind == pending.target.display.kind
        });
    let mut close = false;
    egui::Modal::new(egui::Id::new("display-topology-modal")).frame(crate::ui::controls::dialog_frame()).show(ctx, |ui| {
        crate::ui::controls::configure(ui.style_mut(), crate::ui::theme::CONTROL_HEIGHT);
        ui.set_width(340.0);
        close = crate::ui::controls::dialog_header(ui,pending.action.label(),crate::ui::controls::DialogIcon::Warning,true);
        let description = match pending.action {
            Action::Create { .. } => None,
            Action::Remove { .. } => Some((t().viewer.topology_removes)(&pending.target.label(&snapshot.screens))),
            Action::Exit => Some(t().viewer.topology_exit_interrupts.into()),
            Action::Resolution { .. } => Some(t().viewer.topology_resolution_unsupported.into()),
            Action::FrameRate { .. } => Some(t().viewer.topology_refresh_too_low.into()),
            Action::Enter { .. } => Some(t().viewer.topology_affects_all.into()),
        };
        if let Some(description) = description { ui.label(description); }
        if !valid { ui.add_space(8.0); ui.label(RichText::new(t().viewer.display_state_changed).color(crate::ui::theme::MUTED)); }
        let (apply,cancel) = crate::ui::controls::dialog_actions(ui,Some(crate::ui::controls::DialogAction::new(t().viewer.confirm).enabled(valid)),Some(t().common.cancel));
        close |= cancel;
        {
            if apply {
                if let Err(error) = handle.apply_display_topology(pending.origin, {
                        let (width, height, dpi) = local_parameters(ctx, handle, local_size);
                        match pending.action {
                            Action::Enter { .. } => Action::Enter { width, height, dpi },
                            Action::FrameRate { settings, .. } => Action::FrameRate { settings, width, height, dpi },
                            Action::Resolution { screen_id, choice: DisplayResolution::FollowLocal { .. } } => Action::Resolution { screen_id, choice: DisplayResolution::FollowLocal { width, height } },
                            action => action,
                        }
                    }) {
                    ctx.data_mut(|d| d.insert_temp(egui::Id::new("display-topology-local-error"), error.to_string()));
                }
                close = true;
            }
        }
    });
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        close = true;
    }
    if close {
        ctx.data_mut(|d| d.remove::<Confirmation>(confirmation_id()));
    }
}

pub(super) fn show_error(ctx: &egui::Context, handle: &StreamControlHandle) {
    let status = handle.snapshot().topology;
    let local_id = egui::Id::new("display-topology-local-error");
    let local: Option<String> = ctx.data(|d| d.get_temp(local_id));
    let error = local.as_deref().or_else(|| {
        (status.error && !status.dismissed && ctx.input(|i| i.viewport().focused.unwrap_or(false)))
            .then_some(status.message.as_str())
    });
    if let Some(error) = error {
        egui::Modal::new(egui::Id::new("display-topology-error"))
            .frame(crate::ui::controls::dialog_frame())
            .show(ctx, |ui| {
                ui.set_width(340.0);
                let close = crate::ui::controls::dialog_header(
                    ui,
                    t().viewer.display_action,
                    crate::ui::controls::DialogIcon::Error,
                    true,
                );
                ui.label(error);
                let accept = crate::ui::controls::dialog_actions(
                    ui,
                    Some(crate::ui::controls::DialogAction::new(t().common.ok)),
                    None,
                )
                .0;
                if close || accept {
                    handle.dismiss_display_topology();
                    ctx.data_mut(|d| d.remove::<String>(local_id));
                }
            });
    }
}
