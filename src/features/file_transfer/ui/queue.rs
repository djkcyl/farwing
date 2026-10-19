use super::{Command, Direction, Snapshot, TaskState, size, theme};
use crate::ui::controls::{
    self,
    files::{self, Icon},
};
use egui::{Rect, Ui, UiBuilder, pos2, vec2};

pub(super) struct Queue {
    pub collapsed: bool,
    pub height: f32,
    filter: Option<usize>,
}
impl Default for Queue {
    fn default() -> Self {
        Self {
            collapsed: false,
            height: theme::FILES_TASK_HEIGHT,
            filter: None,
        }
    }
}
pub(super) enum Action {
    Command(Command),
    Cancel(String),
}

fn group(state: TaskState) -> usize {
    match state {
        TaskState::Running | TaskState::Queued => 0,
        TaskState::Paused => 1,
        TaskState::Done | TaskState::Cancelled | TaskState::Skipped => 2,
        TaskState::Failed => 3,
    }
}

// Fixed statistic/action columns share exactly the same edges in the header and every row.
fn columns(rect: Rect) -> [Rect; 6] {
    let flexible = (rect.width() - 290.).max(380.);
    let widths = [
        flexible * 0.31,
        flexible * 0.25,
        flexible * 0.44,
        100.,
        90.,
        100.,
    ];
    let mut left = rect.left();
    widths.map(|width| {
        let r = Rect::from_min_size(pos2(left, rect.top()), vec2(width, rect.height()));
        left += width;
        r
    })
}

