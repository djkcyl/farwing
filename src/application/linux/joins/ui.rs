use super::{AssistAccount, ConnectTarget, JoinAction, JoinRequest, device_id};
use crate::account::{
    api::{DeviceInfo, DeviceList, device_id_text},
    server::ServerConfig,
};
use crate::i18n::t;
use crate::transport::direct::{self, Target, lan, store};
use crate::ui::{controls, device_list, theme};
use anyhow::{Result, bail};
use egui::RichText;
use std::time::{Duration, Instant};
use tokio::{
    runtime::Handle,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Connect,
    Pair,
}

enum Route<'a> {
    Account(&'a DeviceInfo),
    Assist(String),
    Direct {
        address: String,
        expected: Option<String>,
        name: Option<String>,
    },
}

#[derive(Default)]
pub(in super::super) struct JoinUi {
    open: bool,
    mode: Mode,
    target: String,
    expected: String,
    name: Option<String>,
    publisher_id: Option<String>,
    code: String,
    ask: bool,
    // A signed-out installation may need to configure its first server.
    configured_server: Option<ServerConfig>,
    server: String,
    fingerprint: String,
    nearby: Nearby,
    pairing: Option<Pairing>,
    error: Option<String>,
    message: Option<String>,
}

impl JoinUi {
    pub(in super::super) fn show(
        &mut self,
        ui: &mut egui::Ui,
        runtime: &Handle,
        devices: Option<&DeviceList>,
    ) -> Option<JoinAction> {
        let text = t();
        let signed_in = devices.is_some();
        ui.horizontal(|ui| {
            let input = ui.add_sized(
                [
                    (ui.available_width() - 210.0).max(120.0),
                    controls::HEIGHT,
                ],
                controls::singleline(&mut self.target, controls::HEIGHT)
                    .id_salt("linux-connect-target")
                    .hint_text(text.center.target_hint)
                    .char_limit(256),
            );
            if input.changed() {
                self.clear_target_identity();
            }
            let enter = input.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            if ui.add(controls::primary(text.common.connect)).clicked() || enter {
                self.open(Mode::Connect);
            }
            if ui.add(controls::secondary(text.center.pair)).clicked() {
                self.open(Mode::Pair);
            }
        });
        if !self.open {
            return None;
        }
        if self.mode == Mode::Connect
            && matches!(ConnectTarget::parse(&self.target), Ok(ConnectTarget::DeviceId(_)))
        {
            self.nearby.poll(ui, runtime);
        }
        self.poll_pairing();
        let modal = egui::Modal::new(egui::Id::new("linux-join-dialog"))
            .frame(controls::dialog_frame())
            .show(ui.ctx(), |ui| {
                ui.set_width(440.0);
                if let Some(pairing) = &mut self.pairing {
                    pairing.show(ui);
                    return (false, None);
                }
                let title = match self.mode {
                    Mode::Pair => text.center.pair,
                    Mode::Connect if !signed_in => text.center.without_account,
                    Mode::Connect => text.center.connect_to_device,
                };
                let dismiss = controls::dialog_header(
                    ui,
                    title,
                    controls::DialogIcon::Info,
                    true,
                );
                let (label, hint) = if self.mode == Mode::Pair {
                    (text.session.address_required, "192.168.1.2:28890")
                } else {
                    (text.center.enter_target, text.center.target_hint)
                };
                if field(ui, label, &mut self.target, hint, false, 256).changed() {
                    self.clear_target_identity();
                    self.error = None;
                }
                let route = (self.mode == Mode::Connect).then(|| self.route(devices));
                let direct = matches!(&route, Some(Ok(Route::Direct { .. })));
                let address = matches!(ConnectTarget::parse(&self.target), Ok(ConnectTarget::Address(_)));
                if self.mode == Mode::Pair || (direct && address) {
                    let pinned = self.name.is_some() && !self.expected.is_empty();
                    ui.add_enabled_ui(!pinned, |ui| {
                        field(
                            ui,
                            text.center.device_id,
                            &mut self.expected,
                            if self.mode == Mode::Pair { "" } else { text.common.optional },
                            false,
                            64,
                        );
                    });
                }
                if self.mode == Mode::Connect {
                    if !signed_in && matches!(&route, Some(Ok(Route::Assist(_)))) {
                        if let Some(config) = &self.configured_server {
                            ui.label(RichText::new(config.origin()).color(theme::MUTED));
                        } else {
                            field(
                                ui,
                                text.center.server_address,
                                &mut self.server,
                                text.center.server_address_hint,
                                false,
                                256,
                            );
                            field(
                                ui,
                                text.center.fingerprint_optional,
                                &mut self.fingerprint,
                                "SHA-256",
                                false,
                                128,
                            );
                        }
                    }
                    ui.add_enabled_ui(!direct || !self.ask, |ui| {
                        field(
                            ui,
                            text.center.access_code,
                            &mut self.code,
                            text.common.optional,
                            true,
                            256,
                        );
                    });
                    if direct
                        && ui.checkbox(&mut self.ask, text.center.ask_for_approval).changed()
                        && self.ask
                    {
                        self.code.clear();
                    }
                }
                if let Some(message) = &self.message {
                    ui.colored_label(theme::GREEN, message);
                }
                if let Some(error) = &self.error {
                    ui.colored_label(theme::RED, error);
                }
                let submit = if self.mode == Mode::Pair {
                    text.center.pair
                } else {
                    text.common.connect
                };
                let (accept, cancel) = controls::dialog_actions(
                    ui,
                    Some(controls::DialogAction::new(submit)),
                    Some(text.common.cancel),
                );
                if dismiss || cancel {
                    return (true, None);
                }
                if accept {
                    self.error = None;
                    self.message = None;
                    let action = match self.mode {
                        Mode::Pair => self.start_pairing(ui.ctx(), runtime).map(|_| None),
                        Mode::Connect => self.request(devices).map(Some),
                    };
                    match action {
                        Ok(action) => return (false, action),
                        Err(error) => self.error = Some(format!("{error:#}")),
                    }
                }
                (false, None)
            });
        let dismiss = modal.should_close();
        let (close, action) = modal.inner;
        if close || dismiss {
            if let Some(pairing) = &self.pairing {
                pairing.cancel.cancel();
                ui.ctx().request_repaint();
            } else {
                self.close();
            }
            return None;
        }
        if action.is_some() {
            self.close();
        }
        action
    }

