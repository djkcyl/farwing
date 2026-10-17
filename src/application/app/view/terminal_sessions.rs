//! 终端会话页：列出被控端与本机正在跑的 shell，并接到本机终端里。
//!
//! 手机端 UU 打开「终端」时会话会出现在这里；本机也可以自己新建，不必等
//! 远程连接。页面不随本地终端关闭而结束会话。
use super::*;

impl DeviceCenterApp {
    pub(super) fn terminal_sessions_page(&mut self, ui: &mut egui::Ui) {
        use crate::features::host::terminal;
        let sessions = terminal::local_sessions();
        // Where this machine cannot host a local shell (no local attach on
        // this platform, root, SYSTEM), the page still lists and ends the
        // controller's sessions but offers nothing that is bound to fail.
        let local_refusal = terminal::local_unavailable();
        // The host's 「允许主控接入本机终端会话」, which decides whether the
        // sessions opened here are visible to a controller.
        let open_to_controller = self
            .host
            .as_ref()
            .is_some_and(|host| host.terminal_local_allowed());
        // 会话可能在另一头被改名、附着、结束，前台命令也在变，页面每秒刷一次。
        ui.ctx().request_repaint_after(Duration::from_secs(1));
        ui.horizontal(|ui| {
            ui.label(RichText::new("终端会话").size(theme::TITLE).strong());
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if local_refusal.is_none() && ui.add(primary("新建会话")).clicked() {
                    match terminal::create_local(None) {
                        Ok(handle) => {
                            if let Err(error) = terminal::open_local_terminal(handle) {
                                self.status = StatusMessage::error(format!("{error:#}"));
                            }
                        }
                        Err(error) => self.status = StatusMessage::error(error.to_string()),
                    }
                }
                if ui
                    .add_enabled(
                        !sessions.is_empty(),
                        crate::ui::controls::secondary("结束全部"),
                    )
                    .clicked()
                {
                    terminal::end_all();
                }
            });
        });
        ui.add_space(4.0);
        let intro = match local_refusal {
            None => {
                "手机端 UU 打开「终端」后，shell 会出现在这里；也可以自己新建一个。\
                 点「在本地打开」用系统终端接上，本机键盘与手机端操作的是同一个 shell；\
                 关掉本地终端只会断开这一端，会话继续运行。\
                 本机和主控同时接入时，终端按两边较小的尺寸显示，较大的一端会留白。"
            }
            Some(_) => "手机端 UU 打开「终端」后，shell 会出现在这里，可以在这里结束。",
        };
        ui.label(RichText::new(intro).small().color(MUTED));
        if let Some(reason) = local_refusal {
            ui.label(RichText::new(reason).small().color(AMBER));
        } else if open_to_controller {
            ui.label(
                RichText::new("主控能看到并接入你在这里新建的会话，包括屏幕内容和之后的输入。")
                    .small()
                    .color(AMBER),
            );
        } else {
            ui.label(
                RichText::new(
                    "本机会话不对主控开放。要让主控接入，到「被控设置 → 访问权限」\
                     打开「终端」下的「允许主控接入本机终端会话」。",
                )
                .small()
                .color(MUTED),
            );
        }
        ui.add_space(14.0);
        if sessions.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("现在没有终端会话").strong());
                ui.label(
                    RichText::new(if local_refusal.is_none() {
                        "点右上角「新建会话」，或在手机端连上本机后打开终端。"
                    } else {
                        "在手机端连上本机后打开终端。"
                    })
                    .small()
                    .color(MUTED),
                );
            });
            return;
        }
        for session in sessions {
            let (state, state_color) = match session.state {
                "running" => ("运行中", GREEN),
                "detached" => ("已分离", MUTED),
                _ => ("已退出", RED),
            };
            crate::ui::controls::section_frame()
                .inner_margin(egui::Margin::symmetric(16, 12))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&session.name).strong());
                        ui.label(RichText::new(state).small().color(state_color));
                        if session.opened_here {
                            if session.remote {
                                ui.label(RichText::new("主控已接入").small().color(AMBER));
                            } else if open_to_controller {
                                ui.label(RichText::new("主控可见").small().color(AMBER));
                            } else {
                                ui.label(RichText::new("主控不可见").small().color(MUTED));
                            }
                        } else {
                            ui.label(RichText::new("主控打开").small().color(MUTED));
                            if session.remote {
                                ui.label(RichText::new("主控已连接").small().color(BLUE));
                            }
                        }
                        if session.local {
                            ui.label(RichText::new("本机已接入").small().color(GREEN));
                        }
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if local_refusal.is_none()
                                && ui
                                    .add_enabled(
                                        session.state != "exited",
                                        crate::ui::controls::secondary("在本地打开"),
                                    )
                                    .clicked()
                                && let Err(error) = terminal::open_local_terminal(session.handle)
                            {
                                self.status = StatusMessage::error(format!("{error:#}"));
                            }
                            if ui.add(crate::ui::controls::secondary("结束")).clicked()
                                && !terminal::kill_local(session.handle, session.opened_here)
                            {
                                self.status = StatusMessage::error("会话已经不存在");
                            }
                        });
                    });
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "{} · 编号 {} · 创建于 {}",
                                session.shell,
                                session.handle,
                                stamp(session.created_ms)
                            ))
                            .small()
                            .color(MUTED),
                        );
                        if let Some(foreground) = &session.foreground {
                            ui.label(
                                RichText::new(format!("正在运行 {}", foreground.name))
                                    .small()
                                    .color(AMBER),
                            )
                            .on_hover_text(&foreground.command);
                        } else if session.state != "exited" && terminal::foreground_supported() {
                            ui.label(RichText::new("空闲").small().color(MUTED));
                        }
                    });
                });
            ui.add_space(8.0);
        }
    }
}

/// 毫秒时间戳按本地时区显示，与诊断页的写法一致。
fn stamp(milliseconds: i64) -> String {
    chrono::DateTime::from_timestamp_millis(milliseconds)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "未知".into())
}
