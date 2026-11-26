use super::{protocol::FileEntry, service::*};
use crate::ui::{controls, theme, window_manager};
use anyhow::Result;
use egui::{RichText, vec2};
use std::sync::Arc;
mod browser;
mod queue;
use crate::ui::controls::files::{self, Icon};
use browser::Pane;

pub(crate) fn open(
    client: Arc<crate::account::client::AuthenticatedClient>,
    device: crate::account::api::DeviceInfo,
    options: crate::media::ConnectionMediaOptions,
) -> Result<()> {
    crate::account::api::validate_device_id(&device.device_id)?;
    let runtime = tokio::runtime::Handle::current();
    let key = format!("files:{}:{}", client.device_id(), device.device_id);
    let viewport = egui::ViewportBuilder::default()
        .with_title(format!("OpenUUYC · {} · 文件传输", device.alias))
        .with_icon(crate::ui::branding::icon(false))
        .with_inner_size(theme::FILES_WINDOW_SIZE)
        .with_min_inner_size(theme::FILES_WINDOW_MIN);
    window_manager::send(window_manager::Request::Open {
        key,
        config: crate::ui::WindowConfig {
            viewport,
            centered: true,
            notification: false,
            floating: false,
        },
        factory: Box::new(move |ctx, _| {
            theme::configure(ctx);
            let _enter = runtime.enter();
            let handle = super::service::start(client.clone(), device.clone(), options);
            let previous = handle.snapshot();
            let home = super::storage::known_folder(super::storage::KnownFolder::Downloads)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| ":/".into());
            let local = if !previous.local.path.is_empty() {
                previous.local.path.clone()
            } else if std::path::Path::new(&home).is_dir() {
                home
            } else {
                ":/".into()
            };
            let remote = if previous.remote.path.is_empty() {
                ":/".into()
            } else {
                previous.remote.path.clone()
            };
            let _ = handle.send(Command::Browse(false, local.clone()));
            Box::new(Window {
                client,
                alias: device.alias,
                handle,
                local: Pane::new(local, false),
                remote: Pane::new(remote, true),
                ready: false,
                connection_generation: 0,
                dialog: None,
                error: None,
                policy: 2,
                mutation: 0,
                split: 0.5,
                queue: queue::Queue::default(),
                active_remote: false,
            })
        }),
    })
}
struct Window {
    client: Arc<crate::account::client::AuthenticatedClient>,
    alias: String,
    handle: Handle,
    local: Pane,
    remote: Pane,
    ready: bool,
    connection_generation: u64,
    dialog: Option<Dialog>,
    error: Option<String>,
    policy: i32,
    mutation: u64,
    split: f32,
    queue: queue::Queue,
    active_remote: bool,
}

