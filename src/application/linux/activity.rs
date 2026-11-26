use super::*;
use farwing_protocol::api::{AccountEvent, ConnectionRecord, Notice, NoticeSetting, RecordOutcome};

const NOTICE_PAGE: usize = 50;
const NOTICE_GROUPS: [(&str, &[&str]); 5] = [
    (
        "security",
        &[
            "login.new_device",
            "login.failures",
            "security.changed",
            "device.removed",
        ],
    ),
    (
        "connections",
        &[
            "connection.incoming",
            "connection.refused",
            "connection.own",
        ],
    ),
    (
        "friends",
        &[
            "friend.request",
            "friend.accepted",
            "friend.grants",
            "friend.removed",
        ],
    ),
    (
        "devices",
        &["power.remote", "device.offline", "access.changed"],
    ),
    (
        "admin",
        &[
            "admin.certificate",
            "admin.node_offline",
            "admin.registered",
        ],
    ),
];

#[derive(Default)]
struct Listing<T> {
    items: Vec<T>,
    more: bool,
    loaded: bool,
}

impl<T> Listing<T> {
    fn finish(&mut self, before: Option<i64>, items: Vec<T>, limit: usize) {
        self.loaded = true;
        self.more = items.len() == limit;
        if before.is_none() {
            self.items = items;
        } else {
            self.items.extend(items);
        }
    }
}

#[derive(Default)]
pub(super) struct State {
    events_tab: bool,
    connections: Listing<ConnectionRecord>,
    events: Listing<AccountEvent>,
    notices: Listing<Notice>,
    settings: Option<Vec<NoticeSetting>>,
}

pub(in crate::application) enum Update {
    Connections(Option<i64>, Vec<ConnectionRecord>),
    Events(Option<i64>, Vec<AccountEvent>),
    Notices(Option<i64>, Vec<Notice>, Option<Vec<NoticeSetting>>),
    Settings(Vec<NoticeSetting>),
}

impl State {
    pub(super) fn finish(&mut self, update: Update) {
        let record_page = crate::account::api::RECORD_PAGE;
        match update {
            Update::Connections(before, list) => self.connections.finish(before, list, record_page),
            Update::Events(before, list) => self.events.finish(before, list, record_page),
            Update::Notices(before, list, settings) => {
                self.notices.finish(before, list, NOTICE_PAGE);
                if settings.is_some() {
                    self.settings = settings;
                }
            }
            Update::Settings(settings) => self.settings = Some(settings),
        }
    }
}

impl DeviceCenter {
    pub(super) fn refresh_records(&mut self, before: Option<i64>) {
        let events = self.center.activity.events_tab;
        self.request_center(move |client| async move {
            let update = if events {
                let list = client
                    .request(|api| async move { api.account_events(before).await })
                    .await?;
                Update::Events(before, list)
            } else {
                let list = client
                    .request(|api| async move { api.records(None, before).await })
                    .await?;
                Update::Connections(before, list)
            };
            Ok(super::Update::Activity(update))
        });
    }

