//! Records: this account's connections and account events
//! (`docs/connection-records.md`); a connection opens to its details.
use super::cards::{self, Tone};
use super::*;
use crate::application::app::records::{RecordOperation, RecordsTab};
use farwing_protocol::api::{AccountEvent, ConnectionRecord, RecordOutcome, RecordParty};

fn small(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).size(theme::SMALL).color(MUTED)).wrap());
}

fn duration(seconds: i64) -> String {
    let text = &t().records;
    let s = seconds.max(0) as u64;
    match (s / 3600, (s % 3600) / 60) {
        (0, 0) => (text.duration_seconds)(s),
        (0, m) => (text.duration_minutes)(m),
        (h, m) => (text.duration_hours)(h, m),
    }
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// `serde` name of a wire enum (`own`, `desktop`).
fn wire_name<T: serde::Serialize>(value: T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

fn party(p: &RecordParty) -> String {
    let account = if p.nickname.is_empty() { &p.username } else { &p.nickname };
    let account = if account.is_empty() { t().records.guest } else { account.as_str() };
    if p.name.is_empty() {
        account.to_owned()
    } else {
        format!("{account} · {}", p.name)
    }
}

fn joined(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" · ")
}

/// One label and its value.
fn row(ui: &mut egui::Ui, label: &str, value: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal_top(|ui| {
        ui.add_sized(
            [96.0, 18.0],
            egui::Label::new(RichText::new(label).size(theme::SMALL).color(MUTED)),
        );
        ui.vertical(value);
    });
}

fn record_details(ui: &mut egui::Ui, r: &ConnectionRecord) {
    let text = &t().records;
    let end = r.ended_at.unwrap_or_default();
    ui.add_space(8.0);
    ui.separator();
    ui.add_space(4.0);
    row(ui, text.time, |ui| {
        let mut time = cards::when(r.started_at);
        if end > r.started_at {
            time.push_str(&format!(" – {}", cards::when(end)));
        }
        ui.label(time);
    });
    let reason = (text.end_reason)(&r.end_reason);
    if r.outcome == RecordOutcome::Ended && !reason.is_empty() {
        row(ui, text.ended, |ui| {
            ui.label(reason);
        });
    }
    if !r.ip.is_empty() || !r.direct {
        let client = if r.controller.platform == farwing_protocol::Platform::Web {
            text.web
        } else {
            text.app
        };
        row(ui, text.source, |ui| {
            ui.label(joined(&[&r.ip, client]));
        });
    }
    let route = r.viewer.as_ref().map_or("", |v| (text.route)(&v.route));
    if r.direct || !route.is_empty() {
        let relay = r.viewer.as_ref().map(|v| v.relay.to_uppercase()).unwrap_or_default();
        row(ui, text.path, |ui| {
            ui.label(joined(&[if r.direct { text.direct } else { route }, &relay]));
        });
    }
    if let Some(v) = r.viewer.as_ref().filter(|v| v.seconds > 0) {
        let size = if v.width > 0 { format!("{}×{}", v.width, v.height) } else { String::new() };
        let fps = (text.fps)(&format!("{:.1}", v.fps));
        let rtt = if v.rtt_ms > 0 { (text.rtt)(v.rtt_ms) } else { String::new() };
        let reconnects = v.reconnects.max(r.reconnects);
        let reconnects = if reconnects > 0 { (text.reconnects)(reconnects) } else { String::new() };
        row(ui, text.picture, |ui| {
            ui.label(joined(&[&size, &fps, &rtt, &(text.stalls)(v.stalls), &reconnects]));
        });
    }
    if let Some(h) = &r.host_report {
        if r.purpose == farwing_protocol::api::Purpose::Desktop {
            row(ui, text.control, |ui| {
                ui.label(if h.controlled { text.controlled } else { text.view_only });
            });
            if h.camera {
                row(ui, text.camera, |ui| {
                    ui.label(text.camera_viewed);
                });
            }
        }
        if !h.files.is_empty() {
            row(ui, text.files, |ui| {
                for f in &h.files {
                    let outcome = (text.file_outcome)(&wire_name(f.outcome));
                    let direction = if f.upload { text.upload } else { text.download };
                    ui.horizontal_wrapped(|ui| {
                        ui.label(format!("{direction} · {} · {}", f.name, bytes(f.bytes)));
                        let done = f.outcome == farwing_protocol::api::FileOutcome::Done;
                        ui.label(RichText::new(outcome).color(if done { MUTED } else { AMBER }));
                    });
                }
                if h.more_files > 0 {
                    small(ui, &(text.more_files)(h.more_files));
                }
            });
        }
        if !h.ports.is_empty() {
            row(ui, text.ports, |ui| {
                ui.label(RichText::new(h.ports.join(", ")).monospace());
            });
        }
    }
}

fn record_card(ui: &mut egui::Ui, r: &ConnectionRecord, open: bool) -> bool {
    let text = &t().records;
    let mut toggled = false;
    ui.push_id(r.id, |ui| {
        cards::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            let header = ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.set_max_width((ui.available_width() - 120.0).max(200.0));
                    ui.label(format!(
                        "{}  →  {}",
                        party(&r.controller),
                        if r.host.name.is_empty() { "—" } else { &r.host.name }
                    ));
                    let route = r.viewer.as_ref().map_or("", |v| (text.route)(&v.route));
                    small(
                        ui,
                        &joined(&[
                            &cards::when(r.started_at),
                            (text.purpose)(&wire_name(r.purpose)),
                            (text.relation)(&wire_name(r.relation)),
                            (text.admission)(&r.admission),
                            if r.direct { text.direct } else { route },
                            if r.controller.platform == farwing_protocol::Platform::Web {
                                text.web
                            } else {
                                ""
                            },
                        ]),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    let refused = (text.outcome)(&wire_name(r.outcome));
                    if r.outcome == RecordOutcome::Open {
                        cards::chip(ui, text.open, GREEN);
                    } else if !refused.is_empty() {
                        cards::chip(ui, refused, if r.outcome == RecordOutcome::Busy { AMBER } else { RED });
                    } else {
                        small(ui, &duration(r.ended_at.unwrap_or(r.started_at) - r.started_at));
                    }
                });
            });
            let response = ui
                .interact(header.response.rect, ui.id().with("head"), Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            toggled = response.clicked();
            if open {
                record_details(ui, r);
            }
        });
    });
    toggled
}

