//! Screen mutations keep media owners separate from the native topology owner.
use super::*;

pub(super) struct Running {
    streams: Vec<(usize, i32, VideoConfig)>,
    selected: i32,
}

impl Screens {
    fn running(&self) -> Running {
        let streams = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                slot.screen
                    .as_ref()
                    .map(|s| (index, s.id, *lock(&slot.config)))
            })
            .collect();
        Running {
            streams,
            selected: self.reports.current.load(Ordering::Acquire),
        }
    }
    async fn resume(&mut self, running: Running) -> Result<()> {
        self.refresh()?;
        for (index, id, config) in running.streams {
            if let Ok(info) = self.info(id) {
                self.start_at(index, info.screen, config).await?;
            }
        }
        if self.active_slot(running.selected).is_some() {
            self.reports
                .current
                .store(running.selected, Ordering::Release);
            self.reports.notify();
        }
        Ok(())
    }
    async fn fallback(&mut self) -> Result<()> {
        self.refresh()?;
        if self.slots.iter().any(|s| s.screen.is_some()) {
            if self
                .active_slot(self.reports.current.load(Ordering::Acquire))
                .is_none()
            {
                if let Some(screen) = self
                    .slots
                    .iter()
                    .filter_map(|s| s.screen.as_ref())
                    .min_by_key(|s| !s.primary)
                {
                    self.reports.current.store(screen.id, Ordering::Release);
                    self.reports.notify();
                }
            }
            return Ok(());
        }
        let id = {
            let catalog = lock(&self.reports.catalog);
            catalog
                .iter()
                .find(|s| s.screen.primary)
                .or(catalog.first())
                .map(|s| s.screen.id)
                .context("没有可用的显示器")?
        };
        self.start(id).await
    }
    pub(crate) async fn create_virtual(&mut self, resolutions: Vec<(u32, u32)>) -> Result<()> {
        ensure!(
            self.available_slot().is_some() || self.registered.len() == 1,
            "没有空闲的已登记视频轨道"
        );
        let before = self.running();
        let screen = self.displays.create(&self.lease, resolutions).await?;
        if let Err(error) = self.start(screen.id).await {
            if let Some(identity) = screen.identity {
                if let Err(cleanup) = self.displays.remove(identity).await {
                    tracing::error!(%cleanup,"new screen rollback failed");
                }
            }
            self.resume(before).await?;
            return Err(error);
        }
        Ok(())
    }
    pub(crate) async fn remove_virtual(&mut self, id: i32) -> Result<()> {
        let info = self.info(id)?;
        ensure!(info.kind == 1, "目标不是扩展虚拟屏");
        let identity = info.screen.identity.context("缺少显示目标身份")?;
        // A virtual-screen wire type is not a lease on another driver's screen.
        // Reject unsupported removal before interrupting its active capture.
        ensure!(
            self.displays.owns(&identity),
            "此虚拟屏由其他会话或程序管理"
        );
        let before = self.running();
        self.stop(id).await?;
        if let Err(error) = self.displays.remove(identity).await {
            self.resume(before).await?;
            return Err(error);
        }
        for slot in &mut self.slots {
            if slot.suspended.as_ref().is_some_and(|s| s.id == id) {
                slot.suspended = None;
            }
        }
        self.fallback().await
    }
    pub(crate) async fn enter_super(
        &mut self,
        width: u32,
        height: u32,
        dpi: u32,
        saved: bool,
    ) -> Result<i32> {
        ensure!(!self.registered.is_empty(), "没有已登记视频轨道");
        let before = self.running();
        self.stop(-1).await?;
        let result = self.displays.enter_super(width, height, dpi, saved).await;
        let screen = match result {
            Ok(screen) => screen,
            Err(error) => {
                self.resume(before).await?;
                return Err(error);
            }
        };
        if self.before_super.is_none() {
            self.before_super = Some(before);
        }
        self.refresh()?;
        self.start(screen.id).await?;
        Ok(screen.id)
    }
    pub(crate) async fn quit_super(&mut self) -> Result<()> {
        let before = self.running();
        self.stop(-1).await?;
        if let Err(error) = self.displays.quit_super().await {
            self.resume(before).await?;
            return Err(error);
        }
        for slot in &mut self.slots {
            slot.suspended = None;
        }
        if let Some(before) = self.before_super.take() {
            self.resume(before).await?;
        }
        self.fallback().await
    }
    pub(crate) async fn set_resolution(
        &mut self,
        info: ScreenInfo,
        width: u32,
        height: u32,
    ) -> Result<i32> {
        let target = info.target.context("缺少真实显示目标")?;
        if !self.displays.owns(&target.identity) {
            self.displays.set_resolution(target, width, height).await?;
            self.refresh()?;
            return Ok(info.screen.id);
        }
        let mut running = self.running();
        running.streams.retain(|(_, id, _)| *id == info.screen.id);
        // Resizing the current temporary screen is not a new screen selection.
        self.stop_media(info.screen.id).await?;
        let result = self.displays.set_resolution(target, width, height).await;
        self.resume(running).await?;
        Ok(result?.id)
    }
}
