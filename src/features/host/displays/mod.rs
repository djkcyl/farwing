//! Session-owned display mutations. Native work is serialized and keeps its
//! recovery intent alive even if the awaiting RPC or connection is cancelled.
pub(crate) mod fallback;
mod initial;
pub(crate) mod recovery;
mod store;
pub(crate) fn transfer_preferences(to_service: bool) -> anyhow::Result<()> {
    store::transfer_preferences(to_service)
}
use super::{Lease, capture, lock};
use crate::platform::display::{
    self,
    topology::{SavedTarget, SavedTopology, Target, Topology},
    virtual_driver::Driver,
};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use store::{Journal, Preference, Virtual};

struct Heartbeat {
    driver: Arc<Mutex<Driver>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Heartbeat {
    fn open() -> Result<Self> {
        let driver = Arc::new(Mutex::new(Driver::open()?));
        let timeout = lock(&driver).watchdog()?.timeout;
        tracing::debug!(timeout, "virtual display watchdog started");
        let stop = Arc::new(AtomicBool::new(false));
        let worker = if timeout > 0 {
            let driver = driver.clone();
            let stop = stop.clone();
            Some(
                std::thread::Builder::new()
                    .name("display-watchdog".into())
                    .spawn(move || {
                        let interval =
                            Duration::from_millis((u64::from(timeout) * 1000 / 3).clamp(100, 1000));
                        while !stop.load(Ordering::Acquire) {
                            if let Err(error) = lock(&driver).ping() {
                                tracing::warn!(%error,"virtual display watchdog failed");
                                break;
                            }
                            std::thread::park_timeout(interval);
                        }
                    })?,
            )
        } else {
            None
        };
        Ok(Self {
            driver,
            stop,
            worker,
        })
    }
}
impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

struct State {
    journal: Journal,
    path: PathBuf,
    preferences: Preference,
    has_saved_preferences: bool,
    preference_path: Option<PathBuf>,
    driver: Option<Heartbeat>,
    fallback_pin: Option<Driver>,
    render_adapter: Option<u64>,
    dirty: bool,
    restoring: bool,
}
/// The host's display layout while participants are connected. One session
/// is shared by every participant: the first one's preparation may change
/// the layout; later participants select from it. The layout is restored when
/// the last participant leaves.
pub(crate) struct Session {
    lease: Lease,
    state: Arc<Mutex<State>>,
    closed: Arc<AtomicBool>,
    users: std::sync::atomic::AtomicUsize,
    /// The screen the first preparation selected.
    prepared: Mutex<Option<capture::Screen>>,
}
impl Session {
    pub(crate) fn new(lease: Lease, remote: &str) -> Result<Arc<Self>> {
        ensure!(
            !lease.display_scope().is_empty() && lease.requested(),
            "被控显示许可不可用"
        );
        ensure!(remote.len() <= 256, "控制端标识无效");
        let root = store::root()?;
        let token = uuid::Uuid::new_v4().to_string();
        let preference_path = (!lease.is_assistance() && !remote.is_empty()).then(|| {
            root.join("preferences")
                .join(lease.display_scope())
                .join(format!("{:x}.json", Sha256::digest(remote)))
        });
        let preferences = preference_path
            .as_deref()
            .map(store::read::<Preference>)
            .transpose()?
            .flatten();
        let has_saved_preferences = preferences.is_some();
        let preferences = preferences.unwrap_or_default();
        fallback::start_background();
        Ok(Arc::new(Self {
            lease,
            closed: Arc::new(AtomicBool::new(false)),
            users: std::sync::atomic::AtomicUsize::new(1),
            prepared: Mutex::new(None),
            state: Arc::new(Mutex::new(State {
                path: root.join("recovery").join(format!("{token}.json")),
                journal: Journal {
                    mode_changes: Vec::new(),
                    owner_pid: std::process::id(),
                    owner_birth: display::recovery::current_birth()?,
                    retired: false,
                    token,
                    baseline: SavedTopology::default(),
                    dpi: Vec::new(),
                    owned: Vec::new(),
                    super_baseline: None,
                    super_dpi: Vec::new(),
                    applied: None,
                    desired: None,
                    applied_dpi: Vec::new(),
                    desired_dpi: Vec::new(),
                },
                preferences,
                has_saved_preferences,
                preference_path,
                driver: None,
                fallback_pin: None,
                render_adapter: None,
                dirty: false,
                restoring: false,
            })),
        }))
    }
    async fn run<T: Send + 'static>(
        &self,
        action: impl FnOnce(&mut State, &Lease) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        ensure!(
            !self.closed.load(Ordering::Acquire) && self.lease.requested(),
            "显示操作已取消"
        );
        let state = self.state.clone();
        let lease = self.lease.clone();
        let closed = self.closed.clone();
        let activity = crate::platform::windows::host_service::activity::Work::new();
        tokio::task::spawn_blocking(move || {
            let _activity = activity;
            let _serial = display::recovery::Serial::acquire()?;
            let mut state = lock(&state);
            ensure!(
                lease.requested() && !closed.load(Ordering::Acquire),
                "显示操作已取消"
            );
            let result = action(&mut state, &lease);
            if !lease.requested() || closed.load(Ordering::Acquire) {
                if let Err(error) = state.cleanup() {
                    tracing::error!(%error,"cancelled display operation recovery failed");
                }
            }
            result
        })
        .await
        .context("显示工作线程中断")?
    }
    pub(crate) fn annotate(
        &self,
        screen: &mut capture::Screen,
        target: &mut Option<Target>,
    ) -> Result<(i32, i32)> {
        let state = lock(&self.state);
        let Some(owned) = state
            .journal
            .owned
            .iter()
            .find(|o| o.identity.as_ref() == screen.identity.as_ref() && o.identity.is_some())
        else {
            return Ok((
                i32::from(
                    target
                        .as_ref()
                        .is_some_and(|t| t.virtual_provider.is_some()),
                ),
                state
                    .preferences
                    .physical
                    .iter()
                    .find(|p| screen.identity.as_deref() == Some(p.identity.as_str()))
                    .map_or(1, |p| p.resolution_type),
            ));
        };
        screen.id = display::source_id(&format!("farwing-virtual/{}", owned.guid))?;
        screen.render_adapter = state.render_adapter;
        if let Some(target) = target {
            for &(width, height) in &owned.modes {
                if !target
                    .modes
                    .iter()
                    .any(|m| m.width == width && m.height == height)
                {
                    target.modes.push(display::topology::Mode {
                        width,
                        height,
                        hz: owned.hz,
                    });
                }
            }
        }
        Ok((if owned.kind == 2 { 2 } else { 1 }, owned.resolution_type))
    }
    pub(crate) fn owns(&self, identity: &str) -> bool {
        lock(&self.state)
            .journal
            .owned
            .iter()
            .any(|o| o.identity.as_deref() == Some(identity))
    }
    pub(crate) fn observe_layout(&self, current: &SavedTopology) -> Result<()> {
        let mut state = lock(&self.state);
        let changes = &mut state.journal.mode_changes;
        let before = changes.len();
        changes.retain(|c| {
            c.pending
                || recovery::same_target(
                    current.targets.iter().find(|t| t.identity == c.identity),
                    c.after.as_ref(),
                )
        });
        if changes.len() != before && state.dirty {
            tracing::info!("external display mode change released session restoration ownership");
            state.save()?;
        }
        Ok(())
    }
    /// A virtual screen for participant `caller`. The layout is shared, but
    /// adding to it is the caller's own `display` grant: this session's
    /// lease carries the first participant's grants.
    pub(crate) async fn create(
        &self,
        caller: &Lease,
        resolutions: Vec<(u32, u32)>,
    ) -> Result<capture::Screen> {
        ensure!(
            caller.display_expansion_allowed(),
            "本次会话不允许添加虚拟屏"
        );
        self.run(move |state, lease| {
            ensure!(
                state.journal.super_baseline.is_none(),
                "超级屏内不能添加扩展屏"
            );
            let screens = capture::screens()?;
            ensure!(
                screens.len() < 5
                    && state
                        .journal
                        .owned
                        .iter()
                        .filter(|o| matches!(o.kind, 1 | 3))
                        .count()
                        < 3,
                "显示器数量已达上限"
            );
            let primary = screens.iter().find(|s| s.primary).or(screens.first());
            let size = primary.map_or((1920, 1080), |s| (s.width, s.height));
            let dpi = primary.and_then(|s| s.dpi_scale).unwrap_or(100);
            let mut modes = resolutions;
            modes.push(size);
            modes.sort_unstable();
            modes.dedup();
            validate_modes(&modes)?;
            let spec = Virtual {
                guid: uuid::Uuid::new_v4().to_string(),
                identity: None,
                width: size.0,
                height: size.1,
                hz: 144,
                dpi,
                kind: 1,
                resolution_type: 1,
                modes,
                layout: None,
            };
            state.create(spec, lease, false)
        })
        .await
    }
    pub(crate) async fn remove(&self, identity: String) -> Result<()> {
        self.run(move |state, lease| {
            let index = state.owned_index(&identity)?;
            ensure!(
                matches!(state.journal.owned[index].kind, 1 | 3),
                "不能用删除扩展屏操作退出超级屏"
            );
            ensure!(lease.requested(), "显示操作已取消");
            state.save()?;
            let current = Topology::query(true)?.snapshot()?;
            if current.targets.len() == 1
                && current.targets[0].identity == identity
                && !state.journal.baseline.targets.is_empty()
            {
                state.restore_layout(&state.journal.baseline, &state.journal.dpi, false)?;
            }
            let guid = uuid::Uuid::parse_str(&state.journal.owned[index].guid)?;
            lock(&state.driver()?.driver).remove(guid)?;
            if state.journal.owned[index].kind == 3 {
                state.preferences.default_virtual = false;
            }
            state.journal.owned.remove(index);
            // Retain the completed removal even if Windows is still publishing
            // the old active path while monitor departure is being processed.
            state.save()?;
            state.update_preferences()?;
            state.applied()
        })
        .await
    }
    /// The singleton fallback is owned by the machine, never journal.owned.
    pub(crate) async fn ensure_fallback(
        &self,
        previous: capture::Screen,
    ) -> Result<Option<capture::Screen>> {
        self.run(move |state, lease| {
            if state.journal.super_baseline.is_some() || !state.journal.owned.is_empty() {
                return Ok(None);
            }
            ensure!(lease.requested(), "显示操作已取消");
            fallback::ensure(
                state.fallback_driver()?,
                previous.width,
                previous.height,
                &[],
                true,
            )
        })
        .await
    }
    pub(crate) fn fallback_screen(&self) -> Result<Option<capture::Screen>> {
        let mut state = lock(&self.state);
        if state
            .fallback_pin
            .as_ref()
            .is_some_and(|d| d.fallback_state().is_err())
        {
            // A driver/device restart invalidates old handles. Re-open and pin
            // the new generation before resuming its machine-owned monitor.
            state.fallback_pin.take();
            state.fallback_pin = fallback::pin()?;
        }
        match &state.fallback_pin {
            Some(driver) => fallback::screen(driver),
            None => Ok(None),
        }
    }
    pub(crate) fn allows_fallback_retirement(&self) -> bool {
        let state = lock(&self.state);
        state.journal.owned.is_empty()
            && state.journal.super_baseline.is_none()
            && state.fallback_pin.as_ref().is_some_and(|driver| {
                driver
                    .fallback_state()
                    .is_ok_and(|(_, users)| users <= u32::from(driver.owns_fallback_pin()))
            })
    }
    pub(crate) async fn retire_fallback(&self, replacement: String) -> Result<bool> {
        self.run(move |state, _| {
            if !state.journal.owned.is_empty() || state.journal.super_baseline.is_some() {
                return Ok(false);
            }
            let retiring = fallback::screen(state.fallback_driver()?)?.and_then(|s| s.identity);
            let removed = fallback::retire(state.fallback_driver()?, Some(&replacement))?;
            if removed {
                // A session that changed modes must not later restore the
                // fallback baseline after it has been deliberately retired.
                let available = Topology::query(false)?.metadata()?;
                state.journal.baseline.targets.retain(|old| {
                    retiring.as_deref() != Some(old.identity.as_str())
                        && available
                            .iter()
                            .any(|t| t.available && t.identity == old.identity)
                });
                if state.dirty {
                    state.save()?;
                }
            }
            Ok(removed)
        })
        .await
    }
    pub(crate) async fn enter_super(
        &self,
        width: u32,
        height: u32,
        dpi: u32,
        use_saved: bool,
    ) -> Result<capture::Screen> {
        self.run(move |state, lease| state.enter_super(width, height, dpi, use_saved, lease))
            .await
    }
    pub(crate) async fn quit_super(&self) -> Result<()> {
        self.run(|state, _| state.quit_super(true)).await
    }
    pub(crate) async fn resolution_choice(&self, identity: String, kind: i32) -> Result<()> {
        ensure!((2..=4).contains(&kind), "无效分辨率选择");
        self.run(move |state, _| {
            if let Some(owned) = state
                .journal
                .owned
                .iter_mut()
                .find(|o| o.identity.as_deref() == Some(identity.as_str()))
            {
                owned.resolution_type = kind;
                state.save()?;
                state.update_preferences()
            } else {
                state.remember_physical(&identity, Some(kind))
            }
        })
        .await
    }
    pub(crate) async fn set_resolution(
        &self,
        target: Target,
        width: u32,
        height: u32,
    ) -> Result<capture::Screen> {
        self.run(move |state, lease| {
            if let Ok(current) = find_screen(&target.identity) {
                if (current.width, current.height) == (width, height) {
                    return Ok(current);
                }
            }
            state.begin()?;
            if state
                .journal
                .owned
                .iter()
                .any(|o| o.identity.as_deref() == Some(target.identity.as_str()))
            {
                return state.resize_virtual(target.identity, width, height, 0, lease);
            }
            let previous = state.journal.mode_changes.clone();
            state.intend_mode(&target.identity, width, height)?;
            if let Err(error) = target.set_resolution(width, height, || lease.requested()) {
                if error
                    .downcast_ref::<display::topology::ModeTestRejected>()
                    .is_some()
                {
                    state.journal.mode_changes = previous;
                    state.journal.desired = None;
                    state.save()?;
                }
                return Err(error);
            }
            state.applied()?;
            state.remember_physical(&target.identity, None)?;
            find_screen(&target.identity)
        })
        .await
    }
    pub(crate) async fn set_dpi(&self, target: Target, dpi: u32) -> Result<()> {
        self.run(move |state, lease| {
            state.begin()?;
            state.intend_dpi(&target.identity, dpi)?;
            target.set_dpi(dpi, || lease.requested())?;
            if let Some(owned) = state
                .journal
                .owned
                .iter_mut()
                .find(|o| o.identity.as_deref() == Some(target.identity.as_str()))
            {
                owned.dpi = dpi;
            }
            state.applied()?;
            state.remember_physical(&target.identity, None)?;
            state.update_preferences()
        })
        .await
    }
    /// Another participant starts using this layout.
    pub(crate) fn attach(self: &Arc<Self>) -> Option<Arc<Self>> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        self.users.fetch_add(1, Ordering::AcqRel);
        Some(self.clone())
    }
    /// Whether the session still serves participants.
    pub(crate) fn is_open(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && self.lease.requested()
    }
    /// One participant is done with the layout; the last one restores it.
    pub(crate) async fn close(&self) -> Result<()> {
        if self.users.fetch_sub(1, Ordering::AcqRel) > 1 {
            return Ok(());
        }
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let state = self.state.clone();
        let activity = crate::platform::windows::host_service::activity::Work::new();
        tokio::task::spawn_blocking(move || {
            let _activity = activity;
            let _serial = display::recovery::Serial::acquire()?;
            let mut state = lock(&state);
            if state.dirty {
                state.journal.retired = true;
                state.save()?;
            }
            state.cleanup()?;
            state.fallback_pin.take();
            Ok(())
        })
        .await?
    }
    pub(crate) async fn rollback_preparation(&self) -> Result<()> {
        // Other participants keep using a shared layout.
        if self.users.load(Ordering::Acquire) > 1 {
            return Ok(());
        }
        self.run(|state, _| state.cleanup()).await
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let state = self.state.clone();
        let _ = std::thread::Builder::new()
            .name("display-cleanup".into())
            .spawn(move || -> Result<()> {
                let _serial = display::recovery::Serial::acquire()?;
                let mut state = lock(&state);
                if state.dirty {
                    state.journal.retired = true;
                    state.save()?;
                }
                if let Err(error) = state.cleanup() {
                    tracing::error!(%error,"display recovery remains pending");
                }
                state.fallback_pin.take();
                Ok(())
            });
    }
}

