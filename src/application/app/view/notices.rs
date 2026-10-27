//! Notifications: the account's list (opening the page reads them) and
//! which kinds it receives (`docs/notifications.md`).
use super::cards::{self, Tone};
use super::*;
use crate::application::app::notices::NoticeOperation;
use farwing_protocol::api::Notice;

/// The settings' groups, in order.
const GROUPS: [(&str, &[&str]); 5] = [
    ("security", &["login.new_device", "login.failures", "security.changed", "device.removed"]),
    ("connections", &["connection.incoming", "connection.refused", "connection.own"]),
    ("friends", &["friend.request", "friend.accepted", "friend.grants", "friend.removed"]),
    ("devices", &["power.remote", "device.offline", "access.changed"]),
    ("admin", &["admin.certificate", "admin.node_offline", "admin.registered"]),
];

fn small(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(theme::SMALL).color(MUTED)).wrap());
}

fn notice_card(ui: &mut egui::Ui, n: &Notice) {
    let (title, body) = (t().notices.text)(&n.kind, &n.data);
    ui.push_id(n.id, |ui| {
        cards::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.set_max_width((ui.available_width() - 140.0).max(200.0));
                    let title = RichText::new(title);
                    ui.label(if n.read { title } else { title.strong() });
                    if !body.is_empty() {
                        small(ui, &body);
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    small(ui, &cards::when(n.created_at));
                });
            });
        });
    });
}

impl DeviceCenterApp {
    pub(super) fn notices_page(&mut self, ui: &mut egui::Ui) {
        let text = &t().notices;
        if self.notices.list.is_none() && !self.notices.loading && self.notices.error.is_none() {
            self.request_notices(NoticeOperation::Page(None));
        } else if self.notices.list.is_some()
            && self.notices.settings.is_none()
            && !self.notices.loading
            && self.notices.error.is_none()
        {
            self.request_notices(NoticeOperation::Settings);
        }
        if self.notices.loading {
            ui.ctx().request_repaint_after(Duration::from_millis(150));
        }
        let loading = self.notices.loading;
        let mut more = false;
        let mut saved = None;
        crate::ui::controls::page_scroll("notices-page").show(ui, |ui| {
            cards::page_header(ui, text.page, "", |_| {});
            if let Some(error) = &self.notices.error {
                cards::banner(ui, error, Tone::Warning);
            }
            match &self.notices.list {
                None if loading => small(ui, text.loading),
                None => {}
                Some(list) if list.is_empty() => super::device_list::empty(ui, text.empty),
                Some(list) => {
                    for n in list {
                        notice_card(ui, n);
                    }
                    if self.notices.more {
                        ui.add_space(8.0);
                        ui.vertical_centered(|ui| {
                            more = ui
                                .add_enabled(!loading, crate::ui::controls::secondary(text.load_more))
                                .clicked();
                        });
                    }
                }
            }
            if let Some(settings) = &self.notices.settings {
                cards::section_heading(ui, text.settings, None, |_| {});
                ui.add_space(6.0);
                cards::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    for (group, kinds) in GROUPS {
                        let shown: Vec<_> = settings
                            .iter()
                            .filter(|s| kinds.contains(&s.kind.as_str()))
                            .collect();
                        if shown.is_empty() {
                            continue;
                        }
                        ui.add_space(4.0);
                        small(ui, (text.group)(group));
                        for setting in shown {
                            let mut on = setting.enabled;
                            ui.horizontal(|ui| {
                                ui.label((text.kind)(&setting.kind));
                                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                    if crate::ui::controls::switch(ui, &mut on).clicked() {
                                        saved = Some((setting.kind.clone(), on));
                                    }
                                });
                            });
                        }
                    }
                });
            }
            ui.add_space(16.0);
        });
        if more {
            let before = self.notices.list.as_ref().and_then(|l| l.last()).map(|n| n.id);
            self.request_notices(NoticeOperation::Page(before));
        }
        if let Some((kind, on)) = saved {
            self.request_notices(NoticeOperation::Save(kind, on));
        }
    }
}
