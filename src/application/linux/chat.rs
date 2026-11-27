//! Text chat in the Linux device center.
#[path = "../app/view/chat/mod.rs"]
mod page;

use crate::account::api::{DeviceInfo, DeviceList};
use crate::account::client::AuthenticatedClient;
use crate::features::chat::ChatService;
use crate::features::chat::state::Notice;
use crate::i18n::t;
use crate::ui::theme::{AMBER, SURFACE, TEXT};
use std::sync::Arc;
use tokio::runtime::Handle;

pub(super) struct LinuxChat {
    service: ChatService,
    page: page::ChatUi,
    status: Option<(String, egui::Color32)>,
    notice: Option<Notice>,
}

impl LinuxChat {
    pub(super) fn new(
        client: Arc<AuthenticatedClient>,
        runtime: Handle,
        ctx: egui::Context,
    ) -> Self {
        let chat = Self {
            service: ChatService::start(client, runtime, ctx),
            page: page::ChatUi::default(),
            status: None,
            notice: None,
        };
        chat.set_visible(false);
        chat
    }

    /// Keep listening when the center switches pages, but don't mark the old channel read.
    pub(super) fn set_visible(&self, visible: bool) {
        self.service.set_hidden(!visible);
        if !visible {
            self.service.set_focus(None);
        }
    }

    pub(super) fn unread_count(&self) -> usize {
        let (count, any) = self.service.view().totals();
        (count as usize).max(usize::from(any))
    }

    /// Returns true when the user asks to open the notified conversation.
    pub(super) fn show_notice(&mut self, ui: &mut egui::Ui) -> bool {
        if let Some(notice) = self.service.take_notices().into_iter().last() {
            self.notice = Some(notice);
        }
        let Some(notice) = &self.notice else {
            return false;
        };
        let (open, dismiss) = egui::Frame::new()
            .fill(SURFACE)
            .inner_margin(8)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let dismiss = ui.small_button(t().common.close).clicked();
                    let open = ui.button(notice.title.as_str()).clicked();
                    ui.add(egui::Label::new(notice.body.as_str()).wrap());
                    (open, dismiss)
                })
                .inner
            })
            .inner;
        if open || dismiss {
            let channel = notice.channel;
            self.notice = None;
            if open && channel.0 != 0 {
                self.page.open = Some(channel);
            }
        }
        open
    }

    pub(super) fn show(&mut self, ui: &mut egui::Ui) {
        self.show_with_devices(ui, None);
    }

    pub(super) fn show_with_devices(
        &mut self,
        ui: &mut egui::Ui,
        devices: Option<&DeviceList>,
    ) -> Option<DeviceInfo> {
        self.notice = None;
        let notices = self.service.take_notices();
        if let Some(error) = self.service.take_errors().into_iter().last() {
            self.status = Some((error, AMBER));
        } else if let Some(notice) = notices.into_iter().last() {
            self.status = Some((format!("{} · {}", notice.title, notice.body), TEXT));
        }
        if let Some((text, color)) = &self.status {
            let close = egui::Frame::new()
                .fill(SURFACE)
                .inner_margin(8)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let close = ui.small_button(t().common.close).clicked();
                        ui.add(egui::Label::new(egui::RichText::new(text).color(*color)).wrap());
                        close
                    })
                    .inner
                })
                .inner;
            if close {
                self.status = None;
            }
        }
        let rect = ui.available_rect_before_wrap();
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
            self.page.show(ui, &self.service, devices)
        })
        .inner
    }
}

impl Drop for LinuxChat {
    fn drop(&mut self) {
        self.service.stop();
    }
}