enum Dialog {
    Transfer(Vec<Record>),
    Create(bool, String, String),
    Rename(bool, String, String),
    Delete(bool, FileEntry),
    Cancel(String),
}
enum Action {
    Browse(String),
    History(String),
    Transfer,
    Create,
    Rename(FileEntry),
    Delete(FileEntry),
}
impl crate::ui::App for Window {
    fn ui(&mut self, ui: &mut egui::Ui) {
        if !self.client.is_active() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        let s = self.handle.snapshot();
        if s.connected && (!self.ready || self.connection_generation != s.connection_generation) {
            self.ready = true;
            self.connection_generation = s.connection_generation;
            self.submit(Command::Browse(true, self.remote.path.clone()));
        }
        if !s.connected {
            self.ready = false;
        }
        if self.mutation != s.mutation {
            self.refresh();
        }
        self.mutation = s.mutation;
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(theme::FILES_MARGIN),
            )
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 4.;
                ui.horizontal(|ui| {
                    let (icon, _) = ui.allocate_exact_size(vec2(26., 30.), egui::Sense::hover());
                    files::paint_icon(ui.painter(), icon, Icon::Computer, theme::MUTED);
                    ui.label(RichText::new(&self.alias).strong());
                    ui.add_space(8.);
                    ui.label(
                        RichText::new(if s.connected {
                            "● 已连接"
                        } else if s.connecting {
                            "正在连接…"
                        } else {
                            "未连接"
                        })
                        .size(theme::SMALL)
                        .color(if s.connected {
                            theme::GREEN
                        } else {
                            theme::MUTED
                        }),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_enabled(
                                !s.connecting,
                                controls::quiet_button(if s.connected {
                                    "刷新两侧"
                                } else {
                                    "重新连接"
                                }),
                            )
                            .clicked()
                        {
                            if s.connected {
                                self.refresh();
                            } else {
                                self.submit(Command::Connect);
                            }
                        }
                    });
                });
                if let Some(error) = self.error.take() {
                    crate::ui::controls::notice(
                        ui.ctx(),
                        "file-operation-error",
                        "文件传输",
                        crate::ui::controls::DialogIcon::Error,
                        error,
                    );
                }
                crate::ui::controls::observe_notice(
                    ui.ctx(),
                    "file-service-error",
                    "文件传输",
                    crate::ui::controls::DialogIcon::Error,
                    s.error.as_deref(),
                );
                ui.add_space(4.);
                let area = ui.available_rect_before_wrap();
                let queue_height = if self.queue.collapsed {
                    38.
                } else {
                    self.queue.height.clamp(
                        140.,
                        (area.height() - theme::FILES_PANE_MIN_HEIGHT - theme::FILES_SPLITTER)
                            .max(140.),
                    )
                };
                let panes = egui::Rect::from_min_max(
                    area.min,
                    area.right_bottom() - vec2(0., queue_height + theme::FILES_SPLITTER),
                );
                let min = (theme::FILES_PANE_MIN / panes.width()).min(0.49);
                self.split = self.split.clamp(min, 1. - min);
                let divider_x = panes.left() + panes.width() * self.split;
                let divider = egui::Rect::from_min_max(
                    egui::pos2(divider_x - theme::FILES_SPLITTER / 2., panes.top()),
                    egui::pos2(divider_x + theme::FILES_SPLITTER / 2., panes.bottom()),
                );
                let split_response =
                    files::divider(ui, divider, ui.id().with("file-pane-divider"), true);
                if split_response.dragged()
                    && let Some(pos) = split_response.interact_pointer_pos()
                {
                    self.split = ((pos.x - panes.left()) / panes.width()).clamp(min, 1. - min);
                }
                let left = egui::Rect::from_min_max(panes.min, divider.left_bottom());
                let right = egui::Rect::from_min_max(divider.right_top(), panes.max);
                let keyboard = self.dialog.is_none() && s.takeover.is_none();
                let a = browser::show(
                    ui,
                    left,
                    &mut self.local,
                    &s.local,
                    false,
                    &self.alias,
                    s.connected,
                    !s.operation_busy,
                    &mut self.active_remote,
                    keyboard,
                );
                let b = browser::show(
                    ui,
                    right,
                    &mut self.remote,
                    &s.remote,
                    true,
                    &self.alias,
                    s.connected,
                    s.connected && !s.operation_busy,
                    &mut self.active_remote,
                    keyboard,
                );
                if let Some(a) = a {
                    self.action(false, a, &s);
                }
                if let Some(b) = b {
                    self.action(true, b, &s);
                }
                let separator = egui::Rect::from_min_size(
                    panes.left_bottom(),
                    vec2(panes.width(), theme::FILES_SPLITTER),
                );
                let response =
                    files::divider(ui, separator, ui.id().with("file-queue-divider"), false);
                if response.dragged()
                    && let Some(pos) = response.interact_pointer_pos()
                {
                    self.queue.collapsed = false;
                    self.queue.height = (area.bottom() - pos.y).clamp(
                        140.,
                        (area.height() - theme::FILES_PANE_MIN_HEIGHT - theme::FILES_SPLITTER)
                            .max(140.),
                    );
                }
                let queue_rect = egui::Rect::from_min_max(separator.left_bottom(), area.max);
                if let Some(action) = queue::show(ui, queue_rect, &mut self.queue, &s, &self.alias)
                {
                    match action {
                        queue::Action::Command(c) => self.submit(c),
                        queue::Action::Cancel(key) => self.dialog = Some(Dialog::Cancel(key)),
                    }
                }
                ui.allocate_rect(area, egui::Sense::hover());
            });
        if let Some(device) = &s.takeover {
            match crate::session::controller::takeover::confirmation(ui.ctx(), device) {
                Some(true) => self.submit(Command::Takeover(
                    crate::session::controller::takeover::Approval::confirmed(device),
                )),
                Some(false) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                None => {}
            }
        } else {
            self.dialog(ui.ctx());
        }
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(150));
    }
}
impl Window {
    fn submit(&mut self, c: Command) {
        self.error = self.handle.send(c).err().map(|e| e.to_string());
    }
    fn refresh(&mut self) {
        self.submit(Command::Browse(false, self.local.path.clone()));
        if self.ready {
            self.submit(Command::Browse(true, self.remote.path.clone()));
        }
    }
    fn action(&mut self, remote: bool, a: Action, s: &Snapshot) {
        match a {
            Action::Browse(path) => {
                let pane = if remote {
                    &mut self.remote
                } else {
                    &mut self.local
                };
                pane.visit(&path);
                self.submit(Command::Browse(remote, path));
            }
            Action::History(path) => self.submit(Command::Browse(remote, path)),
            Action::Create => {
                self.dialog = Some(Dialog::Create(
                    remote,
                    if remote {
                        self.remote.path.clone()
                    } else {
                        self.local.path.clone()
                    },
                    String::new(),
                ))
            }
            Action::Rename(e) => self.dialog = Some(Dialog::Rename(remote, e.full_path, e.name)),
            Action::Delete(e) => self.dialog = Some(Dialog::Delete(remote, e)),
            Action::Transfer => {
                let pane = if remote { &self.remote } else { &self.local };
                let entries = if remote {
                    &s.remote.entries
                } else {
                    &s.local.entries
                };
                let target = if remote {
                    &self.local.path
                } else {
                    &self.remote.path
                };
                let destination = if remote { &s.local } else { &s.remote };
                if destination.busy || destination.error.is_some() || destination.path.is_empty() {
                    self.error = Some("请先打开可用的目标文件夹".into());
                    return;
                }
                if target == ":/" {
                    self.error = Some("请先打开目标文件夹".into());
                    return;
                }
                let records = entries
                    .iter()
                    .filter(|e| pane.selection.contains(&e.full_path))
                    .map(|e| {
                        Record::new(
                            if remote {
                                Direction::Download
                            } else {
                                Direction::Upload
                            },
                            e.full_path.clone(),
                            target.clone(),
                            self.policy,
                        )
                    })
                    .collect::<Vec<_>>();
                if !records.is_empty() {
                    self.dialog = Some(Dialog::Transfer(records));
                }
            }
        }
    }
    fn dialog(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.dialog else { return };
        let (yes, no) = operation_prompt(ctx, d, &mut self.policy);
        if yes {
            let d = self.dialog.take().unwrap();
            match d {
                Dialog::Transfer(mut r) => {
                    for r in &mut r {
                        r.policy = self.policy;
                    }
                    self.submit(Command::Add(r));
                }
                Dialog::Create(a, b, c) => self.submit(Command::Create(a, b, c)),
                Dialog::Rename(a, b, c) => self.submit(Command::Rename(a, b, c)),
                Dialog::Delete(a, e) => {
                    self.submit(Command::Delete(a, e.full_path, e.entry_type < 4))
                }
                Dialog::Cancel(k) => self.submit(Command::Cancel(k)),
            }
        } else if no {
            self.dialog = None;
        }
    }
}
fn operation_prompt(ctx: &egui::Context, d: &mut Dialog, policy: &mut i32) -> (bool, bool) {
    let mut yes = false;
    let mut no = false;
    let response = egui::Modal::new(egui::Id::new("file-operation"))
        .frame(controls::dialog_frame())
        .show(ctx, |ui| {
            ui.set_width(theme::FILES_DIALOG_WIDTH);
            let title = match d {
                Dialog::Transfer(r) => {
                    if r.first().is_some_and(|r| r.direction == Direction::Upload) {
                        "发送文件"
                    } else {
                        "接收文件"
                    }
                }
                Dialog::Create(..) => "新建文件夹",
                Dialog::Rename(..) => "重命名",
                Dialog::Delete(..) => "删除文件？",
                Dialog::Cancel(..) => "取消传输？",
            };
            no = crate::ui::controls::dialog_header(
                ui,
                title,
                match d {
                    Dialog::Create(..) | Dialog::Rename(..) => controls::DialogIcon::Edit,
                    Dialog::Cancel(..) | Dialog::Delete(..) => controls::DialogIcon::Warning,
                    Dialog::Transfer(..) => controls::DialogIcon::Files,
                },
                true,
            );
            match d {
                Dialog::Transfer(records) => {
                    ui.label(format!("{} 个项目", records.len()));
                    egui::ScrollArea::vertical()
                        .id_salt("file-transfer-confirm-items")
                        .max_height(120.)
                        .show(ui, |ui| {
                            for record in records.iter() {
                                ui.add(egui::Label::new(&record.source).truncate());
                            }
                        });
                    if let Some(r) = records.first() {
                        ui.label(format!("目标：{}", r.destination));
                    }
                    ui.add_space(12.);
                    egui::ComboBox::from_id_salt("file-conflict")
                        .selected_text(match *policy {
                            1 => "同名时覆盖",
                            3 => "同名时跳过",
                            _ => "同名时保留两份",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(policy, 2, "同名时保留两份");
                            ui.selectable_value(policy, 1, "同名时覆盖");
                            ui.selectable_value(policy, 3, "同名时跳过");
                        });
                }
                Dialog::Create(_, path, name) => {
                    ui.label(path.as_str());
                    ui.add_sized(
                        [ui.available_width(), theme::CONTROL_HEIGHT],
                        controls::singleline(name, theme::CONTROL_HEIGHT),
                    );
                }
                Dialog::Rename(_, path, name) => {
                    ui.label(path.as_str());
                    ui.add_sized(
                        [ui.available_width(), theme::CONTROL_HEIGHT],
                        controls::singleline(name, theme::CONTROL_HEIGHT),
                    );
                }
                Dialog::Delete(remote, e) => {
                    ui.label(format!(
                        "{}：{}",
                        if *remote { "远端" } else { "本机" },
                        e.full_path
                    ));
                    ui.label(if e.entry_type < 4 {
                        "将永久删除此文件夹及其中全部内容。"
                    } else {
                        "将永久删除此文件，无法撤销。"
                    });
                }
                Dialog::Cancel(_) => {
                    ui.label("停止此任务并清理未完成的临时文件。已完成的文件会保留。");
                }
            }
            let (accept, dismiss) = crate::ui::controls::dialog_actions(
                ui,
                Some(
                    crate::ui::controls::DialogAction::new("确认")
                        .danger(matches!(d, Dialog::Delete(..))),
                ),
                Some("取消"),
            );
            yes = accept;
            no |= dismiss;
        });
    (yes, no || response.should_close())
}

fn size(n: u64) -> String {
    if n >= 1 << 30 {
        format!("{:.1} GiB", n as f64 / (1u64 << 30) as f64)
    } else if n >= 1 << 20 {
        format!("{:.1} MiB", n as f64 / (1u64 << 20) as f64)
    } else if n >= 1024 {
        format!("{:.1} KiB", n as f64 / 1024.)
    } else {
        format!("{n} B")
    }
}
