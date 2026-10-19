//! One device-state model for lists and details. Local ownership is session-based.
use super::*;
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
        match self.state {
            State::Online => "在线",
            State::Offline => "离线",
            State::Unknown => "状态未知",
            State::Restricted => "未开放连接",
            State::OtherDevice => "其他设备使用",
            State::Connecting => "本机连接中",
            State::Connected => "本机已连接",
            State::Viewing => "本机观看中",
            State::Controlling => "本机控制中",
            State::Forwarding => "本机转发中",
            State::Closing => "正在结束观看",
            State::PowerPending => "等待电源操作",
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
