//! Window-local operation messages. Publishing a message never consumes page layout.
use super::{DialogAction, DialogIcon, dialog_actions, dialog_frame, dialog_header, theme};
use egui::{Context, Id};
use std::collections::{HashMap, VecDeque};

#[derive(Clone)]
struct Notice {
    source: Id,
    title: String,
    message: String,
    icon: DialogIcon,
    action: Option<String>,
    report: bool,
    parent: Option<egui::LayerId>,
}

#[derive(Clone, Default)]
struct State {
    pending: VecDeque<Notice>,
    visible: Option<Id>,
    dialog: Option<(u64, egui::LayerId)>,
    observed: HashMap<Id, String>,
    results: HashMap<Id, bool>,
    progress: HashMap<Id, u64>,
}
fn state_id() -> Id {
    Id::new("operation-notices")
}
fn modal_id() -> Id {
    Id::new("operation-notice-modal")
}

/// Use for an explicit operation result; an identical result from a later user
/// attempt may be shown again after the previous message was acknowledged.
pub(crate) fn notice(
    ctx: &Context,
    source: impl egui::AsId,
    title: &str,
    icon: DialogIcon,
    message: impl Into<String>,
) {
    publish(
        ctx,
        Id::new(source),
        title,
        icon,
        message.into(),
        None,
        false,
        true,
    );
}

fn publish(
    ctx: &Context,
    source: Id,
    title: &str,
    icon: DialogIcon,
    message: String,
    action: Option<&str>,
    report: bool,
    nested: bool,
) {
    if message.trim().is_empty() {
        return;
    }
    let frame = ctx.cumulative_frame_nr();
    let parent = if nested {
        ctx.data_mut(|d| d.get_temp_mut_or_default::<State>(state_id()).dialog)
            .filter(|(seen, _)| *seen == frame)
            .map(|(_, layer)| layer)
            .or_else(|| {
                ctx.memory(|m| m.top_modal_layer())
                    .filter(|layer| layer.id != modal_id())
            })
    } else {
        None
    };
    ctx.data_mut(|d| {
        let state = d.get_temp_mut_or_default::<State>(state_id());
        if let Some(existing) = state.pending.iter_mut().find(|n| n.source == source) {
            existing.message = message;
            existing.title = title.into();
            existing.icon = icon;
            existing.action = action.map(str::to_owned);
            existing.report = report;
        } else {
            state.pending.push_back(Notice {
                source,
                title: title.into(),
                message,
                icon,
                action: action.map(str::to_owned),
                report,
                parent,
            });
        }
    });
    ctx.request_repaint();
}

pub(crate) fn observe_notice(
    ctx: &Context,
    source: impl egui::AsId,
    title: &str,
    icon: DialogIcon,
    message: Option<&str>,
) {
    observe(
        ctx,
        Id::new(source),
        title,
        icon,
        message,
        None,
        false,
        false,
    );
}

/// Return a choice on a following frame. true is the named action; false is
/// dismiss/escape. The business owner retains the action and its permission checks.
pub(crate) fn observe_notice_action(
    ctx: &Context,
    source: impl egui::AsId,
    title: &str,
    icon: DialogIcon,
    message: Option<&str>,
    action: &str,
) -> Option<bool> {
    let source = Id::new(source);
    observe(ctx, source, title, icon, message, Some(action), true, false);
    ctx.data_mut(|d| {
        d.get_temp_mut_or_default::<State>(state_id())
            .results
            .remove(&source)
    })
}

fn observe(
    ctx: &Context,
    source: Id,
    title: &str,
    icon: DialogIcon,
    message: Option<&str>,
    action: Option<&str>,
    report: bool,
    nested: bool,
) {
    let changed = ctx.data_mut(|d| {
        let state = d.get_temp_mut_or_default::<State>(state_id());
        match message.filter(|s| !s.trim().is_empty()) {
            Some(message) if state.observed.get(&source).is_none_or(|old| old != message) => {
                state.observed.insert(source, message.into());
                true
            }
            None => {
                state.observed.remove(&source);
                false
            }
            _ => false,
        }
    });
    if changed {
        publish(
            ctx,
            source,
            title,
            icon,
            message.unwrap().into(),
            action,
            report,
            nested,
        );
    }
}

