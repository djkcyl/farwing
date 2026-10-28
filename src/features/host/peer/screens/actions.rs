//! Screen mutations keep media owners separate from the native topology owner.
use super::*;
use crate::features::stream_control::publisher::refuse;
use crate::features::stream_control::wire::{DisplayKind, display_reason};
use crate::i18n::t;
use farwing_protocol::peer::error_code;

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
                .context(t().features.screen_none_available)?
        };
        self.start(id).await
    }
    /// A virtual display of this size and scale (0: the primary's), shown
    /// on a free track.
    pub(crate) async fn add_virtual(&mut self, width: u32, height: u32, scale: u32) -> Result<()> {
        if self.available_slot().is_none() {
            return Err(refuse(error_code::BUSY, display_reason::NO_FREE_TRACK));
        }
        let before = self.running();
        let screen = self
            .displays
            .create(&self.lease, (width, height), scale)
            .await?;
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
        // A virtual display of another product, or the machine's fallback,
        // is not this session's to remove.
        let identity = info
            .screen
            .identity
            .filter(|identity| info.kind == DisplayKind::Virtual && self.displays.owns(identity))
            .ok_or_else(|| refuse(error_code::BAD_REQUEST, display_reason::NOT_REMOVABLE))?;
        let before = self.running();
        self.halt(id).await?;
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
    /// One super screen replaces the layout. The tracks of this participant
    /// wait for their displays: they resume when the layout comes back,
    /// whoever ends the super screen.
    pub(crate) async fn enter_super(&mut self, width: u32, height: u32, scale: u32) -> Result<i32> {
        let before = self.running();
        self.halt(-1).await?;
        for slot in &mut self.slots {
            slot.awaiting_source = slot.suspended.is_some();
        }
        let screen = match self.displays.enter_super(width, height, scale).await {
            Ok(screen) => screen,
            Err(error) => {
                for slot in &mut self.slots {
                    slot.awaiting_source = false;
                }
                self.resume(before).await?;
                return Err(error);
            }
        };
        self.refresh()?;
        self.start(screen.id).await?;
        Ok(screen.id)
    }
    pub(crate) async fn quit_super(&mut self) -> Result<()> {
        let exclusive: Vec<_> = lock(&self.reports.catalog)
            .iter()
            .filter(|info| info.kind == DisplayKind::Exclusive)
            .map(|info| info.screen.id)
            .collect();
        let before = self.running();
        for id in &exclusive {
            self.halt(*id).await?;
        }
        if let Err(error) = self.displays.quit_super().await {
            self.resume(before).await?;
            return Err(error);
        }
        for slot in &mut self.slots {
            if slot.suspended.as_ref().is_some_and(|s| exclusive.contains(&s.id)) {
                slot.suspended = None;
                slot.awaiting_source = false;
            }
        }
        self.refresh()?;
        for index in 0..self.slots.len() {
            self.reattach(index).await?;
        }
        self.fallback().await
    }
    /// One of the display's modes; a virtual display takes any size.
    pub(crate) async fn set_mode(&mut self, info: ScreenInfo, width: u32, height: u32) -> Result<()> {
        if (info.screen.width, info.screen.height) == (width, height) {
            return Ok(());
        }
        let resizable = matches!(info.kind, DisplayKind::Virtual | DisplayKind::Exclusive);
        let listed = info.target.as_ref().is_some_and(|t| {
            t.modes
                .iter()
                .any(|m| m.width == width && m.height == height)
        });
        if !resizable && !listed {
            return Err(refuse(error_code::BAD_REQUEST, display_reason::UNSUPPORTED_MODE));
        }
        let target = info
            .target
            .clone()
            .context(t().features.screen_physical_target_missing)?;
        if !self.displays.owns(&target.identity) {
            self.displays.set_resolution(target, width, height).await?;
            return self.refresh();
        }
        let mut running = self.running();
        running.streams.retain(|(_, id, _)| *id == info.screen.id);
        // Resizing a virtual display recreates it; its capture follows.
        self.stop_media(info.screen.id).await?;
        let result = self.displays.set_resolution(target, width, height).await;
        self.resume(running).await?;
        result.map(drop)
    }
    pub(crate) async fn set_scale(&mut self, info: ScreenInfo, scale: u32) -> Result<()> {
        let target = info
            .target
            .filter(|t| t.dpi.as_ref().is_some_and(|d| d.supported.contains(&scale)))
            .ok_or_else(|| refuse(error_code::BAD_REQUEST, display_reason::INVALID))?;
        self.displays.set_dpi(target, scale).await?;
        self.refresh()
    }
}
