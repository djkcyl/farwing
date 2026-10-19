//! Local feedback for a submitted physical resolution change; never sends a command.
use crate::features::stream_control::StreamControlHandle;
use crate::ui::theme;

pub(super) fn show(
    ctx: &egui::Context,
    handle: &StreamControlHandle,
    screen_id: i32,
    viewport: egui::Rect,
) -> bool {
    let topology = handle.snapshot().topology;
    let mode = handle.pending_display_resolution(screen_id);
    let topology_visible = topology.blocks(screen_id);
    if !topology_visible && mode.is_none() {
        return false;
    }
    egui::Area::new(egui::Id::new(("display-transition", screen_id)))
        .order(egui::Order::Background)
        .fixed_pos(viewport.min)
        .movable(false)
        .show(ctx, |ui| {
            ui.set_min_size(viewport.size());
            ui.set_clip_rect(viewport);
            ui.painter().rect_filled(viewport, 0.0, theme::VIEWER_SCRIM);
            ui.allocate_rect(viewport, egui::Sense::click());
            let card = egui::Rect::from_center_size(
                viewport.center(),
                egui::vec2(viewport.width().min(280.0), 152.0),
            );
            let mut content = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(card)
                    .layout(egui::Layout::top_down(egui::Align::Center)),
            );
            crate::ui::controls::configure(content.style_mut(), theme::CONTROL_HEIGHT);
            content.add(
                egui::Spinner::new()
                    .size(theme::CONTROL_HEIGHT)
                    .color(theme::ACCENT),
            );
            content.add_space(8.0);
            let heading = if topology_visible {
                topology.message.as_str()
            } else {
                "正在切换分辨率…"
            };
            content.label(egui::RichText::new(heading).size(theme::SECTION));
            if !topology_visible && let Some(mode) = mode {
                content.label(egui::RichText::new(mode.label()).color(theme::MUTED));
            }
            content.add_space(8.0);
            if content
                .add_sized(
                    [120.0, theme::CONTROL_HEIGHT],
                    crate::ui::controls::secondary("继续观看"),
                )
                .on_hover_text("收起遮罩并停止等待；已发送的修改不会撤回")
                .clicked()
            {
                if topology_visible {
                    handle.dismiss_display_topology();
                } else {
                    handle.cancel_display_change(screen_id);
                }
                ctx.request_repaint();
            }
        });
    ctx.request_repaint_after(std::time::Duration::from_millis(33));
    true
}
