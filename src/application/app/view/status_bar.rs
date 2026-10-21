//! The bar along the bottom of the device centre: where this device stands
//! with its server on the left, whether it is being controlled and the
//! version on the right. The one place that shows the server's state; pages
//! do not repeat it.
use super::access::Access;
use super::*;

/// Where this device stands with its server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ServerState {
    /// No server set: LAN and direct connections only.
    NotSet,
    /// A server, not signed in.
    SignedOut,
    /// Signed in; the server has not answered yet.
    Connecting,
    Online,
    /// The last attempt failed; another one is due.
    Unreachable,
}

impl ServerState {
    pub(super) fn label(self) -> &'static str {
        let text = &t().center;
        match self {
            Self::NotSet => text.status_no_server,
            Self::SignedOut => t().common.signed_out,
            Self::Connecting => text.status_connecting,
            Self::Online => text.status_online,
            Self::Unreachable => text.status_unreachable,
        }
    }

    fn color(self) -> Color32 {
        match self {
            Self::NotSet | Self::SignedOut => MUTED,
            Self::Connecting => BLUE,
            Self::Online => GREEN,
            Self::Unreachable => AMBER,
        }
    }
}

const HEIGHT: f32 = 30.0;

impl DeviceCenterApp {
    pub(super) fn server_state(&mut self) -> ServerState {
        match self.access() {
            Access::NoServer => ServerState::NotSet,
            Access::Guest => ServerState::SignedOut,
            Access::Account if self.server_unreachable => ServerState::Unreachable,
            Access::Account if self.cached_devices => ServerState::Connecting,
            Access::Account => ServerState::Online,
        }
    }

    pub(super) fn draw_status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("center-status")
            .resizable(false)
            .exact_size(HEIGHT)
            .frame(
                egui::Frame::new()
                    .fill(SIDEBAR)
                    .stroke(Stroke::new(1.0, LINE))
                    .inner_margin(egui::Margin::symmetric(16, 0)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    self.server_status(ui);
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        let (rect, _) =
                            ui.allocate_exact_size(vec2(140.0, HEIGHT), Sense::hover());
                        self.draw_version(ui, rect);
                        if self.host.as_ref().is_some_and(|h| h.status().connected) {
                            ui.label(
                                RichText::new(t().center.being_controlled)
                                    .size(theme::TINY)
                                    .color(GREEN),
                            );
                            let (dot, _) = ui.allocate_exact_size(vec2(6.0, 6.0), Sense::hover());
                            ui.painter().circle_filled(dot.center(), 3.0, GREEN);
                        }
                    });
                });
            });
    }

    /// The server's state and address, with what can be done about it.
    fn server_status(&mut self, ui: &mut egui::Ui) {
        let state = self.server_state();
        let (dot, _) = ui.allocate_exact_size(vec2(6.0, 6.0), Sense::hover());
        ui.painter().circle_filled(dot.center(), 3.0, state.color());
        ui.label(RichText::new(state.label()).size(theme::TINY).color(TEXT));
        if let Some(origin) = self.saved_server() {
            ui.label(
                RichText::new(origin.trim_start_matches("https://"))
                    .size(theme::TINY)
                    .color(MUTED),
            );
        }
        let action = match state {
            ServerState::NotSet => Some(t().center.status_set_up_server),
            // The sidebar has the sign-in button.
            ServerState::SignedOut => None,
            ServerState::Unreachable => {
                let next = self.next_reconnect(ui.ctx());
                ui.label(
                    RichText::new(match next {
                        Some(seconds) => (t().center.reconnect_in)(seconds),
                        None => t().center.reconnecting.to_owned(),
                    })
                    .size(theme::TINY)
                    .color(MUTED),
                );
                next.map(|_| t().center.reconnect_now)
            }
            ServerState::Connecting | ServerState::Online => None,
        };
        if let Some(action) = action
            && link(ui, action)
        {
            match state {
                ServerState::NotSet => {
                    self.center_ui.page = Page::Settings;
                    self.center_ui.settings_tab = settings::SettingsTab::Account;
                }
                _ => self.reconnect(),
            }
        }
    }
}

/// A small text action in the bar.
fn link(ui: &mut egui::Ui, text: &str) -> bool {
    ui.add(
        egui::Label::new(RichText::new(text).size(theme::TINY).color(BLUE))
            .sense(Sense::click()),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
    .clicked()
}