    pub(super) fn records_page(&mut self, ui: &mut egui::Ui) {
        let text = &t().records;
        let previous = self.center.activity.events_tab;
        ui.add_enabled_ui(!self.busy, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(
                    &mut self.center.activity.events_tab,
                    false,
                    text.tab_connections,
                );
                ui.selectable_value(&mut self.center.activity.events_tab, true, text.tab_events);
            });
        });
        if previous != self.center.activity.events_tab {
            self.refresh_records(None);
        }
        ui.add_space(8.0);
        let state = &self.center.activity;
        let more = if state.events_tab {
            for event in &state.events.items {
                event_card(ui, event);
            }
            if state.events.loaded && state.events.items.is_empty() {
                device_list::empty(ui, text.empty);
            }
            state
                .events
                .more
                .then(|| state.events.items.last().map(|value| value.id))
                .flatten()
        } else {
            for record in &state.connections.items {
                record_card(ui, record);
            }
            if state.connections.loaded && state.connections.items.is_empty() {
                device_list::empty(ui, text.empty);
            }
            state
                .connections
                .more
                .then(|| state.connections.items.last().map(|value| value.id))
                .flatten()
        };
        if let Some(before) = more
            && ui
                .add_enabled(!self.busy, controls::secondary(text.load_more))
                .clicked()
        {
            self.refresh_records(Some(before));
        }
    }

    pub(super) fn refresh_notices(&mut self, before: Option<i64>) {
        self.request_center(move |client| async move {
            let list = client
                .request(|api| async move { api.notices(before, NOTICE_PAGE).await })
                .await?;
            let settings = if before.is_none() {
                let settings = client
                    .request(|api| async move { api.notice_settings().await })
                    .await?;
                if list.unread > 0 {
                    client
                        .request(|api| async move { api.read_notices().await })
                        .await?;
                }
                Some(settings.settings)
            } else {
                None
            };
            Ok(super::Update::Activity(Update::Notices(
                before,
                list.notices,
                settings,
            )))
        });
    }

    pub(super) fn notices_page(&mut self, ui: &mut egui::Ui) {
        let text = &t().notices;
        let state = &self.center.activity;
        for notice in &state.notices.items {
            let (title, body) = (text.text)(&notice.kind, &notice.data);
            controls::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(if notice.read {
                    RichText::new(title)
                } else {
                    RichText::new(title).strong()
                });
                if !body.is_empty() {
                    ui.label(body);
                }
                small(ui, time(notice.created_at));
            });
        }
        if state.notices.loaded && state.notices.items.is_empty() {
            device_list::empty(ui, text.empty);
        }
        if state.notices.more
            && ui
                .add_enabled(!self.busy, controls::secondary(text.load_more))
                .clicked()
        {
            let before = state.notices.items.last().map(|notice| notice.id);
            self.refresh_notices(before);
        }
        let mut save = None;
        if let Some(settings) = &self.center.activity.settings {
            device_list::heading(ui, text.settings, None, true, |_| {});
            controls::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                for (group, kinds) in NOTICE_GROUPS {
                    let mut shown = settings
                        .iter()
                        .filter(|s| kinds.contains(&s.kind.as_str()))
                        .peekable();
                    if shown.peek().is_none() {
                        continue;
                    }
                    ui.label(RichText::new((text.group)(group)).strong());
                    for setting in shown {
                        let mut on = setting.enabled;
                        ui.horizontal(|ui| {
                            ui.label((text.kind)(&setting.kind));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui
                                    .add_enabled_ui(!self.busy, |ui| controls::switch(ui, &mut on))
                                    .inner
                                    .clicked()
                                {
                                    save = Some((setting.kind.clone(), on));
                                }
                            });
                        });
                    }
                    ui.add_space(4.0);
                }
            });
        }
        if let Some((kind, enabled)) = save {
            self.request_center(move |client| async move {
                let settings = client
                    .request(|api| async move { api.save_notice_setting(&kind, enabled).await })
                    .await?;
                Ok(super::Update::Activity(Update::Settings(settings.settings)))
            });
        }
    }
}

fn small(ui: &mut egui::Ui, text: impl Into<String>) {
    ui.label(RichText::new(text.into()).small().color(theme::MUTED));
}

fn joined(parts: &[&str]) -> String {
    parts
        .iter()
        .copied()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

fn wire_name(value: impl serde::Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        _ => String::new(),
    }
}

