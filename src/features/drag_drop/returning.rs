//! Restore the original press and OLE gesture; never construct a replacement copy.
use super::*;
impl Ticket {
    pub fn return_enter(
        &self,
        point: Point,
        position: Arc<dyn Fn() -> Option<Point> + Send + Sync>,
    ) -> Result<()> {
        ensure!(
            point.valid() && self.snapshot().stage == Stage::Dragging,
            "拖出手势已结束"
        );
        *lock(&self.point) = point;
        *lock(&self.resume_position) = Some(position);
        lock(&self.snapshot).stage = Stage::Returning;
        self.input.try_send(Command::Resume(self.id)).map_err(|_| {
            self.cancel();
            anyhow::anyhow!("拖回队列不可用")
        })
    }
    pub fn return_position(&self, point: Point) -> u32 {
        if point.valid() {
            *lock(&self.point) = point;
        }
        0
    }
    pub fn return_commit(&self, point: Point) -> Result<()> {
        ensure!(point.valid(), "拖回位置无效");
        *lock(&self.point) = point;
        Ok(())
    }
}
impl Actor {
    pub(super) fn try_start_native(&mut self, id: u64) -> Result<()> {
        let Some(entry) = self.entries.get_mut(&id) else {
            return Ok(());
        };
        if (entry.reverse && !entry.preserve_source && !entry.source_released)
            || (self.shared.return_capable.load(Ordering::Acquire) && entry.image.is_none())
        {
            return Ok(());
        }
        if let Some((offer, summary)) = entry.pending_offer.take() {
            self.start_native(id, offer, summary)?;
        }
        Ok(())
    }
    pub(super) async fn begin_return(&mut self, id: u64) -> Result<()> {
        let Some(entry) = self.entries.get_mut(&id).filter(|e| {
            e.reverse && !e.sending && !e.committed && !e.resuming && e.preserve_source
        }) else {
            return Ok(());
        };
        ensure!(
            self.shared.return_capable.load(Ordering::Acquire)
                && entry
                    .release_source
                    .as_ref()
                    .and_then(|s| s.current.as_ref())
                    .is_some_and(|f| f()),
            "原按下所有权已结束"
        );
        entry.resuming = true;
        entry.resume_at = Some(Instant::now());
        if let Some(source) = &entry.native {
            source.cancel();
        } else {
            entry.native_finished = true;
        }
        let point = *lock(&entry.ticket.point);
        self.emit(id, Payload::Resume(point), false).await?;
        Ok(())
    }
    pub(super) async fn receive_return(&mut self, id: u64, point: Point) -> Result<()> {
        ensure!(
            self.role == Role::Host
                && self.shared.return_capable.load(Ordering::Acquire)
                && self.shared.enabled.load(Ordering::Acquire),
            "拖回未获许可"
        );
        let position = self.position(point)?;
        let Some(entry) = self.entries.get_mut(&id).filter(|e| {
            e.reverse
                && e.sending
                && e.preserve_source
                && !e.committed
                && !e.source_released
                && !e.resuming
        }) else {
            self.emit(id, Payload::Resumed(false), false).await?;
            return Ok(());
        };
        let portal = entry
            .portal
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("原拖动接管已结束"))?;
        entry.resuming = true;
        entry.resume_at = Some(Instant::now());
        portal.resume(position);
        Ok(())
    }
    pub(super) async fn complete_return(&mut self, id: u64) -> Result<()> {
        let Some(entry) = self
            .entries
            .get_mut(&id)
            .filter(|e| e.resuming && !e.sending && e.native_finished && e.returned.is_some())
        else {
            return Ok(());
        };
        if entry.cancel_requested {
            return Ok(());
        }
        let provider = lock(&entry.ticket.resume_position).clone();
        let point = provider.and_then(|f| f());
        let restored = entry.returned == Some(true)
            && entry.ticket.alive.load(Ordering::Acquire)
            && point.is_some_and(|point| {
                entry
                    .release_source
                    .as_ref()
                    .and_then(|s| s.resume.as_ref())
                    .is_some_and(|f| f(point, native::left_held()))
            });
        if restored {
            // The input lease is now disarmed and still contains the original
            // press. Clearing the drag flags allows ordinary pointer routing.
            self.shared
                .pointer_owner
                .compare_exchange(id, 0, Ordering::AcqRel, Ordering::Acquire)
                .ok();
            self.shared
                .source_held
                .compare_exchange(id, 0, Ordering::AcqRel, Ordering::Acquire)
                .ok();
            if let Some(source) = entry.release_source.as_ref() {
                if let Some(wake) = source.wake.as_ref() {
                    wake();
                }
            }
            lock(&entry.ticket.snapshot).stage = Stage::Cancelled;
            self.emit(id, Payload::HandoffDone(true), false).await?;
            self.entries.remove(&id);
            tracing::info!(
                drag = id,
                "native original drag returned without a new press"
            );
        } else {
            entry.cancel_requested = true;
            self.emit(id, Payload::Cancel(true), false).await?;
        }
        Ok(())
    }
    pub(super) async fn return_tick(&mut self, id: u64) -> Result<()> {
        let entry = &self.entries[&id];
        let age = entry.resume_at.map_or(Duration::ZERO, |at| at.elapsed());
        if age > Duration::from_secs(5) {
            self.finish(id, 4, "拖回交接超时，已释放按下".into())
                .await?;
            return Ok(());
        }
        if self.role == Role::Controller
            && (!entry.ticket.alive.load(Ordering::Acquire) || age > Duration::from_secs(2))
            && !entry.cancel_requested
        {
            self.entries.get_mut(&id).unwrap().cancel_requested = true;
            self.emit(id, Payload::Cancel(true), false).await?;
        }
        Ok(())
    }
}
