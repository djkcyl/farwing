//! Initial layout policy, before capabilities are probed or media tracks start.
use super::*;
use crate::features::stream_control::publisher::{ConnectOptions, Resolution};

fn size(value: Option<&Resolution>) -> Option<(u32, u32)> {
    let value = value?;
    let (w, h) = (
        u32::try_from(value.width).ok()?,
        u32::try_from(value.height).ok()?,
    );
    (w > 0 && h > 0).then_some((w, h))
}

impl Session {
    pub(crate) async fn prepare(&self, options: ConnectOptions) -> Result<capture::Screen> {
        ensure!(
            options.kind == 1 && matches!(options.connect_type, 1 | 2),
            "不支持的被控连接类型"
        );
        ensure!(
            !options.force_virtual || self.lease.display_expansion_allowed(),
            "本次会话不允许强制创建虚拟屏"
        );
        ensure!(options.virtual_modes.len() <= 64, "虚拟屏模式数量过多");
        if let Some(params) = &options.params {
            ensure!((0..=4).contains(&params.resolution_type), "无效分辨率策略");
            if params.resolution_type == 4 {
                validate_modes(&[
                    size(params.chosen_resolution.as_ref()).context("缺少指定分辨率")?
                ])?;
            }
        }
        let modes: Vec<_> = options
            .virtual_modes
            .iter()
            .map(|m| Ok((u32::try_from(m.width)?, u32::try_from(m.height)?)))
            .collect::<Result<_>>()?;
        if !modes.is_empty() {
            validate_modes(&modes)?;
        }
        tokio::task::spawn_blocking(recovery::recover_abandoned).await??;
        self.run(move |state, lease| state.prepare(options, modes, lease))
            .await
    }
    pub(crate) fn initial_screen(&self, screen: &capture::Screen) -> capture::Screen {
        let state = lock(&self.state);
        let mut initial = screen.clone();
        if let Some(saved) = state
            .journal
            .baseline
            .targets
            .iter()
            .find(|t| screen.identity.as_deref() == Some(t.identity.as_str()))
        {
            initial.width = saved.width;
            initial.height = saved.height;
            initial.left = saved.left;
            initial.top = saved.top;
            initial.dpi_scale = state
                .journal
                .dpi
                .iter()
                .find(|(id, _)| id == &saved.identity)
                .map(|(_, dpi)| *dpi);
        }
        initial
    }
}

