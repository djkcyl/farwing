use super::{
    Rule,
    service::{Command, Handle, new_id},
};
use crate::ui::{controls, theme, window_manager};
use crate::i18n::t;
use anyhow::{Context, Result};
use egui::{RichText, vec2};
use std::sync::Arc;

pub(crate) fn open(
    client: Arc<crate::account::client::AuthenticatedClient>,
    device: crate::account::api::DeviceInfo,
    options: crate::media::ConnectionMediaOptions,
) -> Result<()> {
    crate::account::api::validate_device_id(&device.device_id)?;
    let runtime = tokio::runtime::Handle::current();
    let key = format!("ports:{}:{}", client.device_id(), device.device_id);
    let viewport = egui::ViewportBuilder::default()
        .with_title(format!("{} · {} · {}", crate::APP_NAME, device.alias, t().features.mapping_title))
        .with_icon(crate::ui::branding::icon())
        .with_inner_size(theme::MAPPING_WINDOW_SIZE)
        .with_min_inner_size(theme::MAPPING_WINDOW_MIN);
    window_manager::send(window_manager::Request::Open {
        key,
        config: crate::ui::WindowConfig {
            viewport,
            centered: true,
            notification: false,
        },
        factory: Box::new(move |ctx, _| {
            theme::configure(ctx);
            let _enter = runtime.enter();
            let handle = super::service::start(Arc::clone(&client), device.clone(), options);
            Box::new(PortWindow {
                client,
                alias: device.alias,
                handle,
                editor: Editor::default(),
            })
        }),
    })
}
struct PortWindow {
    client: Arc<crate::account::client::AuthenticatedClient>,
    alias: String,
    handle: Handle,
    editor: Editor,
}
impl crate::ui::App for PortWindow {
    fn ui(&mut self, ui: &mut egui::Ui) {
        if !self.client.is_active() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(theme::MAPPING_MARGIN),
            )
            .show(ui, |ui| {
                self.editor.show(ui, &self.alias, &self.handle);
            });
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(200));
    }
}
#[derive(Default)]
struct Editor {
    form: Option<Form>,
    error: Option<String>,
    delete: Option<Rule>,
}
struct Form {
    id: u64,
    name: String,
    local_addr: String,
    local: String,
    target: String,
    remote: String,
    enabled: bool,
    protocol: super::Protocol,
    new: bool,
}
impl From<&Rule> for Form {
    fn from(r: &Rule) -> Self {
        Self {
            id: r.id,
            name: r.name.clone(),
            local_addr: r.local_addr.to_string(),
            local: r.local_port.to_string(),
            target: r.target.to_string(),
            remote: r.remote_port.to_string(),
            enabled: r.enabled,
            protocol: r.protocol,
            new: false,
        }
    }
}
/// `host:port`, with IPv6 addresses in brackets.
fn target_label(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

impl Form {
    fn rule(&self) -> Result<Rule> {
        let rule = Rule {
            id: self.id,
            name: self.name.trim().into(),
            local_addr: self
                .local_addr
                .trim()
                .parse()
                .context(t().features.mapping_listen_address_format)?,
            local_port: self.local.trim().parse().context(t().features.mapping_local_port_range)?,
            target: self
                .target
                .trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned(),
            remote_port: self.remote.trim().parse().context(t().features.mapping_target_port_range)?,
            enabled: self.enabled,
            protocol: self.protocol,
        };
        rule.validate()?;
        Ok(rule)
    }
}
impl Editor {
    fn submit(&mut self, handle: &Handle, command: Command) -> bool {
        match handle.send(command) {
            Ok(()) => {
                self.error = None;
                true
            }
            Err(error) => {
                self.error = Some(error.to_string());
                false
            }
        }
    }
    fn begin_add(&mut self) {
        self.form = Some(Form {
            id: new_id(),
            name: String::new(),
            local_addr: "127.0.0.1".into(),
            local: String::new(),
            target: "127.0.0.1".into(),
            remote: String::new(),
            enabled: true,
            protocol: super::Protocol::Tcp,
            new: true,
        });
        self.error = None;
    }
    fn show(&mut self, ui: &mut egui::Ui, alias: &str, handle: &Handle) {
        let snapshot = handle.snapshot();
        ui.horizontal(|ui| {
            let left = (ui.available_width() - 210.0).max(200.0);
            ui.allocate_ui_with_layout(
                vec2(left, 32.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.label(RichText::new(t().features.mapping_title).size(theme::DIALOG_TITLE).strong());
                    ui.add_space(8.0);
                    ui.add(egui::Label::new(RichText::new(alias).color(theme::MUTED)).truncate())
                        .on_hover_text(alias);
                },
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut enabled = snapshot.enabled;
                if controls::service_switch(ui, &mut enabled).changed() {
                    handle.set_enabled(enabled);
                }
                ui.add_space(4.0);
                let (label, color) = if !snapshot.enabled {
                    (t().features.mapping_service_off, theme::MUTED)
                } else if snapshot.busy {
                    (t().features.mapping_connecting, theme::MUTED)
                } else if snapshot.connected {
                    (t().features.mapping_service_running, theme::GREEN)
                } else {
                    (t().features.mapping_connect_failed, theme::RED)
                };
                ui.label(RichText::new(label).size(theme::COMPACT_TEXT).color(color));
            });
        });
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(t().features.mapping_rules).strong());
            ui.label(RichText::new(snapshot.rules.len().to_string()).color(theme::MUTED));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if snapshot.enabled
                    && !snapshot.busy
                    && !snapshot.connected
                    && ui.add(controls::quiet_button(t().features.mapping_reconnect)).clicked()
                {
                    self.submit(handle, Command::Retry);
                }
                if !snapshot.rules.is_empty() && ui.add(controls::primary(t().features.mapping_add_rule)).clicked()
                {
                    self.begin_add();
                }
            });
        });
        crate::ui::controls::observe_notice(
            ui.ctx(),
            "mapping-service",
            t().features.mapping_title,
            crate::ui::controls::DialogIcon::Error,
            snapshot.error.as_deref(),
        );
        if let Some(error) = self.error.take() {
            crate::ui::controls::notice(
                ui.ctx(),
                "mapping-operation",
                t().features.mapping_title,
                crate::ui::controls::DialogIcon::Error,
                error,
            );
        }
        ui.add_space(10.0);
        let body_height = (ui.available_height() - 32.0).max(180.0);
        egui::Frame::new()
            .fill(theme::SIDEBAR)
            .stroke(egui::Stroke::new(1.0, theme::LINE))
            .corner_radius(theme::PANEL_RADIUS)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                if snapshot.rules.is_empty() {
                    if controls::mapping_empty(ui, alias, body_height) {
                        self.begin_add();
                    }
                } else {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    controls::mapping_table_header(ui);
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .max_height(body_height - theme::MAPPING_TABLE_HEADER)
                        .show(ui, |ui| {
                            for (index, (rule, status)) in snapshot.rules.iter().enumerate() {
                                ui.push_id(rule.id, |ui| {
                                    let local =
                                        std::net::SocketAddr::new(rule.local_addr, rule.local_port)
                                            .to_string();
                                    let target = format!(
                                        "{} · {}",
                                        rule.protocol.label(),
                                        target_label(&rule.target, rule.remote_port)
                                    );
                                    let (label, color) = if !snapshot.enabled {
                                        (t().features.mapping_service_off.into(), theme::MUTED)
                                    } else if !rule.enabled {
                                        (t().features.mapping_rule_disabled.into(), theme::MUTED)
                                    } else if status.error.is_some() && !status.listening {
                                        (t().features.mapping_listen_error.into(), theme::RED)
                                    } else if !status.listening {
                                        (t().features.mapping_waiting.into(), theme::MUTED)
                                    } else if rule.protocol == super::Protocol::Udp {
                                        (
                                            if status.error.is_some() {
                                                t().features.mapping_udp_error
                                            } else {
                                                t().features.mapping_udp_listening
                                            }
                                            .into(),
                                            if status.error.is_some() {
                                                theme::RED
                                            } else {
                                                theme::GREEN
                                            },
                                        )
                                    } else {
                                        match &status.probe {
                                            super::ProbeStatus::Unknown => {
                                                (t().features.mapping_probe_pending.into(), theme::MUTED)
                                            }
                                            super::ProbeStatus::Reachable { millis, .. } => {
                                                (format!("TCP {:.0} ms", millis), theme::GREEN)
                                            }
                                            super::ProbeStatus::Unreachable(_) => {
                                                (t().features.mapping_tcp_unreachable.into(), theme::RED)
                                            }
                                        }
                                    };
                                    let reachable = if !snapshot.enabled || !status.listening {
                                        None
                                    } else {
                                        match status.probe {
                                            super::ProbeStatus::Reachable { .. } => Some(true),
                                            super::ProbeStatus::Unreachable(_) => Some(false),
                                            _ => None,
                                        }
                                    };
                                    let http = snapshot.enabled
                                        && status.listening
                                        && matches!(
                                            status.probe,
                                            super::ProbeStatus::Reachable { http: true, .. }
                                        );
                                    let url = http.then(|| {
                                        let address = if rule.local_addr.is_unspecified() {
                                            if rule.local_addr.is_ipv4() {
                                                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
                                            } else {
                                                std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
                                            }
                                        } else {
                                            rule.local_addr
                                        };
                                        format!(
                                            "http://{}/",
                                            std::net::SocketAddr::new(address, rule.local_port)
                                        )
                                    });
                                    let detail = if status.listening {
                                        (t().features.mapping_connections)(status.connections)
                                    } else {
                                        String::new()
                                    };
                                    let hint = if rule.protocol == super::Protocol::Udp {
                                        status.error.as_deref().unwrap_or(t().features.mapping_udp_no_probe)
                                    } else {
                                        match &status.probe {
                                            super::ProbeStatus::Unreachable(error) => {
                                                error.as_str()
                                            }
                                            _ => t().features.mapping_probe_interval,
                                        }
                                    };
                                    let speed = if !snapshot.enabled || !rule.enabled {
                                        "—".into()
                                    } else {
                                        format!(
                                            "↑ {}/s\n↓ {}/s",
                                            bytes(status.send_rate.max(0.0) as u64),
                                            bytes(status.receive_rate.max(0.0) as u64)
                                        )
                                    };
                                    let traffic = format!(
                                        "↑ {}\n↓ {}",
                                        bytes(status.total_sent),
                                        bytes(status.total_received)
                                    );
                                    match controls::mapping_row(
                                        ui,
                                        controls::MappingRow {
                                            name: &rule.name,
                                            local: &local,
                                            target: &target,
                                            status: &label,
                                            detail: &detail,
                                            hint,
                                            speed: &speed,
                                            traffic: &traffic,
                                            reachable,
                                            url: url.as_deref(),
                                            probing: status.probing,
                                            can_probe: rule.protocol == super::Protocol::Tcp,
                                            enabled: rule.enabled,
                                            color,
                                        },
                                        index + 1 == snapshot.rules.len(),
                                    ) {
                                        controls::MappingRowAction::None => {}
                                        controls::MappingRowAction::Probe => {
                                            self.submit(handle, Command::Probe(rule.id));
                                        }
                                        controls::MappingRowAction::Enable(value) => {
                                            self.submit(handle, Command::Enable(rule.id, value));
                                        }
                                        controls::MappingRowAction::Edit => {
                                            self.form = Some(Form::from(rule));
                                            self.error = None;
                                        }
                                        controls::MappingRowAction::Delete => {
                                            self.delete = Some(rule.clone());
                                        }
                                    }
                                    crate::ui::controls::observe_notice(
                                        ui.ctx(),
                                        ("mapping-rule", rule.id),
                                        t().features.mapping_rule_error,
                                        crate::ui::controls::DialogIcon::Error,
                                        status.error.as_deref(),
                                    );
                                });
                            }
                        });
                }
            });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(t().features.mapping_title)
                    .size(theme::SMALL)
                    .color(theme::MUTED),
            );
        });
        self.dialogs(ui.ctx(), handle);
    }
    fn dialogs(&mut self, ctx: &egui::Context, handle: &Handle) {
        if let Some(form) = &mut self.form {
            let mut save = false;
            let mut cancel = false;
            let modal = egui::Modal::new(egui::Id::new("port-rule-editor"))
                .frame(controls::dialog_frame())
                .show(ctx, |ui| {
                    ui.set_width(theme::MAPPING_DIALOG_WIDTH);
                    cancel = crate::ui::controls::dialog_header(
                        ui,
                        if form.new {
                            t().features.mapping_add_rule
                        } else {
                            t().features.mapping_edit_rule
                        },
                        crate::ui::controls::DialogIcon::Edit,
                        true,
                    );
                    field(ui, t().features.mapping_rule_name, &mut form.name, t().features.mapping_rule_name_hint);
                    ui.horizontal(|ui| {
                        ui.label(t().features.mapping_protocol);
                        egui::ComboBox::from_id_salt("mapping-protocol")
                            .selected_text(form.protocol.label())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut form.protocol,
                                    super::Protocol::Tcp,
                                    "TCP",
                                );
                                ui.selectable_value(
                                    &mut form.protocol,
                                    super::Protocol::Udp,
                                    "UDP",
                                );
                            });
                    });
                    ui.add_space(16.0);
                    ui.columns(2, |columns| {
                        egui::Frame::new()
                            .fill(theme::SIDEBAR)
                            .corner_radius(theme::PANEL_RADIUS)
                            .inner_margin(14)
                            .show(&mut columns[0], |ui| {
                                ui.set_width(ui.available_width());
                                ui.set_min_height(theme::MAPPING_ENDPOINT_HEIGHT);
                                ui.label(RichText::new(t().features.mapping_this_device).strong());
                                ui.add_space(12.0);
                                field(ui, t().features.mapping_listen_address, &mut form.local_addr, "127.0.0.1");
                                ui.add_space(10.0);
                                field(ui, t().features.mapping_local_port, &mut form.local, t().features.mapping_local_port_hint);
                            });
                        egui::Frame::new()
                            .fill(theme::SIDEBAR)
                            .corner_radius(theme::PANEL_RADIUS)
                            .inner_margin(14)
                            .show(&mut columns[1], |ui| {
                                ui.set_width(ui.available_width());
                                ui.set_min_height(theme::MAPPING_ENDPOINT_HEIGHT);
                                ui.label(RichText::new(t().features.mapping_target_service).strong());
                                ui.add_space(12.0);
                                field(ui, t().features.mapping_target_address, &mut form.target, t().features.mapping_target_address_hint);
                                ui.add_space(10.0);
                                field(ui, t().features.mapping_target_port, &mut form.remote, t().features.mapping_target_port_hint);
                            });
                    });
                    ui.add_space(12.0);
                    if let Some(error) = self.error.take() {
                        crate::ui::controls::notice(
                            ui.ctx(),
                            "mapping-operation",
                            t().features.mapping_title,
                            crate::ui::controls::DialogIcon::Error,
                            error,
                        );
                    }
                    let (accept, dismiss) = crate::ui::controls::dialog_actions(
                        ui,
                        Some(crate::ui::controls::DialogAction::new(if form.new {
                            t().features.mapping_add_rule
                        } else {
                            t().features.mapping_save_changes
                        })),
                        Some(t().common.cancel),
                    );
                    save = accept;
                    cancel |= dismiss;
                });
            if save {
                match form.rule() {
                    Ok(rule) => {
                        if self.submit(handle, Command::Save(rule)) {
                            self.form = None;
                        }
                    }
                    Err(error) => self.error = Some(error.to_string()),
                }
            } else if cancel || modal.should_close() {
                self.form = None;
                self.error = None;
            }
        }
        if let Some(rule) = self.delete.clone() {
            let mut confirm = false;
            let mut cancel = false;
            let modal = egui::Modal::new(egui::Id::new("port-rule-delete"))
                .frame(controls::dialog_frame())
                .show(ctx, |ui| {
                    ui.set_width(theme::MAPPING_DIALOG_WIDTH);
                    cancel = crate::ui::controls::dialog_header(
                        ui,
                        t().features.mapping_delete_title,
                        crate::ui::controls::DialogIcon::Warning,
                        true,
                    );
                    ui.label((t().features.mapping_delete_body)(&rule.name));
                    let (accept, dismiss) = crate::ui::controls::dialog_actions(
                        ui,
                        Some(crate::ui::controls::DialogAction::new(t().common.delete).danger(true)),
                        Some(t().common.cancel),
                    );
                    confirm = accept;
                    cancel |= dismiss;
                });
            if confirm {
                self.submit(handle, Command::Delete(rule.id));
                self.delete = None;
            } else if cancel || modal.should_close() {
                self.delete = None;
            }
        }
    }
}
fn bytes(n: u64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MiB", n as f64 / 1048576.0)
    } else {
        format!("{:.1} KiB", n as f64 / 1024.0)
    }
}

fn field(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str) {
    ui.label(
        RichText::new(label)
            .size(theme::COMPACT_TEXT)
            .color(theme::MUTED),
    );
    ui.add_sized(
        vec2(ui.available_width(), controls::HEIGHT),
        controls::singleline(value, controls::HEIGHT).hint_text(hint),
    );
}