pub(super) fn show(
    ui: &mut Ui,
    rect: Rect,
    q: &mut Queue,
    s: &Snapshot,
    alias: &str,
) -> Option<Action> {
    let mut action = None;
    let mut child = ui.new_child(UiBuilder::new().id_salt("queue").max_rect(rect));
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    let ui = &mut child;
    ui.spacing_mut().item_spacing = vec2(6., 0.);
    let mut counts = [0; 4];
    for r in &s.records {
        counts[group(r.state)] += 1;
    }
    let header = Rect::from_min_size(rect.min, vec2(rect.width(), 38.));
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(header)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
        |ui| {
            if files::icon(
                ui,
                if q.collapsed {
                    Icon::Expand
                } else {
                    Icon::Collapse
                },
                if q.collapsed {
                    "展开队列"
                } else {
                    "收起队列"
                },
                true,
            )
            .clicked()
            {
                q.collapsed = !q.collapsed;
            }
            ui.label(egui::RichText::new("传输队列").strong());
            ui.add_space(8.);
            for (filter, label, count) in [
                (None, "全部", s.records.len()),
                (Some(0), "进行中", counts[0]),
                (Some(1), "已暂停", counts[1]),
                (Some(2), "已完成", counts[2]),
                (Some(3), "失败", counts[3]),
            ] {
                if ui
                    .selectable_label(q.filter == filter, format!("{label} {count}"))
                    .clicked()
                {
                    q.filter = filter;
                    q.collapsed = false;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(counts[2] > 0, controls::quiet_button("清除已完成"))
                    .clicked()
                {
                    action = Some(Action::Command(Command::ClearCompleted));
                }
                if counts[0] > 0 {
                    if ui.add(controls::quiet_button("全部暂停")).clicked() {
                        action = Some(Action::Command(Command::PauseAll));
                    }
                } else if ui
                    .add_enabled(
                        s.connected && counts[1] + counts[3] > 0,
                        controls::quiet_button("全部继续"),
                    )
                    .clicked()
                {
                    action = Some(Action::Command(Command::ResumeAll));
                }
            });
        },
    );
    if q.collapsed {
        return action;
    }
    let table = Rect::from_min_max(header.left_bottom(), rect.max);
    files::panel(ui, table, false);
    let headings = Rect::from_min_size(table.min + vec2(1., 1.), vec2(table.width() - 2., 28.));
    for (col, label) in
        columns(headings)
            .iter()
            .zip(["文件名", "方向", "进度", "速度", "剩余", "操作"])
    {
        files::text(ui, col.shrink2(vec2(10., 0.)), label, theme::MUTED, false);
    }
    ui.painter().line_segment(
        [headings.left_bottom(), headings.right_bottom()],
        egui::Stroke::new(1., theme::LINE),
    );
    let body = Rect::from_min_max(
        headings.left_bottom() + vec2(1., 1.),
        table.max - vec2(2., 2.),
    );
    let records = s
        .records
        .iter()
        .rev()
        .filter(|r| q.filter.is_none_or(|filter| group(r.state) == filter))
        .collect::<Vec<_>>();
    ui.scope_builder(UiBuilder::new().max_rect(body), |ui| {
        ui.spacing_mut().item_spacing.y = 0.;
        egui::ScrollArea::vertical()
            .id_salt(("tasks", q.filter))
            .auto_shrink([false, false])
            .max_height(body.height())
            .show_rows(ui, theme::FILES_QUEUE_ROW, records.len(), |ui, range| {
                for index in range {
                    let r = records[index];
                    let (row, response) = ui.allocate_exact_size(
                        vec2(ui.available_width(), theme::FILES_QUEUE_ROW),
                        egui::Sense::hover(),
                    );
                    let cols = columns(row);
                    if response.hovered() {
                        ui.painter().rect_filled(row, 0, theme::HOVER);
                    }
                    ui.painter().line_segment(
                        [row.left_bottom(), row.right_bottom()],
                        egui::Stroke::new(1., theme::LINE),
                    );
                    let name = r
                        .source
                        .trim_end_matches(['\\', '/'])
                        .rsplit(['\\', '/'])
                        .next()
                        .unwrap_or(&r.source);
                    let icon = Rect::from_center_size(
                        cols[0].left_center() + vec2(18., 0.),
                        vec2(18., 18.),
                    );
                    controls::file_icons::paint(
                        ui.painter(),
                        icon,
                        name,
                        if r.folder.is_empty() { 4 } else { 0 },
                        "",
                    );
                    let mut name_rect = cols[0].shrink2(vec2(10., 0.));
                    name_rect.min.x += 24.;
                    files::text(ui, name_rect, name, theme::TEXT, false);
                    response.on_hover_text(format!(
                        "{}\n→ {}{}",
                        r.source,
                        r.destination,
                        r.error
                            .as_ref()
                            .map(|e| format!("\n{e}"))
                            .unwrap_or_default()
                    ));
                    let dir = if r.direction == Direction::Upload {
                        format!("本机 → {alias}")
                    } else {
                        format!("{alias} → 本机")
                    };
                    files::text(
                        ui,
                        cols[1].shrink2(vec2(10., 0.)),
                        &dir,
                        theme::MUTED,
                        false,
                    );
                    let p = s.progress.get(&r.key).cloned().unwrap_or_default();
                    let bytes = if r.state == TaskState::Done {
                        r.total
                    } else {
                        p.bytes.max(r.completed_bytes()).max(r.confirmed)
                    };
                    let progress = cols[2].shrink2(vec2(10., 5.));
                    let percent = if r.total == 0 {
                        if r.state == TaskState::Done { 1. } else { 0. }
                    } else {
                        (bytes as f32 / r.total as f32).clamp(0., 1.)
                    };
                    let state = if r.state == TaskState::Running {
                        format!(
                            "{:.0}% · {} / {}",
                            percent * 100.,
                            size(bytes),
                            size(r.total)
                        )
                    } else {
                        format!("{} · {} / {}", r.state.label(), size(bytes), size(r.total))
                    };
                    files::text(
                        ui,
                        Rect::from_min_size(progress.min, vec2(progress.width(), 22.)),
                        &state,
                        if r.state == TaskState::Failed {
                            theme::RED
                        } else {
                            theme::MUTED
                        },
                        false,
                    );
                    let bar = Rect::from_min_size(
                        progress.min + vec2(0., 28.),
                        vec2(progress.width(), 3.),
                    );
                    ui.painter().rect_filled(bar, 2, theme::SURFACE);
                    ui.painter().rect_filled(
                        Rect::from_min_size(bar.min, vec2(bar.width() * percent, 3.)),
                        2,
                        if r.state == TaskState::Done {
                            theme::GREEN
                        } else {
                            theme::ACCENT
                        },
                    );
                    let running = r.state == TaskState::Running;
                    files::text(
                        ui,
                        cols[3].shrink2(vec2(10., 0.)),
                        &if running {
                            format!("{}/s", size(p.speed as u64))
                        } else {
                            "—".into()
                        },
                        theme::MUTED,
                        false,
                    );
                    let remaining = if running && p.speed > 0. && r.total > bytes {
                        let seconds = ((r.total - bytes) as f64 / p.speed)
                            .ceil()
                            .min(u64::MAX as f64) as u64;
                        if seconds >= 3600 {
                            format!("{} 时 {} 分", seconds / 3600, (seconds % 3600) / 60)
                        } else if seconds >= 60 {
                            format!("{} 分 {} 秒", seconds / 60, seconds % 60)
                        } else {
                            format!("{seconds} 秒")
                        }
                    } else {
                        "—".into()
                    };
                    files::text(
                        ui,
                        cols[4].shrink2(vec2(10., 0.)),
                        &remaining,
                        theme::MUTED,
                        false,
                    );
                    ui.scope_builder(
                        UiBuilder::new()
                            .id_salt(&r.key)
                            .max_rect(cols[5].shrink2(vec2(6., 0.)))
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                        |ui| {
                            if matches!(r.state, TaskState::Running | TaskState::Queued) {
                                if files::icon(ui, Icon::Pause, "暂停", true).clicked() {
                                    action = Some(Action::Command(Command::Pause(r.key.clone())));
                                }
                            } else if matches!(r.state, TaskState::Paused | TaskState::Failed) {
                                if files::icon(ui, Icon::Resume, "继续", s.connected).clicked() {
                                    action = Some(Action::Command(Command::Resume(r.key.clone())));
                                }
                            } else {
                                ui.allocate_space(vec2(
                                    theme::COMPACT_HEIGHT,
                                    theme::COMPACT_HEIGHT,
                                ));
                            }
                            let complete = group(r.state) == 2;
                            if files::icon(
                                ui,
                                Icon::Close,
                                if complete {
                                    "移除记录"
                                } else {
                                    "取消传输"
                                },
                                true,
                            )
                            .clicked()
                            {
                                action = Some(if complete {
                                    Action::Command(Command::Remove(r.key.clone()))
                                } else {
                                    Action::Cancel(r.key.clone())
                                });
                            }
                        },
                    );
                }
            });
    });
    if records.is_empty() {
        files::empty(
            ui,
            body,
            match q.filter {
                None => "选择文件后上传或下载",
                Some(0) => "没有进行中的任务",
                Some(1) => "没有暂停的任务",
                Some(2) => "没有已完成的任务",
                _ => "没有失败的任务",
            },
        );
    }
    action
}
