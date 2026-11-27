use super::{Event, viewers::account_viewport};
use crate::account::api::{DeviceList, device_id_text};
use crate::i18n::t;
use crate::ui::{controls, theme};
use egui::ViewportId;
use std::sync::mpsc;

pub(super) fn show(
    ui: &mut egui::Ui,
    devices: &DeviceList,
    events: &mpsc::Sender<Event>,
    parent: ViewportId,
    source: ViewportId,
    busy: bool,
) {
    let ctx = ui.ctx().clone();
    let send = |event| {
        if events.send(event).is_ok() {
            ctx.request_repaint_of(parent);
        }
    };
    let button = controls::viewer_device_button(ui, t().controls.switch_device);
    if button.clicked() && !busy {
        send(Event::RefreshDevices);
    }
    egui::Popup::menu(&button)
        .width(theme::DEVICE_MENU_WIDTH)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_width(theme::DEVICE_MENU_WIDTH);
            if controls::device_menu_header(ui, busy) {
                send(Event::RefreshDevices);
            }
            let online = devices.devices.iter().chain(&devices.shared).filter(|device| {
                device.device_id != devices.current_device.device_id
                    && device.is_connected()
                    && device.controlled_support
                    && device.controllable
                    && device.platform.is_desktop()
                    && devices.with_capability(&device.device_id, farwing_protocol::caps::VIEW).is_ok()
            });
            let height = theme::DEVICE_MENU_ROW_HEIGHT;
            let gap = theme::DEVICE_MENU_ROW_GAP;
            let rows = online.clone().count();
            if rows == 0 {
                if !busy {
                    ui.weak(t().viewer.no_other_online_devices);
                }
                return;
            }
            let visible = rows.min(7) as f32;
            let list_height = (visible * height + (visible - 1.0) * gap)
                .min((ctx.content_rect().height() - 120.0).max(height));
            egui::ScrollArea::vertical()
                .min_scrolled_height(list_height)
                .max_height(list_height)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = gap;
                    for device in online {
                        let current = account_viewport(&device.device_id) == source;
                        let response = ui.push_id(&device.device_id, |ui| {
                            controls::device_menu_row(
                                ui, super::center::alias(device), &device_id_text(&device.device_id),
                                device.status_label(), theme::GREEN, current, !busy && !current,
                            )
                        }).inner;
                        if response.clicked() {
                            send(Event::SwitchDevice { source, device_id: device.device_id.clone() });
                            ui.close();
                        }
                    }
                });
        });
}