fn record_card(ui: &mut egui::Ui, record: &ConnectionRecord) {
    use farwing_protocol::api::Purpose;
    let text = &t().records;
    let party = &record.controller;
    let account = if !party.nickname.is_empty() {
        party.nickname.as_str()
    } else if !party.username.is_empty() {
        party.username.as_str()
    } else {
        text.guest
    };
    let title = format!(
        "{} → {}",
        joined(&[account, &party.name]),
        if record.host.name.is_empty() {
            "—"
        } else {
            &record.host.name
        }
    );
    controls::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::CollapsingHeader::new(title)
            .id_salt(record.id)
            .show(ui, |ui| {
                let end = record.ended_at.map(time).unwrap_or_default();
                ui.label(format!(
                    "{}: {}",
                    text.time,
                    joined(&[&time(record.started_at), &end])
                ));
                let reason = (text.end_reason)(&record.end_reason);
                if !reason.is_empty() {
                    ui.label(format!("{}: {}", text.ended, reason));
                }
                if !record.ip.is_empty() {
                    ui.label(format!("{}: {}", text.source, record.ip));
                }
                if let Some(viewer) = &record.viewer {
                    let route = (text.route)(&viewer.route);
                    if record.direct || !route.is_empty() {
                        ui.label(format!(
                            "{}: {}",
                            text.path,
                            joined(&[
                                if record.direct { text.direct } else { route },
                                &viewer.relay
                            ])
                        ));
                    }
                    if viewer.seconds > 0 {
                        ui.label(format!(
                            "{}: {}",
                            text.picture,
                            joined(&[
                                &format!("{}×{}", viewer.width, viewer.height),
                                &(text.fps)(&format!("{:.1}", viewer.fps)),
                                &(text.rtt)(viewer.rtt_ms),
                                &(text.stalls)(viewer.stalls),
                                &(text.reconnects)(viewer.reconnects.max(record.reconnects)),
                            ])
                        ));
                    }
                }
                if let Some(host) = &record.host_report {
                    if record.purpose == Purpose::Desktop {
                        ui.label(format!(
                            "{}: {}",
                            text.control,
                            if host.controlled {
                                text.controlled
                            } else {
                                text.view_only
                            }
                        ));
                    }
                    if host.camera {
                        ui.label(format!("{}: {}", text.camera, text.camera_viewed));
                    }
                    if !host.files.is_empty() {
                        ui.label(text.files);
                        for file in &host.files {
                            ui.label(joined(&[
                                if file.upload {
                                    text.upload
                                } else {
                                    text.download
                                },
                                &file.name,
                                &format!("{} B", file.bytes),
                                (text.file_outcome)(&wire_name(file.outcome)),
                            ]));
                        }
                        if host.more_files > 0 {
                            small(ui, (text.more_files)(host.more_files));
                        }
                    }
                    if !host.ports.is_empty() {
                        ui.label(format!("{}: {}", text.ports, host.ports.join(", ")));
                    }
                }
            });
        let outcome = (text.outcome)(&wire_name(record.outcome));
        let status = if record.outcome == RecordOutcome::Open {
            text.open.into()
        } else if !outcome.is_empty() {
            outcome.into()
        } else if let Some(end) = record.ended_at {
            (text.duration_seconds)((end - record.started_at).max(0) as u64)
        } else {
            String::new()
        };
        small(
            ui,
            joined(&[
                &time(record.started_at),
                (text.purpose)(&wire_name(record.purpose)),
                (text.relation)(&wire_name(record.relation)),
                (text.admission)(&record.admission),
                &status,
            ]),
        );
    });
}

fn event_card(ui: &mut egui::Ui, event: &AccountEvent) {
    let text = &t().records;
    controls::card().show(ui, |ui| {
        ui.set_width(ui.available_width());
        let name = (text.event)(&event.kind);
        ui.label(RichText::new(if name.is_empty() { &event.kind } else { name }).strong());
        let field = |name: &str| {
            event
                .detail
                .get(name)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
        };
        let previous = if field("previous").is_empty() {
            String::new()
        } else {
            (text.previous)(field("previous"))
        };
        let friend = if field("friend").is_empty() {
            String::new()
        } else {
            (text.friend)(field("friend"))
        };
        let other = if field("other").is_empty() {
            String::new()
        } else {
            (text.other_device)(field("other"))
        };
        let actor = if event.actor.is_empty() {
            String::new()
        } else {
            (text.by)(&event.actor)
        };
        small(
            ui,
            joined(&[
                &time(event.at),
                field("device"),
                &previous,
                &friend,
                &other,
                (text.power_action)(field("action")),
                &actor,
                &event.ip,
            ]),
        );
        if !event.ok {
            ui.colored_label(theme::RED, text.failed);
        }
    });
}