    pub(in super::super) fn open_target(
        &mut self,
        target: String,
        expected: Option<String>,
        name: Option<String>,
    ) {
        self.target = target;
        self.clear_target_identity();
        self.expected = expected.unwrap_or_default();
        self.name = name;
        self.open(Mode::Connect);
    }

    pub(in super::super) fn open_saved(&mut self, device: &crate::account::assist::SavedDevice) {
        self.open_target(device.connect_id.clone(), None, None);
        self.publisher_id = (!device.publisher_device_id.is_empty())
            .then(|| device.publisher_device_id.clone());
    }

    pub(in super::super) fn refresh_nearby(&mut self) {
        self.nearby.next_scan = None;
    }

    pub(in super::super) fn show_nearby(&mut self, ui: &mut egui::Ui, runtime: &Handle) {
        self.nearby.poll(ui, runtime);
        let text = &t().center;
        if let Some(error) = &self.nearby.error {
            ui.colored_label(theme::AMBER, error);
        }
        if self.nearby.found.is_empty() && self.nearby.error.is_none() {
            device_list::empty(
                ui,
                if self.nearby.scan.is_some() {
                    text.searching_lan
                } else {
                    text.no_nearby_devices
                },
            );
        }
        let mut selected = None;
        for host in &self.nearby.found {
            let id = device_id_text(&host.announce.device_id);
            let detail = format!("ID {id} · {}", (text.address_line)(&host.target().to_string()));
            let mut mode = None;
            ui.push_id(&host.announce.device_id, |ui| {
                device_list::row(
                    ui,
                    &device_list::Row {
                        title: &host.announce.name,
                        tile: device_list::Tile::Generic,
                        status: None,
                        chips: &[],
                        extra_badges: None,
                        detail: &detail,
                        selected: None,
                    },
                    |ui| {
                        if ui.add(controls::primary(t().common.connect)).clicked() {
                            mode = Some(Mode::Connect);
                        }
                        if ui.add(controls::secondary(text.pair)).clicked() {
                            mode = Some(Mode::Pair);
                        }
                    },
                );
            });
            if let Some(mode) = mode {
                selected = Some((host, mode));
            }
        }
        if let Some((host, mode)) = selected {
            let target = host.target().to_string();
            let expected = host.announce.device_id.clone();
            let name = host.announce.name.clone();
            self.open_target(target, Some(expected), Some(name));
            self.mode = mode;
            ui.ctx().request_repaint();
        }
    }