fn validate_modes(modes: &[(u32, u32)]) -> Result<()> {
    ensure!(
        !modes.is_empty()
            && modes.len() <= 64
            && modes
                .iter()
                .all(|(w, h)| (2..=16384).contains(w) && (2..=16384).contains(h)),
        "无效虚拟显示模式"
    );
    Ok(())
}
fn remove_owned(driver: &Driver, guid: &str) -> Result<()> {
    match driver.remove(uuid::Uuid::parse_str(guid)?) {
        Ok(()) => Ok(()),
        Err(error)
            if error
                .downcast_ref::<windows::core::Error>()
                .is_some_and(|e| e.code() == windows::core::HRESULT::from_win32(1168)) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}
fn current_dpi() -> Result<Vec<(String, u32)>> {
    Ok(Topology::query(true)?
        .metadata()?
        .into_iter()
        .filter_map(|t| t.dpi.map(|d| (t.identity, d.current)))
        .collect())
}
fn find_screen(identity: &str) -> Result<capture::Screen> {
    capture::screens()?
        .into_iter()
        .find(|s| s.identity.as_deref() == Some(identity))
        .context("显示目标尚未进入桌面")
}
fn restore_dpi(values: &[(String, u32)]) -> Result<()> {
    let targets = Topology::query(true)?.metadata()?;
    for (identity, dpi) in values {
        if let Some(target) = targets.iter().find(|t| &t.identity == identity) {
            target.set_dpi(*dpi, || true)?;
        }
    }
    Ok(())
}

impl State {
    fn fallback_driver(&mut self) -> Result<&Driver> {
        if self
            .fallback_pin
            .as_ref()
            .is_some_and(|d| d.fallback_state().is_err())
        {
            self.fallback_pin.take();
        }
        if self.fallback_pin.is_none() {
            self.fallback_pin = fallback::pin()?;
        }
        self.fallback_pin
            .as_ref()
            .context("常驻无屏显示需要安装或更新虚拟显示驱动")
    }
    fn enter_super(
        &mut self,
        width: u32,
        height: u32,
        dpi: u32,
        use_saved: bool,
        lease: &Lease,
    ) -> Result<capture::Screen> {
        let size = if use_saved {
            self.preferences.super_size.unwrap_or((width, height))
        } else {
            (width, height)
        };
        validate_modes(&[size])?;
        let dpi = if use_saved && self.preferences.super_dpi > 0 {
            self.preferences.super_dpi
        } else {
            dpi
        };
        if let Some(owned) = self.journal.owned.iter().find(|o| o.kind == 2).cloned() {
            return self.resize_virtual(
                owned.identity.context("超级屏尚未就绪")?,
                size.0,
                size.1,
                dpi,
                lease,
            );
        }
        self.begin()?;
        self.journal.super_baseline = Some(Topology::query(true)?.snapshot()?);
        self.journal.super_dpi = current_dpi()?;
        self.save()?;
        let spec = Virtual {
            guid: uuid::Uuid::new_v4().to_string(),
            identity: None,
            width: size.0,
            height: size.1,
            hz: 144,
            dpi,
            kind: 2,
            resolution_type: if use_saved
                && (2..=4).contains(&self.preferences.super_resolution_type)
            {
                self.preferences.super_resolution_type
            } else {
                4
            },
            modes: vec![size],
            layout: None,
        };
        let screen = match self.create(spec, lease, true) {
            Ok(screen) => screen,
            Err(error) => {
                if !self.journal.owned.iter().any(|o| o.kind == 2) {
                    self.journal.super_baseline = None;
                    self.journal.super_dpi.clear();
                    if let Err(save) = self.save() {
                        tracing::error!(%save,"failed to update display rollback record");
                    }
                }
                return Err(error);
            }
        };
        self.preferences.super_enabled = true;
        self.preferences.super_size = Some(size);
        self.preferences.super_dpi = screen.dpi_scale.unwrap_or(dpi);
        self.save_preferences()?;
        Ok(screen)
    }
    fn begin(&mut self) -> Result<()> {
        if !self.dirty {
            self.journal.baseline = Topology::query(true)?.snapshot()?;
            self.journal.dpi = current_dpi()?;
            self.journal.applied = Some(self.journal.baseline.clone());
            self.journal.applied_dpi = self.journal.dpi.clone();
            self.save()?;
            if let Err(error) = if crate::platform::windows::host_service::resident::is_owner() {
                // The service supervises this persistent owner and its next
                // incarnation replays the durable recovery journal.
                Ok(())
            } else {
                display::recovery::start_guard(&self.journal.token)
            } {
                let _ = std::fs::remove_file(&self.path);
                return Err(error);
            }
            self.dirty = true;
        }
        self.save()
    }
    fn save(&self) -> Result<()> {
        store::write(&self.path, &self.journal)
    }
    fn save_preferences(&self) -> Result<()> {
        if let Some(path) = &self.preference_path {
            let mut saved = self.preferences.clone();
            saved.physical.clear();
            store::write(path, &saved)?;
        }
        Ok(())
    }
    fn update_preferences(&mut self) -> Result<()> {
        if self.restoring {
            return Ok(());
        }
        self.preferences.manual = self
            .journal
            .owned
            .iter()
            .filter(|o| o.kind == 1)
            .cloned()
            .collect();
        if let Some(super_screen) = self.journal.owned.iter().find(|o| o.kind == 2) {
            self.preferences.super_size = Some((super_screen.width, super_screen.height));
            self.preferences.super_dpi = super_screen.dpi;
            self.preferences.super_resolution_type = super_screen.resolution_type;
        }
        self.save_preferences()
    }
    fn remember_physical(&mut self, identity: &str, choice: Option<i32>) -> Result<()> {
        if self.restoring
            || self
                .journal
                .owned
                .iter()
                .any(|o| o.identity.as_deref() == Some(identity))
        {
            return Ok(());
        }
        let screen = find_screen(identity)?;
        let resolution_type = choice.unwrap_or_else(|| {
            self.preferences
                .physical
                .iter()
                .find(|p| p.identity == identity)
                .map_or(1, |p| p.resolution_type)
        });
        self.preferences.physical.retain(|p| p.identity != identity);
        self.preferences.physical.push(store::Physical {
            identity: identity.into(),
            width: screen.width,
            height: screen.height,
            dpi: screen.dpi_scale.unwrap_or(0),
            resolution_type,
        });
        Ok(())
    }
    fn applied(&mut self) -> Result<()> {
        // Only poll idempotent observations. A successful IOCTL/display request
        // is never replayed just because Windows has not converged yet.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let (snapshot, dpi) = loop {
            let observed =
                (|| -> Result<_> { Ok((Topology::query(true)?.snapshot()?, current_dpi()?)) })();
            match observed {
                Ok(state) => break state,
                Err(error) if std::time::Instant::now() >= deadline => return Err(error),
                Err(_) => std::thread::sleep(Duration::from_millis(25)),
            }
        };
        for owned in &mut self.journal.owned {
            if let Some(target) = snapshot
                .targets
                .iter()
                .find(|t| owned.identity.as_deref() == Some(t.identity.as_str()))
            {
                owned.layout = Some(target.clone());
                owned.width = target.width;
                owned.height = target.height;
                if let Some((_, value)) = dpi
                    .iter()
                    .find(|(id, _)| owned.identity.as_deref() == Some(id.as_str()))
                {
                    owned.dpi = *value;
                }
            }
        }
        {
            let changes = &mut self.journal.mode_changes;

            for change in changes.iter_mut().filter(|c| c.pending) {
                let observed = snapshot
                    .targets
                    .iter()
                    .find(|t| t.identity == change.identity);
                if let (Some(expected), Some(observed)) = (&change.after, observed) {
                    if (
                        expected.width,
                        expected.height,
                        expected.left,
                        expected.top,
                        expected.rotation,
                    ) == (
                        observed.width,
                        observed.height,
                        observed.left,
                        observed.top,
                        observed.rotation,
                    ) {
                        change.after = Some(observed.clone());
                    }
                }
                change.pending = false;
            }
            changes.retain(|c| !recovery::same_target(c.before.as_ref(), c.after.as_ref()));
        }
        // Observation is not ownership: unrelated game/local changes must not
        // become our expected modes merely because another operation completed.
        if self.journal.desired.is_some() {
            self.journal.applied = Some(snapshot);
        }
        for (id, expected) in &self.journal.desired_dpi {
            if let Some(value) = dpi
                .iter()
                .find(|(key, value)| key == id && value == expected)
            {
                self.journal.applied_dpi.retain(|(key, _)| key != id);
                self.journal.applied_dpi.push(value.clone());
            }
        }
        self.journal.desired = None;
        self.journal.desired_dpi.clear();
        self.save()
    }
    fn driver(&mut self) -> Result<&Heartbeat> {
        if self.driver.is_none() {
            let instance = Driver::device_instance()?;
            let targets = Topology::query(false)?.targets()?;
            for target in targets.iter().filter(|t| t.available) {
                ensure!(!target.adapter_path.is_empty(), "无法核实显示器的驱动归属");
                let path = target
                    .adapter_path
                    .trim_start_matches(r"\\?\")
                    .trim_start_matches(r"\??\");
                let owner = path.split("#{").next().unwrap_or(path).replace('#', r"\");
                ensure!(
                    owner != instance
                        || self
                            .fallback_pin
                            .as_ref()
                            .map(|d| d.fallback())
                            .transpose()?
                            .flatten()
                            .and_then(|o| {
                                Topology::query(false)
                                    .ok()?
                                    .identity_of(o.adapter, o.target)
                            })
                            .is_some_and(|identity| identity == target.identity)
                        || self
                            .journal
                            .owned
                            .iter()
                            .any(|o| o.identity.as_deref() == Some(target.identity.as_str())),
                    "虚拟显示驱动已有其他程序创建的显示器"
                );
            }
            self.driver = Some(Heartbeat::open()?);
            if let Some(adapter) = capture::encoding_adapters()?.first() {
                // Reasserting the render adapter can disturb a live fallback.
                // All displays on our adapter already share its selected GPU.
                if !targets.iter().any(|t| {
                    t.available
                        && t.virtual_provider == Some(display::topology::VirtualProvider::Farwing)
                }) {
                    lock(&self.driver.as_ref().unwrap().driver).render_adapter(adapter.luid)?;
                }
                self.render_adapter = Some(adapter.luid);
            }
        }
        Ok(self.driver.as_ref().unwrap())
    }
    fn owned_index(&self, identity: &str) -> Result<usize> {
        self.journal
            .owned
            .iter()
            .position(|o| o.identity.as_deref() == Some(identity))
            .context("不是本次会话创建的虚拟显示器")
    }
    fn create(
        &mut self,
        spec: Virtual,
        lease: &Lease,
        super_screen: bool,
    ) -> Result<capture::Screen> {
        let before = Topology::query(true)?.snapshot()?;
        let dpi = current_dpi()?;
        let preferences = self.preferences.clone();
        let guid = spec.guid.clone();
        let result = self.create_inner(spec, lease, super_screen);
        if result.is_err() && self.journal.owned.iter().any(|o| o.guid == guid) {
            let recovery = (|| -> Result<()> {
                if !before.targets.is_empty() {
                    before.apply()?;
                    restore_dpi(&dpi)?;
                }
                let driver = self.driver()?.driver.clone();
                remove_owned(&lock(&driver), &guid)?;
                self.journal.owned.retain(|o| o.guid != guid);
                self.preferences = preferences;
                self.save_preferences()?;
                self.applied()
            })();
            if let Err(error) = recovery {
                tracing::error!(%error,"virtual display creation rollback remains pending");
            }
        }
        result
    }
    fn create_inner(
        &mut self,
        spec: Virtual,
        lease: &Lease,
        super_screen: bool,
    ) -> Result<capture::Screen> {
        let driver = self.driver()?.driver.clone();
        self.begin()?;
        let index = self.journal.owned.len();
        self.journal.owned.push(spec.clone());
        self.save()?;
        ensure!(lease.requested(), "显示操作已取消");
        let output = lock(&driver).add(
            uuid::Uuid::parse_str(&spec.guid)?,
            spec.width,
            spec.height,
            spec.hz,
        )?;
        // Monitor arrival and activation are separate from the IOCTL result.
        // The monitor is found by identity: Windows may also present it
        // through the render adapter, under another adapter and target ID.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let target = loop {
            ensure!(lease.requested(), "显示操作已取消");
            let topology = Topology::query(false)?;
            if let Some(identity) = topology.identity_of(output.adapter, output.target)
                && let Some(target) = topology
                    .targets()?
                    .into_iter()
                    .find(|t| t.identity == identity)
            {
                break target;
            }
            ensure!(
                std::time::Instant::now() < deadline,
                "虚拟显示器创建后未出现"
            );
            std::thread::sleep(Duration::from_millis(25));
        };
        self.journal.owned[index].identity = Some(target.identity.clone());
        self.save()?;
        // A physical monitor may return while the IDD monitor is arriving.
        // Preserve its current layout instead of applying the earlier empty one.
        let mut before = Topology::query(true)?.snapshot()?;
        before.targets.retain(|t| t.identity != target.identity);
        let default_left = before
            .targets
            .iter()
            .map(|t| i64::from(t.left) + i64::from(t.width))
            .max()
            .unwrap_or(0);
        let (left, top) = spec
            .layout
            .as_ref()
            .filter(|layout| {
                !before.targets.iter().any(|old| {
                    i64::from(layout.left) < i64::from(old.left) + i64::from(old.width)
                        && i64::from(old.left) < i64::from(layout.left) + i64::from(spec.width)
                        && i64::from(layout.top) < i64::from(old.top) + i64::from(old.height)
                        && i64::from(old.top) < i64::from(layout.top) + i64::from(spec.height)
                })
            })
            .map_or((default_left, 0), |layout| {
                (i64::from(layout.left), layout.top)
            });
        let mut desired = before;
        desired.targets.push(SavedTarget {
            identity: target.identity.clone(),
            source_group: format!("virtual/{}", spec.guid),
            left: i32::try_from(left)?,
            top,
            width: spec.width,
            height: spec.height,
            rotation: 1,
            refresh_numerator: spec.hz,
            refresh_denominator: 1,
            scan_line_ordering: 1,
        });
        ensure!(lease.requested(), "显示操作已取消");
        self.intend_layout(&desired)?;
        // Windows attaches a new monitor by itself as well (for example as a
        // clone of the primary), which leaves its own path unavailable for a
        // moment. Applying the layout undoes that; the next attempt places it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            ensure!(lease.requested(), "显示操作已取消");
            let missing = desired.apply()?;
            if !missing.contains(&target.identity) {
                break;
            }
            ensure!(
                std::time::Instant::now() < deadline,
                "新虚拟屏未能加入显示布局"
            );
            tracing::debug!("new virtual display not yet available to the layout; retrying");
            std::thread::sleep(Duration::from_millis(200));
        }
        // Windows activates the new path shortly after the layout returns.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            ensure!(lease.requested(), "显示操作已取消");
            match target.revalidate() {
                Ok(current) if current.active => break,
                current => ensure!(
                    std::time::Instant::now() < deadline,
                    "新虚拟屏已加入布局但未启用（{}）",
                    current.map_or_else(|e| format!("{e:#}"), |_| "未激活".into())
                ),
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        tracing::info!(identity = %target.identity, "virtual display active");
        if spec.dpi > 0 {
            self.intend_dpi(&target.identity, spec.dpi)?;
            target.set_dpi(spec.dpi, || lease.requested())?;
        }
        if super_screen {
            let mut only = desired.targets.last().cloned().context("缺少超级屏布局")?;
            only.left = 0;
            only.top = 0;
            ensure!(lease.requested(), "显示操作已取消");
            let layout = SavedTopology {
                targets: vec![only],
            };
            self.intend_layout(&layout)?;
            layout.apply()?;
        }
        self.applied()?;
        let mut screen = find_screen(&target.identity)?;
        screen.id = display::source_id(&format!("farwing-virtual/{}", spec.guid))?;
        screen.render_adapter = self.render_adapter;
        if spec.kind == 1 {
            self.update_preferences()?;
        }
        Ok(screen)
    }
    fn resize_virtual(
        &mut self,
        identity: String,
        width: u32,
        height: u32,
        dpi: u32,
        lease: &Lease,
    ) -> Result<capture::Screen> {
        validate_modes(&[(width, height)])?;
        let index = self.owned_index(&identity)?;
        let original = self.journal.owned[index].clone();
        let before = Topology::query(true)?.snapshot()?;
        let before_dpi = current_dpi()?;
        let target = Topology::query(false)?
            .targets()?
            .into_iter()
            .find(|t| t.identity == identity)
            .context("虚拟屏已断开")?;
        if target
            .modes
            .iter()
            .any(|m| m.width == width && m.height == height)
        {
            self.intend_mode(&identity, width, height)?;
            target.set_resolution(width, height, || lease.requested())?;
            if dpi > 0 {
                self.intend_dpi(&identity, dpi)?;
                target.set_dpi(dpi, || lease.requested())?;
            }
            self.applied()?;
            self.update_preferences()?;
            let mut screen = find_screen(&identity)?;
            screen.id = display::source_id(&format!("farwing-virtual/{}", original.guid))?;
            screen.render_adapter = self.render_adapter;
            return Ok(screen);
        }
        let result = (|| -> Result<capture::Screen> {
            ensure!(lease.requested(), "显示操作已取消");
            self.save()?;
            remove_owned(&lock(&self.driver()?.driver), &original.guid)?;
            self.journal.owned.remove(index);
            let mut spec = original.clone();
            spec.identity = None;
            spec.width = width;
            spec.height = height;
            // A newly recreated monitor has a new recommended DPI range. No
            // explicit DPI request means let Windows choose its valid scale.
            spec.dpi = dpi;
            if !spec.modes.contains(&(width, height)) {
                spec.modes.push((width, height));
            }
            let screen = self.create(spec, lease, false)?;
            self.restore_recreated_layout(&before, &identity, &screen, width, height)?;
            self.applied()?;
            self.update_preferences()?;
            Ok(screen)
        })();
        if result.is_err() && lease.requested() {
            let rollback = (|| -> Result<()> {
                remove_owned(&lock(&self.driver()?.driver), &original.guid)?;
                self.journal.owned.retain(|o| o.guid != original.guid);
                let mut spec = original.clone();
                spec.identity = None;
                let screen = self.create(spec, lease, false)?;
                self.restore_recreated_layout(
                    &before,
                    &identity,
                    &screen,
                    original.width,
                    original.height,
                )?;
                let dpi: Vec<_> = before_dpi
                    .into_iter()
                    .map(|(id, dpi)| {
                        if id == identity {
                            (screen.identity.clone().unwrap_or(id), dpi)
                        } else {
                            (id, dpi)
                        }
                    })
                    .collect();
                restore_dpi(&dpi)?;
                self.applied()?;
                self.update_preferences()
            })();
            if let Err(error) = rollback {
                tracing::error!(%error,"virtual display resize rollback remains pending");
            }
        }
        result
    }
    fn restore_recreated_layout(
        &mut self,
        before: &SavedTopology,
        identity: &str,
        screen: &capture::Screen,
        width: u32,
        height: u32,
    ) -> Result<()> {
        let next = screen.identity.as_ref().context("缺少新目标身份")?;
        let mut layout = before.clone();
        if let Some(target) = layout.targets.iter_mut().find(|t| t.identity == identity) {
            target.identity = next.clone();
            target.width = width;
            target.height = height;
        }
        if let Some(saved) = self.journal.super_baseline.as_mut() {
            for target in &mut saved.targets {
                if target.identity == identity {
                    target.identity = next.clone();
                }
            }
        }
        self.intend_layout(&layout)?;
        layout.apply()?;
        Ok(())
    }
    fn quit_super(&mut self, explicit: bool) -> Result<()> {
        if let Some(layout) = self.journal.super_baseline.clone() {
            if !layout.targets.is_empty() {
                let restored = self.restore_layout(&layout, &self.journal.super_dpi, true)?;
                {
            let changes = &mut self.journal.mode_changes;

                    for change in changes
                        .iter_mut()
                        .filter(|c| restored.contains(&c.identity))
                    {
                        change.after = layout
                            .targets
                            .iter()
                            .find(|t| t.identity == change.identity)
                            .cloned();
                        change.pending = false;
                    }
                    changes.retain(|c| !recovery::same_target(c.before.as_ref(), c.after.as_ref()));
                }
            }
            let guids: Vec<_> = self
                .journal
                .owned
                .iter()
                .filter(|o| o.kind == 2)
                .map(|o| o.guid.clone())
                .collect();
            for guid in &guids {
                lock(&self.driver()?.driver).remove(uuid::Uuid::parse_str(guid)?)?;
            }
            self.journal.owned.retain(|o| o.kind != 2);
            self.journal.super_baseline = None;
            self.journal.super_dpi.clear();
            self.applied()?;
        }
        if explicit {
            self.preferences.super_enabled = false;
            self.save_preferences()?;
        }
        Ok(())
    }
    fn cleanup(&mut self) -> Result<()> {
        if !self.dirty {
            self.driver.take();
            return Ok(());
        }
        self.restore_layout(&self.journal.baseline, &self.journal.dpi, false)?;
        if !self.journal.owned.is_empty() && self.fallback_pin.is_some() {
            let leaving: Vec<_> = self
                .journal
                .owned
                .iter()
                .filter_map(|o| o.identity.clone())
                .collect();
            // A manual/super screen may be the last output after the physical
            // monitor was unplugged. Establish the persistent replacement first.
            fallback::ensure(self.fallback_driver()?, 1920, 1080, &leaving, false)?;
        }
        if !self.journal.owned.is_empty() {
            let driver = self.driver()?.driver.clone();
            for owned in &self.journal.owned {
                remove_owned(&lock(&driver), &owned.guid)?;
            }
        }
        self.journal.owned.clear();
        self.journal.super_baseline = None;
        self.driver.take();
        self.dirty = false;
        self.journal.mode_changes = Vec::new();
        self.journal.desired = None;
        self.journal.desired_dpi.clear();
        match std::fs::remove_file(&self.path) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
}
