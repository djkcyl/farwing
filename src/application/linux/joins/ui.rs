use super::{AssistAccount, JoinRequest, device_id};
use crate::account::{api::device_id_text, server::ServerConfig};
use crate::i18n::t;
use crate::transport::direct::{self, Target, lan, store};
use crate::ui::{controls, theme};
use anyhow::Result;
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
    Assist,
    Direct,
    Pair,
}

#[derive(Default)]
pub(in super::super) struct JoinUi {
    open: bool,
    mode: Mode,
    target: String,
    address: String,
    expected: String,
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
        signed_in: bool,
        server: &str,
        fingerprint: &str,
    ) -> Option<Result<JoinRequest>> {
        let text = t();
        let label = if signed_in {
            text.center.connect_to_device
        } else {
            text.center.without_account
        };
        if ui.add(controls::secondary(label)).clicked() {
            self.open = true;
            self.error = None;
            self.message = None;
            self.server = server.to_owned();
            self.fingerprint = fingerprint.to_owned();
            if !signed_in {
                match ServerConfig::load() {
                    Ok(config) => self.configured_server = config,
                    Err(error) => {
                        self.configured_server = None;
                        self.error = Some(format!("{error:#}"));
                    }
                }
                if self.configured_server.is_none() && self.server.trim().is_empty() {
                    self.mode = Mode::Direct;
                }
            }
        }
        if !self.open {
            return None;
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
                ui.heading(text.center.connect_to_device);
                let previous_mode = self.mode;
                ui.horizontal_wrapped(|ui| {
                    ui.selectable_value(&mut self.mode, Mode::Assist, text.center.via_server);
                    ui.selectable_value(&mut self.mode, Mode::Direct, text.center.without_server);
                    ui.selectable_value(&mut self.mode, Mode::Pair, text.center.pair);
                });
                if previous_mode != self.mode {
                    self.code.clear();
                    self.error = None;
                    self.message = None;
                }
                ui.add_space(8.0);
                match self.mode {
                    Mode::Assist => {
                        if !signed_in {
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
                        field(
                            ui,
                            text.center.enter_target,
                            &mut self.target,
                            text.session.assist_target_required,
                            false,
                            64,
                        );
                    }
                    Mode::Direct | Mode::Pair => {
                        field(
                            ui,
                            text.session.address_required,
                            &mut self.address,
                            "192.168.1.2:28890",
                            false,
                            256,
                        );
                        field(
                            ui,
                            text.center.device_id,
                            &mut self.expected,
                            if self.mode == Mode::Direct {
                                text.common.optional
                            } else {
                                ""
                            },
                            false,
                            64,
                        );
                    }
                }
                if self.mode != Mode::Pair {
                    ui.add_enabled_ui(self.mode != Mode::Direct || !self.ask, |ui| {
                        field(
                            ui,
                            text.center.access_code,
                            &mut self.code,
                            text.common.optional,
                            true,
                            256,
                        );
                    });
                    if self.mode == Mode::Direct
                        && ui
                            .checkbox(&mut self.ask, text.center.ask_for_approval)
                            .changed()
                        && self.ask
                    {
                        self.code.clear();
                    }
                }
                if self.mode != Mode::Assist {
                    ui.add_space(6.0);
                    if let Some(host) = self.nearby.show(ui, runtime) {
                        self.address = host.target().to_string();
                        self.expected = host.announce.device_id;
                        self.code.clear();
                        self.error = None;
                    }
                }
                if let Some(message) = &self.message {
                    ui.colored_label(theme::GREEN, message);
                }
                if let Some(error) = &self.error {
                    ui.colored_label(theme::RED, error);
                }
                let mut result = None;
                let mut close = false;
                ui.horizontal(|ui| {
                    close = ui.add(controls::secondary(text.common.cancel)).clicked();
                    let submit = if self.mode == Mode::Pair {
                        text.center.pair
                    } else {
                        text.common.connect
                    };
                    if ui.add(controls::primary(submit)).clicked() {
                        self.error = None;
                        self.message = None;
                        let action = match self.mode {
                            Mode::Pair => self.start_pairing(ui.ctx(), runtime).map(|_| None),
                            _ => self.request(signed_in).map(Some),
                        };
                        match action {
                            Ok(request) => result = request,
                            Err(error) => self.error = Some(format!("{error:#}")),
                        }
                    }
                });
                (close, result)
            });
        let dismiss = modal.should_close();
        let (close, request) = modal.inner;
        if close || dismiss {
            if let Some(pairing) = &self.pairing {
                pairing.cancel.cancel();
                ui.ctx().request_repaint();
            } else {
                self.close();
            }
            return None;
        }
        if request.is_some() {
            self.close();
        }
        request.map(Ok)
    }

    fn close(&mut self) {
        self.open = false;
        self.code.clear();
        self.ask = false;
    }

    fn request(&self, signed_in: bool) -> Result<JoinRequest> {
        let code = self.code.trim();
        match self.mode {
            Mode::Assist => {
                let account = if signed_in {
                    AssistAccount::SignedIn
                } else if let Some(config) = &self.configured_server {
                    AssistAccount::Guest(Some(config.clone()))
                } else {
                    AssistAccount::Guest(Some(ServerConfig::new(
                        &self.server,
                        Some(&self.fingerprint),
                        None,
                    )?))
                };
                JoinRequest::assist(&self.target, code.to_owned(), account)
            }
            Mode::Direct => JoinRequest::direct(
                &self.address,
                (!self.ask && !code.is_empty()).then(|| code.to_owned()),
                self.ask,
                (!self.expected.trim().is_empty()).then_some(self.expected.as_str()),
                self.nearby
                    .found
                    .iter()
                    .find(|host| host.announce.device_id == self.expected)
                    .map(|host| host.announce.name.as_str()),
            ),
            Mode::Pair => unreachable!("pairing does not start a viewing session"),
        }
    }

    fn start_pairing(&mut self, ctx: &egui::Context, runtime: &Handle) -> Result<()> {
        let target = Target::parse(&self.address)?;
        let expected = device_id(&self.expected)?;
        let known_name = self
            .nearby
            .found
            .iter()
            .find(|host| host.announce.device_id == expected)
            .map(|host| host.announce.name.as_str());
        let id = device_id_text(&expected);
        let name = (t().center.name_with_id)(known_name.unwrap_or(&self.address), &id);
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
                self.mode = Mode::Direct;
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
) {
    ui.label(RichText::new(label).color(theme::MUTED));
    ui.add_sized(
        [ui.available_width(), controls::HEIGHT],
        controls::singleline(value, controls::HEIGHT)
            .id_salt(label)
            .hint_text(hint)
            .password(password)
            .char_limit(limit),
    );
}

