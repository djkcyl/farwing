//! One device-state model for lists and details. Local ownership is session-based.
use super::*;
use crate::i18n::t;
use crate::ui::theme;

#[derive(Clone, Copy)]
enum State {
    Online,
    Offline,
    Unknown,
    Restricted,
    OtherDevice,
    Connecting,
    Connected,
    Viewing,
    Controlling,
    Forwarding,
    Closing,
    PowerPending,
}

pub(crate) struct DeviceStatus {
    state: State,
}

impl DeviceStatus {
    pub(crate) fn label(&self) -> &'static str {
        let t = t();
        let text = &t.app;
        match self.state {
            State::Online => t.common.online,
            State::Offline => t.common.offline,
            State::Unknown => text.status_unknown,
            State::Restricted => text.status_restricted,
            State::OtherDevice => text.status_other_device,
            State::Connecting => text.status_connecting,
            State::Connected => text.status_connected,
            State::Viewing => text.status_viewing,
            State::Controlling => text.status_controlling,
            State::Forwarding => text.status_forwarding,
            State::Closing => text.status_closing,
            State::PowerPending => text.status_power_pending,
        }
    }

    pub(crate) fn color(&self) -> egui::Color32 {
        match self.state {
            State::Online => theme::GREEN,
            State::OtherDevice | State::Restricted | State::PowerPending => theme::AMBER,
            State::Offline | State::Unknown => theme::MUTED,
            _ => theme::ACCENT,
        }
    }
}

pub(crate) fn connection_status(device: &DeviceInfo, controller: &str) -> DeviceStatus {
    resolve(device, Some(controller), None, false, false)
}

fn resolve(
    device: &DeviceInfo,
    controller: Option<&str>,
    viewer: Option<bool>,
    closing: bool,
    power_pending: bool,
) -> DeviceStatus {
    let activity = controller
        .and_then(|id| crate::session::controller::gui_connection_activity(id, &device.device_id));
    let mapping = controller
        .and_then(|id| crate::features::port_mapping::service::status(id, &device.device_id));
    let forwarding =
        mapping.as_ref().is_some_and(|s| s.enabled && s.connected) && activity.is_some();
    let state = if power_pending {
        State::PowerPending
    } else if viewer.is_some() && closing {
        State::Closing
    } else if activity
        .as_ref()
        .is_some_and(|s| s.viewing && s.controlling)
    {
        State::Controlling
    } else if viewer == Some(true) && activity.is_some() {
        State::Viewing
    } else if viewer.is_some() {
        State::Connecting
    } else if activity.as_ref().is_some_and(|s| s.viewing) {
        State::Viewing
    } else if forwarding {
        State::Forwarding
    } else if activity.is_some() {
        State::Connected
    } else if mapping.as_ref().is_some_and(|s| s.enabled && s.busy) {
        State::Connecting
    } else if device.is_offline() {
        State::Offline
    } else if !device.is_connected() {
        State::Unknown
    } else if device.participant_count() > 0 {
        State::OtherDevice
    } else if device.platform.is_desktop()
        && (!device.controllable || !device.controlled_support)
    {
        State::Restricted
    } else {
        State::Online
    };
    DeviceStatus { state }
}

impl DeviceCenterApp {
    pub(super) fn device_status(&self, device: &DeviceInfo) -> DeviceStatus {
        resolve(
            device,
            self.devices
                .as_ref()
                .map(|list| list.current_device.device_id.as_str()),
            self.viewer_for_device(&device.device_id)
                .map(|s| s.handle.info().is_some_and(|info| info.playing)),
            self.viewer_for_device(&device.device_id)
                .is_some_and(|s| s.closing),
            self.power_progress
                .get(&device.device_id)
                .is_some_and(|p| p.waiting),
        )
    }
}
