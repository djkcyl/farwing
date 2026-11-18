//! 设备详情页的远端终端入口。

impl super::DeviceCenterApp {
    pub(super) fn start_terminal_service(&mut self, device: crate::account::api::DeviceInfo) {
        if self.logout_pending || self.mutation_pending {
            return;
        }
        if self
            .worker
            .commands
            .send(super::GuiCommand::Terminal {
                generation: self.login_generation,
                device,
            })
            .is_err()
        {
            self.status = super::StatusMessage::error("设备后台服务已停止");
        }
    }

    /// 在系统终端里打开 CLI，让它经控制中心的终端服务接入对端。
    /// `session` 为 None 表示新建一个 shell 再接入。
    pub(super) fn launch_remote_terminal(&mut self, target: &str, session: Option<u32>) {
        if !crate::features::terminal::relay::available() {
            self.status = super::StatusMessage::error(
                "没有设置 XDG_RUNTIME_DIR，终端窗口无法接到控制中心；请改用命令行 OpenUUYC terminal",
            );
            return;
        }
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(error) => {
                self.status = super::StatusMessage::error(format!("找不到当前程序路径：{error}"));
                return;
            }
        };
        let mut args = vec![
            exe.into_os_string(),
            "terminal".into(),
            "--device-id".into(),
            target.into(),
            "--via-gui".into(),
        ];
        match session {
            Some(handle) => args.push(handle.to_string().into()),
            None => args.push("--new".into()),
        }
        if let Err(error) = crate::features::terminal::launch::open(args) {
            self.status = super::StatusMessage::error(format!("{error:#}"));
        }
    }
}