fn event_detail(e: &AccountEvent) -> String {
    let text = &t().records;
    let field = |key: &str| e.detail.get(key).and_then(|v| v.as_str()).unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    if !field("device").is_empty() {
        parts.push(field("device").to_owned());
    }
    if !field("previous").is_empty() {
        parts.push((text.previous)(field("previous")));
    }
    if !field("friend").is_empty() {
        parts.push((text.friend)(field("friend")));
    }
    if !field("other").is_empty() {
        parts.push((text.other_device)(field("other")));
    }
    let action = (text.power_action)(field("action"));
    if !action.is_empty() {
        parts.push(action.to_owned());
    }
    parts.join(" · ")
}

fn event_card(ui: &mut egui::Ui, e: &AccountEvent) {
    let text = &t().records;
    ui.push_id(e.id, |ui| {
        cards::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    let kind = (text.event)(&e.kind);
                    ui.label(if kind.is_empty() { e.kind.as_str() } else { kind });
                    let by = if e.actor.is_empty() { String::new() } else { (text.by)(&e.actor) };
                    small(ui, &joined(&[&cards::when(e.at), &event_detail(e), &by, &e.ip]));
                });
                if !e.ok {
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        cards::chip(ui, text.failed, RED);
                    });
                }
            });
        });
    });
}

impl DeviceCenterApp {
    pub(super) fn records_page(&mut self, ui: &mut egui::Ui) {
        let text = &t().records;
        let tab = self.records.tab;
        let shown = match tab {
            RecordsTab::Connections => self.records.connections.is_some(),
            RecordsTab::Events => self.records.events.is_some(),
        };
        if !shown && !self.records.loading && self.records.error.is_none() {
            self.request_records(match tab {
                RecordsTab::Connections => RecordOperation::Connections(None),
                RecordsTab::Events => RecordOperation::Events(None),
            });
        }
        if self.records.loading {
            ui.ctx().request_repaint_after(Duration::from_millis(150));
        }
        let loading = self.records.loading;
        let mut refresh = false;
        let mut more = false;
        crate::ui::controls::page_scroll("records-page").show(ui, |ui| {
            cards::page_header(ui, text.page, "", |ui| {
                if loading {
                    ui.add_sized(
                        [theme::CONTROL_HEIGHT, theme::CONTROL_HEIGHT],
                        egui::Spinner::new().size(18.0),
                    );
                } else {
                    refresh = super::assist_components::glyph(ui, Icon::Refresh, t().common.refresh, MUTED)
                        .clicked();
                }
            });
            if cards::tabs(
                ui,
                &mut self.records.tab,
                &[
                    (RecordsTab::Connections, text.tab_connections),
                    (RecordsTab::Events, text.tab_events),
                ],
            ) {
                self.records.error = None;
            }
            ui.add_space(12.0);
            if let Some(error) = &self.records.error {
                cards::banner(ui, error, Tone::Warning);
            }
            match self.records.tab {
                RecordsTab::Connections => {
                    let Some(list) = &self.records.connections else {
                        if loading {
                            small(ui, text.loading);
                        }
                        return;
                    };
                    if list.is_empty() {
                        super::device_list::empty(ui, text.empty);
                        return;
                    }
                    let mut toggled = None;
                    for r in list {
                        if record_card(ui, r, self.records.open.contains(&r.id)) {
                            toggled = Some(r.id);
                        }
                    }
                    if let Some(id) = toggled
                        && !self.records.open.remove(&id)
                    {
                        self.records.open.insert(id);
                    }
                    if self.records.more_connections {
                        ui.add_space(8.0);
                        ui.vertical_centered(|ui| {
                            more = ui
                                .add_enabled(!loading, crate::ui::controls::secondary(text.load_more))
                                .clicked();
                        });
                    }
                }
                RecordsTab::Events => {
                    let Some(list) = &self.records.events else {
                        if loading {
                            small(ui, text.loading);
                        }
                        return;
                    };
                    if list.is_empty() {
                        super::device_list::empty(ui, text.empty);
                        return;
                    }
                    for e in list {
                        event_card(ui, e);
                    }
                    if self.records.more_events {
                        ui.add_space(8.0);
                        ui.vertical_centered(|ui| {
                            more = ui
                                .add_enabled(!loading, crate::ui::controls::secondary(text.load_more))
                                .clicked();
                        });
                    }
                }
            }
            ui.add_space(16.0);
        });
        if refresh {
            match self.records.tab {
                RecordsTab::Connections => self.records.connections = None,
                RecordsTab::Events => self.records.events = None,
            }
            self.records.error = None;
        }
        if more {
            let operation = match self.records.tab {
                RecordsTab::Connections => RecordOperation::Connections(
                    self.records.connections.as_ref().and_then(|l| l.last()).map(|r| r.id),
                ),
                RecordsTab::Events => RecordOperation::Events(
                    self.records.events.as_ref().and_then(|l| l.last()).map(|e| e.id),
                ),
            };
            self.request_records(operation);
        }
    }
}
