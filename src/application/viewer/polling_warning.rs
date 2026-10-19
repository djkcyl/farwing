//! Noninteractive overlay: no focus, input region or automatic policy changes.
use crate::features::remote_input::RemoteInput;

pub(super) fn show(ctx: &egui::Context, input: &RemoteInput, bounds: egui::Rect) {
    let Some(warning) = input.polling_warning() else {
        return;
    };
    crate::ui::controls::input_rate_warning(ctx, warning.hz, bounds);
    ctx.request_repaint_after(
        warning
            .until
            .saturating_duration_since(std::time::Instant::now()),
    );
}
