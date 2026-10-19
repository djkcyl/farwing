use super::*;
impl DeviceCenterApp {
    pub(in crate::application::app) fn open_file_transfer(&mut self, id: String) {
        if self.logout_pending {
            return;
        }
        let Some(device) = self
            .devices
            .as_ref()
            .and_then(|list| list.with_capability(&id, farwing_protocol::caps::FILES).ok())
            .cloned()
        else {
            return;
        };
        if self
            .worker
            .commands
            .send(GuiCommand::Files {
                generation: self.login_generation,
                device,
                options: self.media,
            })
            .is_err()
        {
            self.status = StatusMessage::error("设备后台服务已停止");
        }
    }
    pub(in crate::application::app) fn open_port_mapping(&mut self, id: String) {
        if self.logout_pending {
            return;
        }
        let Some(device) = self
            .devices
            .as_ref()
            .and_then(|list| list.with_capability(&id, farwing_protocol::caps::PORTS).ok())
            .cloned()
        else {
            return;
        };
        if self
            .worker
            .commands
            .send(GuiCommand::Ports {
                generation: self.login_generation,
                device,
                options: self.media,
            })
            .is_err()
        {
            self.status = StatusMessage::error("设备后台服务已停止");
        }
    }
}
