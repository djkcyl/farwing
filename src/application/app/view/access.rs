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

impl Page {
    /// The page is only for an account. Accepting assistance works for
    /// guests; assisting others is gated on that page itself.
    pub(super) fn needs_account(self) -> bool {
        matches!(self, Self::Friends | Self::DeviceDetails)
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
            "登录"
        } else {
            "设置服务器并登录"
        }
    }

    /// Start signing in from wherever it is needed.
    pub(super) fn sign_in(&mut self) {
        let step = self.sign_in_step();
        self.open_sign_in(step);
    }

    /// A page for an account only: why, and how to get there.
    pub(super) fn locked_page(&mut self, ui: &mut egui::Ui) {
        let page = self.center_ui.page;
        let title = match page {
            Page::Friends => "登录后使用好友",
            _ => "登录后使用",
        };
        cards::page_header(ui, page.title(), "", |_| {});
        let primary = self.sign_in_label();
        match cards::locked(ui, title, &[primary, "附近的设备"]) {
            Some(0) => self.sign_in(),
            Some(_) => {
                self.center_ui.page = Page::Devices;
                self.direct.tab = super::super::direct::DeviceTab::Nearby;
                self.direct.tab_chosen = true;
                self.direct.scan(ui.ctx());
            }
            None => {}
        }
    }

    /// The account footer while not signed in: what is set up, and sign-in.
    pub(super) fn signed_out_footer(&mut self, ui: &mut egui::Ui, bounds: egui::Rect) {
        let left = bounds.left() + 6.0;
        let right = bounds.right() - 6.0;
        let server = self.saved_server();
        ui.painter().text(
            egui::pos2(left, bounds.top() + 30.0),
            egui::Align2::LEFT_CENTER,
            "未登录",
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
                .add_sized(button.size(), primary("登录").min_size(vec2(0.0, 0.0)))
                .clicked();
        });
        if sign_in {
            self.sign_in();
        }
        let baseline = bounds.top() + 62.0;
        let (text, color) = match &server {
            Some(origin) => (
                format!("服务器 {}", origin.trim_start_matches("https://")),
                MUTED,
            ),
            None => ("未设置服务器".to_owned(), MUTED),
        };
        ui.painter()
            .circle_filled(egui::pos2(left + 3.0, baseline), 3.0, color);
        let galley = ui.painter().layout(
            text,
            FontId::proportional(theme::TINY),
            MUTED,
            (right - left - 13.0).max(0.0),
        );
        ui.painter().galley(
            egui::pos2(left + 13.0, baseline - galley.size().y / 2.0),
            galley,
            MUTED,
        );
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