    fn open(&mut self, mode: Mode) {
        self.open = true;
        self.mode = mode;
        self.code.clear();
        self.ask = false;
        self.error = None;
        self.message = None;
        self.server.clear();
        self.fingerprint.clear();
        self.configured_server = match ServerConfig::load() {
            Ok(config) => config,
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                None
            }
        };
    }

    fn close(&mut self) {
        self.open = false;
        self.code.clear();
        self.ask = false;
    }

    fn clear_target_identity(&mut self) {
        self.expected.clear();
        self.name = None;
        self.publisher_id = None;
        self.code.clear();
        self.ask = false;
    }

    fn route<'a>(&self, devices: Option<&'a DeviceList>) -> Result<Route<'a>> {
        Ok(match ConnectTarget::parse(&self.target)? {
            ConnectTarget::ShortNumber(number) => Route::Assist(number),
            ConnectTarget::Address(address) => Route::Direct {
                address: address.to_string(),
                expected: (!self.expected.trim().is_empty()).then(|| self.expected.clone()),
                name: self.name.clone(),
            },
            ConnectTarget::DeviceId(id) => {
                if self.code.trim().is_empty()
                    && !self.ask
                    && let Some(device) = devices
                        .into_iter()
                        .flat_map(|devices| devices.devices.iter().chain(&devices.shared))
                        .find(|device| device.device_id == id)
                {
                    anyhow::ensure!(
                        super::super::center::can_view(device),
                        "{}",
                        t().center.no_permissions
                    );
                    return Ok(Route::Account(device));
                }
                if let Some(host) = self.nearby.found.iter().find(|h| h.announce.device_id == id) {
                    return Ok(Route::Direct {
                        address: host.target().to_string(),
                        expected: Some(id),
                        name: Some(host.announce.name.clone()),
                    });
                }
                if devices.is_none()
                    && self.configured_server.is_none()
                    && self.server.trim().is_empty()
                    && let Some(host) = store::known_host(&id).filter(|host| !host.address.is_empty())
                {
                    return Ok(Route::Direct {
                        address: host.address,
                        expected: Some(id),
                        name: Some(host.name),
                    });
                }
                Route::Assist(id)
            }
        })
    }

    fn request(&self, devices: Option<&DeviceList>) -> Result<JoinAction> {
        let code = self.code.trim();
        Ok(match self.route(devices)? {
            Route::Account(device) => JoinAction::Account(device.clone()),
            Route::Assist(target) => {
                let account = if devices.is_some() {
                    AssistAccount::SignedIn
                } else if let Some(config) = &self.configured_server {
                    AssistAccount::Guest(Some(config.clone()))
                } else {
                    if self.server.trim().is_empty() {
                        if super::is_short_number(&target) {
                            bail!("{}", t().center.short_number_needs_server);
                        }
                        bail!("{}", t().center.status_no_server);
                    }
                    AssistAccount::Guest(Some(ServerConfig::new(
                        &self.server,
                        Some(&self.fingerprint),
                        None,
                    )?))
                };
                JoinAction::Join(JoinRequest::assist(
                    &target,
                    code.to_owned(),
                    account,
                    self.publisher_id.as_deref(),
                )?)
            }
            Route::Direct { address, expected, name } => JoinAction::Join(JoinRequest::direct(
                &address,
                (!self.ask && !code.is_empty()).then(|| code.to_owned()),
                self.ask,
                expected.as_deref(),
                name.as_deref(),
            )?),
        })
    }

    fn start_pairing(&mut self, ctx: &egui::Context, runtime: &Handle) -> Result<()> {
        let target = Target::parse(&self.target)?;
        let expected = device_id(&self.expected)?;
        let id = device_id_text(&expected);
        let name = (t().center.name_with_id)(self.name.as_deref().unwrap_or(&self.target), &id);
        self.pairing = Some(Pairing::start(ctx, runtime, target, expected, name));
        Ok(())
    }

    fn poll_pairing(&mut self) {
        let Some(pairing) = &mut self.pairing else {
            return;
        };
        let result = loop {
            match pairing.events.try_recv() {
                Ok(PairEvent::Code(code)) => pairing.code = Some(code),
                Ok(PairEvent::Finished(result)) => break result,
                Err(mpsc::error::TryRecvError::Empty) => return,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    break Err(anyhow::anyhow!("{}", t().app.request_send_failed));
                }
            }
        };
        match result {
            Ok(name) => {
                self.message = Some((t().center.paired_with)(&name));
                self.mode = Mode::Connect;
                self.code.clear();
                self.ask = false;
            }
            Err(_) if pairing.cancel.is_cancelled() => {
                self.message = Some(t().center.pairing_canceled.into());
            }
            Err(error) => {
                self.error = Some((t().center.pairing_failed)(
                    &pairing.name,
                    &format!("{error:#}"),
                ));
            }
        }
        self.pairing = None;
    }
}

fn field(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut String,
    hint: &str,
    password: bool,
    limit: usize,
) -> egui::Response {
    ui.label(RichText::new(label).color(theme::MUTED));
    ui.add_sized(
        [ui.available_width(), controls::HEIGHT],
        controls::singleline(value, controls::HEIGHT)
            .id_salt(label)
            .hint_text(hint)
            .password(password)
            .char_limit(limit),
    )
}