pub(crate) fn show_notices(ctx: &Context) {
    let frame = ctx.cumulative_frame_nr();
    ctx.data_mut(|d| {
        let state = d.get_temp_mut_or_default::<State>(state_id());
        let finished = state
            .progress
            .iter()
            .filter(|(_, seen)| **seen != frame)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for source in finished {
            state.pending.retain(|n| n.source != source);
            state.observed.remove(&source);
            state.progress.remove(&source);
        }
    });
    let parent = ctx
        .data_mut(|d| d.get_temp_mut_or_default::<State>(state_id()).dialog)
        .filter(|(seen, _)| *seen == frame)
        .map(|(_, layer)| layer);
    let current = ctx.data_mut(|d| {
        let state = d.get_temp_mut_or_default::<State>(state_id());
        let eligible = |notice: &&Notice| {
            parent.is_none_or(|layer| layer.id == modal_id() || notice.parent == Some(layer))
        };
        // Keep a visible message stable until acknowledgement. A form validation
        // message may pass queued background failures that are waiting for that form.
        let current = state
            .visible
            .and_then(|id| state.pending.iter().find(|n| n.source == id))
            .filter(eligible)
            .or_else(|| state.pending.iter().find(eligible))
            .cloned();
        state.visible = current.as_ref().map(|n| n.source);
        current
    });
    let Some(current) = current else {
        return;
    };
    let mut closed = false;
    let mut accepted = false;
    let response = egui::Modal::new(modal_id())
        .frame(dialog_frame())
        .show(ctx, |ui| {
            ui.set_width(
                theme::MESSAGE_DIALOG_WIDTH.min((ctx.content_rect().width() - 64.).max(240.)),
            );
            closed = dialog_header(ui, &current.title, current.icon, true);
            egui::ScrollArea::vertical()
                .id_salt(current.source)
                .max_height((ctx.content_rect().height() - 180.).clamp(80., 300.))
                .show(ui, |ui| {
                    ui.add(egui::Label::new(&current.message).wrap().selectable(true));
                });
            let (yes, no) = dialog_actions(
                ui,
                Some(DialogAction::new(
                    current.action.as_deref().unwrap_or("知道了"),
                )),
                current
                    .action
                    .as_ref()
                    .filter(|_| current.report)
                    .map(|_| "关闭"),
            );
            accepted = yes;
            closed |= no;
        });
    if accepted || closed || response.should_close() {
        ctx.data_mut(|d| {
            let state = d.get_temp_mut_or_default::<State>(state_id());
            if let Some(index) = state
                .pending
                .iter()
                .position(|n| n.source == current.source)
            {
                state.pending.remove(index);
            }
            state.visible = None;
            if current.report {
                state.results.insert(current.source, accepted);
            }
        });
        ctx.request_repaint();
    }
}

/// End a transient progress message when its owning operation finishes.
pub(crate) fn clear_notice(ctx: &Context, source: impl egui::AsId) {
    clear_by_id(ctx, Id::new(source));
}

fn clear_by_id(ctx: &Context, source: Id) {
    ctx.data_mut(|d| {
        let state = d.get_temp_mut_or_default::<State>(state_id());
        state.pending.retain(|n| n.source != source);
        state.observed.remove(&source);
        state.results.remove(&source);
        state.progress.remove(&source);
    });
}

pub(crate) fn progress_notice(
    ctx: &Context,
    source: impl egui::AsId,
    title: &str,
    message: Option<&str>,
) {
    let source = Id::new(source);
    if let Some(message) = message {
        observe(
            ctx,
            source,
            title,
            DialogIcon::Waiting,
            Some(message),
            Some("关闭提示"),
            false,
            false,
        );
        let frame = ctx.cumulative_frame_nr();
        ctx.data_mut(|d| {
            d.get_temp_mut_or_default::<State>(state_id())
                .progress
                .insert(source, frame);
        });
    } else {
        clear_by_id(ctx, source);
    }
}

pub(crate) fn observe_form_notice(
    ctx: &Context,
    source: impl egui::AsId,
    title: &str,
    icon: DialogIcon,
    message: Option<&str>,
) {
    observe(
        ctx,
        Id::new(source),
        title,
        icon,
        message,
        None,
        false,
        true,
    );
}

// Header registration also sees newly opened forms in this frame; egui's
// top_modal_layer reports only the previous frame.
pub(super) fn register_dialog(ctx: &Context, layer: egui::LayerId) {
    if layer.id == modal_id() {
        return;
    }
    let frame = ctx.cumulative_frame_nr();
    ctx.data_mut(|d| {
        d.get_temp_mut_or_default::<State>(state_id()).dialog = Some((frame, layer));
    });
}
