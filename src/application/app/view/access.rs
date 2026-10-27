//! What can be used without an account or a server, and the way to the rest
//! (`docs/access-model.md` §2.5). The device center is the same in every
//! case; entries that need an account stay visible, locked, and explain how
//! to unlock them. Without an account this device still connects (by device
//! ID, short number or address, with an access code or as a trusted device)
//! and still hosts (as a guest, offline without a server).
use super::cards;
use super::*;
use crate::application::app::login_form::LoginStep;

/// How far this device is signed in.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Access {
    /// No Farwing Server set: LAN and direct connections only.
    NoServer,
    /// A server without an account: guest assistance, LAN and direct.
    Guest,
    Account,
}

/// Why something that needs the server cannot be used now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Blocker {
    NoServer,
    SignedOut,
    Unreachable,
}

impl Blocker {
    /// Why, in a word or two: the same text everywhere.
    pub(super) fn reason(self) -> &'static str {
        match self {
            Self::NoServer => t().center.status_no_server,
            Self::SignedOut => t().common.signed_out,
            Self::Unreachable => t().center.status_unreachable,
        }
    }
}

/// A button for something that needs the server: disabled, with the reason
/// on hover, while that cannot work.
pub(super) fn server_button(
    ui: &mut egui::Ui,
    blocker: Option<Blocker>,
    enabled: bool,
    button: impl egui::Widget,
) -> egui::Response {
    let response = ui.add_enabled(enabled && blocker.is_none(), button);
    match blocker {
        Some(blocker) => response.on_disabled_hover_text(blocker.reason()),
        None => response,
    }
}

impl Page {
    /// The page is only for an account. Accepting assistance works for
    /// guests; assisting others is gated on that page itself.
    pub(super) fn needs_account(self) -> bool {
        matches!(self, Self::Friends | Self::Records | Self::Notices | Self::DeviceDetails)
    }
}

impl DeviceCenterApp {
    /// The saved server's address, read at most every two seconds.
    pub(super) fn saved_server(&mut self) -> Option<String> {
        if self
            .center_ui
            .server_cache
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= Duration::from_secs(2))
        {
            self.center_ui.server_cache = Some((
                Instant::now(),
                crate::application::app::login_form::LoginForm::saved_origin(),
            ));
        }
        self.center_ui
            .server_cache
            .as_ref()
            .and_then(|(_, origin)| origin.clone())
    }

    pub(super) fn access(&mut self) -> Access {
        if !self.needs_login() {
            Access::Account
        } else if self.saved_server().is_some() {
            Access::Guest
        } else {
            Access::NoServer
        }
    }

    /// Leave the device center for the sign-in steps.
    pub(super) fn open_sign_in(&mut self, step: LoginStep) {
        self.center_ui.browsing = false;
        self.center_ui.server_cache = None;
        self.login_form.step = step;
        self.login_form.error = None;
    }

    /// Where signing in starts: the account, or the server first.
    pub(super) fn sign_in_step(&mut self) -> LoginStep {
        if self.saved_server().is_some() {
            LoginStep::Account
        } else {
            LoginStep::Server
        }
    }

    /// The label of the action that starts signing in.
    pub(super) fn sign_in_label(&mut self) -> &'static str {
        if self.saved_server().is_some() {
            t().common.sign_in
        } else {
            t().center.set_up_server_and_sign_in
        }
    }

    /// Start signing in from wherever it is needed.
    pub(super) fn sign_in(&mut self) {
        let step = self.sign_in_step();
        self.open_sign_in(step);
    }

    /// What keeps account features from working now; `None` when they do.
    pub(super) fn blocker(&mut self) -> Option<Blocker> {
        use super::status_bar::ServerState;
        match self.server_state() {
            ServerState::NotSet => Some(Blocker::NoServer),
            ServerState::SignedOut => Some(Blocker::SignedOut),
            ServerState::Unreachable => Some(Blocker::Unreachable),
            ServerState::Connecting | ServerState::Online => None,
        }
    }

    /// The one placeholder for anything that needs the server: why, and
    /// the way out.
    pub(super) fn blocked_state(&mut self, ui: &mut egui::Ui, blocker: Blocker) {
        let (detail, actions) = match blocker {
            Blocker::NoServer => (None, vec![t().center.status_set_up_server]),
            Blocker::SignedOut => (None, vec![t().common.sign_in]),
            // While an attempt is under way there is nothing to press.
            Blocker::Unreachable => match self.next_reconnect(ui.ctx()) {
                Some(seconds) => (
                    Some((t().center.reconnect_in)(seconds)),
                    vec![t().center.reconnect_now],
                ),
                None => (Some(t().center.reconnecting.to_owned()), Vec::new()),
            },
        };
        let offline = blocker == Blocker::Unreachable;
        if cards::locked(ui, blocker.reason(), detail.as_deref(), offline, &actions).is_some() {
            match blocker {
                Blocker::NoServer | Blocker::SignedOut => self.sign_in(),
                Blocker::Unreachable => self.reconnect(),
            }
        }
    }

    /// A page for an account only, while that is not available.
    pub(super) fn locked_page(&mut self, ui: &mut egui::Ui) {
        let page = self.center_ui.page;
        cards::page_header(ui, page.title(), "", |_| {});
        let blocker = self.blocker().unwrap_or(Blocker::SignedOut);
        self.blocked_state(ui, blocker);
    }

    /// The account footer while not signed in: what is set up, and sign-in.
    pub(super) fn signed_out_footer(&mut self, ui: &mut egui::Ui, bounds: egui::Rect) {
        let left = bounds.left() + 6.0;
        let right = bounds.right() - 6.0;
        ui.painter().text(
            egui::pos2(left, bounds.top() + 30.0),
            egui::Align2::LEFT_CENTER,
            t().common.signed_out,
            FontId::proportional(theme::BODY),
            TEXT,
        );
        let button = egui::Rect::from_min_max(
            egui::pos2(right - 64.0, bounds.top() + 16.0),
            egui::pos2(right, bounds.top() + 44.0),
        );
        let mut sign_in = false;
        ui.scope_builder(egui::UiBuilder::new().max_rect(button), |ui| {
            sign_in = ui
                .add_sized(
                    button.size(),
                    primary(t().common.sign_in).min_size(vec2(0.0, 0.0)),
                )
                .clicked();
        });
        if sign_in {
            self.sign_in();
        }
    }

    /// This device's name and (display) ID, also without an account.
    pub(super) fn local_identity(&mut self) -> Option<(String, String)> {
        if self.center_ui.local_identity.is_none()
            && let Ok(device) = crate::transport::direct::local_device()
        {
            self.center_ui.local_identity = Some((
                device.name.clone(),
                crate::account::api::device_id_text(&device.identity.device_id),
            ));
        }
        self.center_ui.local_identity.clone()
    }
}