#[derive(Default)]
struct Nearby {
    found: Vec<lan::Found>,
    error: Option<String>,
    scan: Option<oneshot::Receiver<Result<Vec<lan::Found>>>>,
    next_scan: Option<Instant>,
}

impl Nearby {
    fn show(&mut self, ui: &mut egui::Ui, runtime: &Handle) -> Option<lan::Found> {
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
        let text = &t().center;
        ui.label(RichText::new(text.tab_nearby).strong());
        if let Some(error) = &self.error {
            ui.colored_label(theme::AMBER, error);
        }
        if self.found.is_empty() && self.error.is_none() {
            ui.label(
                RichText::new(if self.scan.is_some() {
                    text.searching_lan
                } else {
                    text.no_nearby_devices
                })
                .color(theme::MUTED),
            );
        }
        let mut selected = None;
        egui::ScrollArea::vertical()
            .id_salt("join-nearby")
            .max_height(130.0)
            .show(ui, |ui| {
                for host in &self.found {
                    ui.push_id(&host.announce.device_id, |ui| {
                        let label = (text.name_with_id)(
                            &host.announce.name,
                            &device_id_text(&host.announce.device_id),
                        );
                        if ui
                            .add(controls::secondary(&label))
                            .on_hover_text(host.address.to_string())
                            .clicked()
                        {
                            selected = Some(host.clone());
                        }
                    });
                }
            });
        selected
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
