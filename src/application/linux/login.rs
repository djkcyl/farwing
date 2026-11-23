use crate::account::{login::Credentials, server::ServerConfig};
use crate::i18n::t;
use crate::ui::{controls, login, theme};
use anyhow::{Result, ensure};
use egui::{Align, Layout, RichText};

#[derive(Clone, Copy, PartialEq)]
enum Step {
    Server,
    Account,
}

pub(super) struct LoginScreen {
    step: Step,
    server: String,
    fingerprint: String,
    username: String,
    password: String,
}

impl LoginScreen {
    pub(super) fn new(server: String, fingerprint: String) -> Self {
        Self {
            step: if server.is_empty() {
                Step::Server
            } else {
                Step::Account
            },
            server,
            fingerprint,
            username: String::new(),
            password: String::new(),
        }
    }

    pub(super) fn server(&self) -> &str {
        &self.server
    }

    pub(super) fn show(
        &mut self,
        root: &mut egui::Ui,
        busy: bool,
        error: &mut Option<String>,
    ) -> Option<Result<(ServerConfig, Credentials)>> {
        let step = self.step;
        let height = match step {
            Step::Server => 350.0,
            Step::Account => 390.0,
        };
        let mut attempt = None;
        login::card(root, height, |ui| {
            attempt = match self.step {
                Step::Server => self.server_step(ui, busy).err().map(Err),
                Step::Account => self.account_step(ui, busy),
            };
            if self.step != step {
                *error = None;
            }
            if let Some(error) = error.as_deref().filter(|_| !busy) {
                ui.add_space(6.0);
                ui.add(egui::Label::new(RichText::new(error).color(theme::AMBER)).wrap());
            }
        });
        attempt
    }

    fn server_step(&mut self, ui: &mut egui::Ui, busy: bool) -> Result<()> {
        let text = t();
        let size = [ui.available_width(), theme::CONTROL_HEIGHT];
        step_title(ui, text.center.connect_server);
        let enter = ui
            .add_enabled_ui(!busy, |ui| {
                ui.label(RichText::new(text.center.server_address).color(theme::MUTED));
                let address = ui.add_sized(
                    size,
                    controls::singleline(&mut self.server, size[1])
                        .id_salt("server-address")
                        .hint_text(text.center.server_address_hint)
                        .char_limit(256),
                );
                ui.add_space(6.0);
                ui.label(RichText::new(text.center.fingerprint_optional).color(theme::MUTED));
                let fingerprint = ui.add_sized(
                    size,
                    controls::singleline(&mut self.fingerprint, size[1])
                        .id_salt("server-fingerprint")
                        .hint_text("SHA-256")
                        .char_limit(128),
                );
                if address.changed() || fingerprint.changed() {
                    self.password.clear();
                }
                address.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter))
            })
            .inner;
        ui.add_space(10.0);
        let ready = !busy && !self.server.trim().is_empty();
        if (primary_action(ui, ready, text.common.next) || enter) && ready {
            let config = self.server_config()?;
            self.server = config.origin().to_owned();
            self.step = Step::Account;
            ui.ctx().request_repaint();
        }
        Ok(())
    }

    fn account_step(
        &mut self,
        ui: &mut egui::Ui,
        busy: bool,
    ) -> Option<Result<(ServerConfig, Credentials)>> {
        let text = t();
        let size = [ui.available_width(), theme::CONTROL_HEIGHT];
        step_title(ui, text.center.sign_in_title);
        let change = ui
            .allocate_ui_with_layout(
                egui::vec2(size[0], ui.spacing().interact_size.y),
                Layout::right_to_left(Align::Center),
                |ui| {
                    let change = ui
                        .add_enabled(!busy, egui::Link::new(text.center.change))
                        .clicked();
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.label(RichText::new(text.center.server).color(theme::MUTED));
                        ui.add(
                            egui::Label::new(RichText::new(&self.server).color(theme::TEXT))
                                .truncate(),
                        )
                        .on_hover_text(&self.server);
                    });
                    change
                },
            )
            .inner;
        ui.add_space(8.0);
        let enter = ui
            .add_enabled_ui(!busy, |ui| {
                ui.label(RichText::new(text.center.username).color(theme::MUTED));
                ui.add_sized(
                    size,
                    controls::singleline(&mut self.username, size[1])
                        .id_salt("account-username")
                        .char_limit(128),
                );
                ui.add_space(6.0);
                ui.label(RichText::new(text.center.password).color(theme::MUTED));
                let password = ui.add_sized(
                    size,
                    controls::singleline(&mut self.password, size[1])
                        .id_salt("account-password")
                        .password(true)
                        .char_limit(1024),
                );
                password.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter))
            })
            .inner;
        ui.add_space(14.0);
        let ready = !busy && !self.username.trim().is_empty() && !self.password.is_empty();
        let submit = primary_action(
            ui,
            ready,
            if busy {
                text.center.signing_in
            } else {
                text.common.sign_in
            },
        );
        if change {
            self.step = Step::Server;
            ui.ctx().request_repaint();
            None
        } else if ready && (submit || enter) {
            Some(self.credentials())
        } else {
            None
        }
    }

    fn server_config(&self) -> Result<ServerConfig> {
        ServerConfig::new(&self.server, Some(&self.fingerprint), None)
    }

    fn credentials(&mut self) -> Result<(ServerConfig, Credentials)> {
        let config = self.server_config()?;
        ensure!(
            !ServerConfig::overridden(),
            "Unset FARWING_SERVER_URL to change the configured server"
        );
        let credentials = Credentials::new(&self.username, self.password.clone())?;
        self.password.clear();
        Ok((config, credentials))
    }
}

fn step_title(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).size(theme::DIALOG_TITLE).strong());
    ui.add_space(12.0);
}

fn primary_action(ui: &mut egui::Ui, enabled: bool, label: &str) -> bool {
    let size = [ui.available_width(), theme::CONTROL_HEIGHT];
    ui.add_enabled_ui(enabled, |ui| {
        ui.add_sized(size, controls::centered_primary(label))
    })
    .inner
    .clicked()
}