impl State {
    fn initial_resolution(
        &mut self,
        target: &Target,
        width: u32,
        height: u32,
        lease: &Lease,
    ) -> Result<bool> {
        let previous = self.journal.mode_changes.clone();
        self.intend_mode(&target.identity, width, height)?;
        match target.set_resolution(width, height, || lease.requested()) {
            Ok(()) => Ok(true),
            Err(error)
                if error
                    .downcast_ref::<display::topology::ModeTestRejected>()
                    .is_some() =>
            {
                ensure!(lease.requested(), "显示操作已取消");
                // Preflight did not modify Windows. Do not leave a fictitious
                // desired mode for cleanup, or persist it as successfully applied.
                self.journal.desired = None;
                self.journal.mode_changes = previous;
                self.save()?;
                tracing::warn!(width, height, %error,
                    "initial display mode rejected; retaining current mode for connection");
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn prepare(
        &mut self,
        options: ConnectOptions,
        modes: Vec<(u32, u32)>,
        lease: &Lease,
    ) -> Result<capture::Screen> {
        if self.fallback_pin.is_none() {
            self.fallback_pin = fallback::pin().unwrap_or_else(|error| {
                tracing::warn!(error=%format!("{error:#}"), "optional display driver unavailable");
                None
            });
        }
        let mut screens = capture::screens()?;
        tracing::info!(
            screens = screens.len(),
            forced_virtual = options.force_virtual,
            saved_virtual = self.preferences.default_virtual,
            "host initial display selection"
        );
        let choice = options.params.as_ref().map_or(0, |p| p.resolution_type);
        let local = size(
            options
                .params
                .as_ref()
                .and_then(|p| p.local_resolution.as_ref()),
        );
        let virtual_size = if choice == 0 {
            size(options.virtual_initial.as_ref())
        } else {
            local
        }
        .unwrap_or((1920, 1080));
        let mut automatic = None;
        let targets = Topology::query(true)?.metadata()?;
        if !options.force_virtual
            && !self.preferences.default_virtual
            && !self.preferences.super_enabled
            && !targets.iter().any(|t| {
                t.available && t.virtual_provider != Some(display::topology::VirtualProvider::Uu)
            })
        {
            automatic = fallback::ensure(
                self.fallback_driver()?,
                virtual_size.0,
                virtual_size.1,
                &[],
                true,
            )?;
            screens = capture::screens()?;
        }
        if !options.force_virtual && self.preferences.super_enabled {
            return self.enter_super(virtual_size.0, virtual_size.1, 0, true, lease);
        }
        if options.force_virtual || self.preferences.default_virtual {
            // 「只用虚拟屏启动」在 Linux 上还没做：它要的是本机屏幕不再显示操作内容，
            // 做不到就照常把物理屏留着等于骗人。主控这次明确要，就带着原因失败；
            // 本机存下来的偏好只降级处理——那可能是从 Windows 带过来的偏好文件，
            // 为它把连接挡死会让这台机器再也连不上。
            #[cfg(not(windows))]
            ensure!(
                !options.force_virtual,
                "Linux 被控端还不支持「只用虚拟屏启动」：物理显示器没法下线，请改用普通连接"
            );
            let result = validate_modes(&[virtual_size]).and_then(|()| {
                self.create(
                    Virtual {
                        guid: uuid::Uuid::new_v4().to_string(),
                        identity: None,
                        width: virtual_size.0,
                        height: virtual_size.1,
                        hz: 144,
                        dpi: 0,
                        kind: 3,
                        resolution_type: choice.max(1),
                        modes,
                        layout: None,
                    },
                    lease,
                    options.force_virtual || self.preferences.default_virtual,
                )
            });
            // 走到这里的在 Linux 上只剩存下来的偏好（主控明确要的上面已经挡掉了）。
            // 那块屏要靠辅助程序、内核和当时那个合成器，换了桌面（niri 换成 GNOME）
            // 或者辅助程序没启用就建不出来；和 `restore_virtual` 一样记一条警告、
            // 退回物理屏接着连，而不是让这台机器从此连不上。Windows 的虚拟屏驱动是
            // 装好的，建不出来说明环境坏了，照旧报错。
            #[cfg(not(windows))]
            let result = match result {
                Err(error) => {
                    tracing::warn!(
                        error = %format!("{error:#}"),
                        "按保存的偏好在连接时建虚拟屏失败，改用物理显示器"
                    );
                    None
                }
                created => Some(created),
            };
            #[cfg(windows)]
            let result = Some(result);
            if let Some(result) = result {
                if result.is_ok() && options.force_virtual && !self.has_saved_preferences {
                    self.preferences.default_virtual = true;
                    self.save_preferences()?;
                }
                return result;
            }
        }
        let saved = self.preferences.manual.clone();
        ensure!(
            saved.len() <= 3 && saved.len() + screens.len() <= 5,
            "已保存布局超出屏幕数量上限"
        );
        self.restoring = true;
        let restore = (|| -> Result<()> {
            for mut spec in saved {
                spec.identity = None;
                self.restore_virtual(spec, lease)?;
            }
            // Physical modes belong to the current desktop owner. Previous
            // connections may remember virtual resources, but must not replay
            // a physical resolution/DPI over a game or a local user's choice.
            Ok(())
        })();
        self.restoring = false;
        restore?;
        let mut screens = capture::screens()?;
        for screen in &mut screens {
            if let Some(owned) =
                self.journal.owned.iter().find(|o| {
                    o.identity.is_some() && o.identity.as_ref() == screen.identity.as_ref()
                })
            {
                screen.id = display::source_id(&format!("openuuyc-virtual/{}", owned.guid))?;
                screen.render_adapter = self.render_adapter;
            }
        }
        let selected = if let Some(automatic) = automatic.as_ref() {
            screens.iter().find(|s| s.identity == automatic.identity)
        } else if options.screen_id == -1 {
            screens.iter().find(|s| s.primary).or(screens.first())
        } else {
            screens.iter().find(|s| s.id == options.screen_id)
        }
        .cloned()
        .context("请求的显示器不可用")?;
        // The saved controller layout owns initial physical/virtual modes.
        // Incoming resolution choices apply only when no layout was saved.
        if self.has_saved_preferences {
            return Ok(selected);
        }
        let requested = match choice {
            2 => local,
            4 => size(
                options
                    .params
                    .as_ref()
                    .and_then(|p| p.chosen_resolution.as_ref()),
            ),
            _ => None,
        };
        if let Some((width, height)) = requested {
            validate_modes(&[(width, height)])?;
            if (selected.width, selected.height) == (width, height) {
                // Preserve the explicit preference without starting a recovery
                // guard or writing mutation intent for an unchanged mode.
                self.remember_physical(
                    selected.identity.as_deref().context("缺少初始显示目标")?,
                    Some(choice),
                )?;
                return Ok(selected);
            }
            let target = Topology::query(true)?
                .targets()?
                .into_iter()
                .find(|t| selected.identity.as_deref() == Some(t.identity.as_str()))
                .context("缺少初始显示目标")?;
            if !target
                .modes
                .iter()
                .any(|m| m.width == width && m.height == height)
            {
                return self.enter_super(width, height, 0, false, lease);
            }
            self.begin()?;
            let changed = self.initial_resolution(&target, width, height, lease)?;
            self.applied()?;
            if changed {
                self.remember_physical(&target.identity, Some(choice))?;
            }
            return find_screen(&target.identity);
        }
        Ok(selected)
    }
}