#[derive(Default)]
struct Nearby {
    found: Vec<lan::Found>,
    error: Option<String>,
    scan: Option<oneshot::Receiver<Result<Vec<lan::Found>>>>,
    next_scan: Option<Instant>,
}

impl Nearby {
    fn poll(&mut self, ui: &egui::Ui, runtime: &Handle) {
        if let Some(scan) = &mut self.scan {
            let result = match scan.try_recv() {
                Ok(result) => Some(result),
                Err(oneshot::error::TryRecvError::Empty) => None,
                Err(oneshot::error::TryRecvError::Closed) => {
                    Some(Err(anyhow::anyhow!("{}", t().app.request_send_failed)))
                }
            };
            if let Some(result) = result {
                self.scan = None;
                let gap = if result.is_ok() { 3 } else { 10 };
                self.next_scan = Some(Instant::now() + Duration::from_secs(gap));
                match result {
                    Ok(found) => {
                        self.found = found;
                        self.error = None;
                    }
                    Err(error) => {
                        self.error = Some((t().app.lan_discovery_failed)(&format!("{error:#}")));
                    }
                }
            }
        }
        if self.scan.is_none() {
            let delay = self.next_scan.map_or(Duration::ZERO, |at| {
                at.saturating_duration_since(Instant::now())
            });
            if delay.is_zero() {
                let (sender, receiver) = oneshot::channel();
                let ctx = ui.ctx().clone();
                self.scan = Some(receiver);
                runtime.spawn(async move {
                    let result =
                        lan::discover(Duration::from_millis(1500))
                            .await
                            .map(|mut hosts| {
                                let local = direct::local_device_id();
                                hosts.retain(|host| {
                                    local.as_deref() != Some(host.announce.device_id.as_str())
                                });
                                hosts
                            });
                    let _ = sender.send(result);
                    ctx.request_repaint();
                });
            } else {
                ui.ctx().request_repaint_after(delay);
            }
        }
    }
}

struct Pairing {
    name: String,
    code: Option<String>,
    agree: Option<oneshot::Sender<()>>,
    events: mpsc::UnboundedReceiver<PairEvent>,
    cancel: CancellationToken,
}

enum PairEvent {
    Code(String),
    Finished(Result<String>),
}

impl Pairing {
    fn start(
        ctx: &egui::Context,
        runtime: &Handle,
        target: Target,
        expected: String,
        name: String,
    ) -> Self {
        let (sender, events) = mpsc::unbounded_channel();
        let (agree, agreed) = oneshot::channel();
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let ctx = ctx.clone();
        runtime.spawn(async move {
            let result = async {
                let local = direct::local_device()?;
                let host = direct::client::pair(
                    &target,
                    &expected,
                    &local,
                    |code| {
                        let _ = sender.send(PairEvent::Code(code));
                        ctx.request_repaint();
                    },
                    agreed,
                    &task_cancel,
                )
                .await?;
                let previous = store::find_trusted(&host.public_key);
                store::save_trusted(store::TrustedDevice {
                    device_id: host.device_id,
                    public_key: host.public_key,
                    name: host.name.clone(),
                    grants: previous
                        .as_ref()
                        .map_or_else(farwing_protocol::caps::all, |peer| peer.grants.clone()),
                    trusted_at: previous
                        .as_ref()
                        .map_or_else(|| chrono::Utc::now().timestamp(), |peer| peer.trusted_at),
                    last_seen: previous.as_ref().map_or(0, |peer| peer.last_seen),
                    paired: true,
                })?;
                Ok(host.name)
            }
            .await;
            let _ = sender.send(PairEvent::Finished(result));
            ctx.request_repaint();
        });
        Self {
            name,
            code: None,
            agree: Some(agree),
            events,
            cancel,
        }
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        let text = t();
        ui.heading((text.center.pairing_with)(&self.name));
        let detail = match &self.code {
            Some(code) => {
                ui.label(
                    RichText::new(code)
                        .monospace()
                        .size(theme::DIALOG_TITLE)
                        .color(theme::ACCENT),
                );
                if self.agree.is_some() {
                    text.center.compare_pairing_code
                } else {
                    text.center.waiting_for_confirmation
                }
            }
            None => text.center.connecting_to_peer,
        };
        ui.label(detail);
        ui.horizontal(|ui| {
            if ui.add(controls::secondary(text.common.cancel)).clicked() {
                self.cancel.cancel();
            }
            if self.code.is_some()
                && self.agree.is_some()
                && !self.cancel.is_cancelled()
                && ui.add(controls::primary(text.center.pair)).clicked()
                && let Some(agree) = self.agree.take()
            {
                let _ = agree.send(());
            }
        });
    }
}

impl Drop for Pairing {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
