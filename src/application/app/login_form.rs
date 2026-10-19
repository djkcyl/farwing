//! Login page state: the Farwing Server address, its certificate trust and the
//! account credentials.
use super::*;
use crate::account::server::{CertificateProbe, ServerConfig};
use std::sync::mpsc::{Receiver, TryRecvError};

/// The two steps of signing in: the server first, then the account. The
/// server step stands alone because the program also works without an
/// account (LAN and direct connections, guest assistance).
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum LoginStep {
    #[default]
    Server,
    Account,
}

/// How an account login was made (the password form is the only one).
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum LoginMethod {
    #[default]
    Password,
}

/// Fingerprints compare by their hex digits only.
fn same_fingerprint(a: &str, b: &str) -> bool {
    let digits = |s: &str| {
        s.chars()
            .filter(char::is_ascii_hexdigit)
            .map(|c| c.to_ascii_uppercase())
            .collect::<String>()
    };
    let a = digits(a);
    !a.is_empty() && a == digits(b)
}

pub(super) struct LoginForm {
    pub step: LoginStep,
    /// The server being saved; the account step follows.
    pub saving_server: bool,
    /// Continue to the account step once the certificate check finishes.
    pub advance: bool,
    /// Attempt counter of the current submission.
    pub generation: u64,
    pub submitting: bool,
    pub status: String,
    pub error: Option<String>,
    pub server: String,
    /// SHA-256 certificate fingerprint the user chose to trust (optional).
    pub pin: String,
    pub username: String,
    pub password: String,
    pub probe: Option<std::result::Result<CertificateProbe, String>>,
    probe_rx: Option<Receiver<std::result::Result<CertificateProbe, String>>>,
}

impl Default for LoginForm {
    fn default() -> Self {
        let saved = ServerConfig::load().ok().flatten();
        Self {
            step: if saved.is_some() {
                LoginStep::Account
            } else {
                LoginStep::Server
            },
            saving_server: false,
            advance: false,
            generation: 0,
            submitting: false,
            status: String::new(),
            error: None,
            server: saved
                .as_ref()
                .map(|server| server.origin().to_owned())
                .unwrap_or_default(),
            pin: saved
                .as_ref()
                .and_then(ServerConfig::pinned_fingerprint)
                .unwrap_or_default(),
            username: String::new(),
            password: String::new(),
            probe: None,
            probe_rx: None,
        }
    }
}

impl LoginForm {
    pub fn probing(&self) -> bool {
        self.probe_rx.is_some()
    }

    pub fn server_config(&self) -> Result<ServerConfig> {
        ServerConfig::new(&self.server, Some(self.pin.as_str()), None)
    }

    pub fn can_submit(&self) -> bool {
        !self.submitting
            && self.server_config().is_ok()
            && crate::account::login::Credentials::new(&self.username, self.password.clone())
                .is_ok()
    }

    /// Inspect the server certificate in the background.
    pub fn start_probe(&mut self, ctx: &egui::Context) {
        if self.probing() {
            return;
        }
        let address = self.server.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        let started = std::thread::Builder::new()
            .name("server-probe".into())
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(anyhow::Error::from)
                    .and_then(|runtime| {
                        runtime.block_on(crate::account::server::probe_certificate(&address))
                    })
                    .map_err(|error| format!("{error:#}"));
                let _ = tx.send(result);
                ctx.request_repaint();
            });
        match started {
            Ok(_) => {
                self.probe = None;
                self.probe_rx = Some(rx);
            }
            Err(error) => self.probe = Some(Err(format!("无法检查服务器：{error}"))),
        }
    }

    /// The checked certificate is trusted: by Windows, or by the typed
    /// fingerprint.
    pub fn certificate_trusted(&self) -> bool {
        matches!(&self.probe, Some(Ok(probe))
            if probe.trusted_by_system || same_fingerprint(&self.pin, &probe.sha256))
    }

    /// The server this form was saved with, if any.
    pub fn saved_origin() -> Option<String> {
        ServerConfig::load()
            .ok()
            .flatten()
            .map(|server| server.origin().to_owned())
    }

    pub fn poll_probe(&mut self) {
        let Some(rx) = &self.probe_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                self.probe = Some(result);
                self.probe_rx = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.probe = Some(Err("服务器检查未完成".into()));
                self.probe_rx = None;
            }
        }
    }

    pub fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.submitting = false;
        self.status.clear();
        self.error = None;
    }

    pub fn clear_private(&mut self) {
        self.cancel();
        self.password.clear();
    }
}

impl DeviceCenterApp {
    pub(super) fn submit_login(&mut self) {
        if self.login_form.submitting
            || self.login_restoring
            || self.logout_pending
            || self.has_viewers()
            || self.mutation_pending
        {
            return;
        }
        let server = match self.login_form.server_config() {
            Ok(server) => server,
            Err(error) => {
                self.login_form.error = Some(format!("{error:#}"));
                self.login_form.status = format!("{error:#}");
                return;
            }
        };
        let credentials = match crate::account::login::Credentials::new(
            &self.login_form.username,
            std::mem::take(&mut self.login_form.password),
        ) {
            Ok(credentials) => credentials,
            Err(error) => {
                self.login_form.error = Some(format!("{error:#}"));
                self.login_form.status = format!("{error:#}");
                return;
            }
        };
        self.clear_catalog();
        self.devices = None;
        self.selected_device_id = None;
        self.login_form.generation = self.login_form.generation.wrapping_add(1);
        self.login_form.submitting = true;
        self.login_form.error = None;
        self.login_form.status = "正在登录…".into();
        self.login_error = None;
        self.login_status = "正在登录…".into();
        self.sync_login_running();
        if self
            .worker
            .commands
            .send(GuiCommand::Login {
                generation: self.login_generation,
                attempt: self.login_form.generation,
                server,
                credentials,
            })
            .is_err()
        {
            self.login_form.submitting = false;
            self.sync_login_running();
            self.login_form.status = "登录服务不可用".into();
            self.login_form.error = Some("设备后台服务已经停止".into());
        }
    }

    /// Save the server of the first step (closing a guest presence on the
    /// previous server); the account step follows.
    pub(super) fn use_server(&mut self) {
        self.login_form.advance = false;
        let server = match self.login_form.server_config() {
            Ok(server) => server,
            Err(error) => {
                self.login_form.error = Some(format!("{error:#}"));
                self.login_form.status = format!("{error:#}");
                return;
            }
        };
        self.login_form.saving_server = true;
        self.login_form.error = None;
        if self
            .worker
            .commands
            .send(GuiCommand::UseServer { server })
            .is_err()
        {
            self.login_form.saving_server = false;
            self.login_form.error = Some("设备后台服务已经停止".into());
        }
    }

    pub(super) fn finish_server(&mut self, result: std::result::Result<(), String>) {
        self.login_form.saving_server = false;
        match result {
            Ok(()) => {
                self.login_form.step = LoginStep::Account;
                self.login_form.error = None;
                self.login_form.status.clear();
            }
            Err(error) => {
                self.login_form.status = format!("保存服务器失败：{error}");
                self.login_form.error = Some(error);
            }
        }
    }

    pub(super) fn cancel_password_login(&mut self) {
        self.login_form.cancel();
        self.sync_login_running();
        let _ = self
            .worker
            .commands
            .send(GuiCommand::CancelAttempt(self.login_generation));
    }
}
