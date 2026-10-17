//! Machine-owned headless display. It is deliberately absent from session
//! journals/preferences: closing a connection must not unplug the only screen.
use super::*;
use crate::platform::display::{
    topology::VirtualProvider,
    virtual_driver::{FALLBACK_ID, Output},
};
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    width: u32,
    height: u32,
    hz: u32,
}

fn path() -> Result<PathBuf> {
    // Display recovery is independent from account login/authorization. SYSTEM
    // keeps using the machine directory even after the account vault is disabled.
    let root = if crate::platform::host_service::vault::sid(std::process::id())? == "S-1-5-18" {
        crate::platform::host_service::vault::root()?.join("displays")
    } else {
        store::root()?
    };
    Ok(root.join("fallback.json"))
}
fn clear_record() -> Result<()> {
    match std::fs::remove_file(path()?) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
pub(super) fn target(output: Output, targets: &[Target]) -> Option<&Target> {
    targets.iter().find(|t| {
        t.adapter == output.adapter
            && t.target == output.target
            && t.adapter_path
                .to_ascii_lowercase()
                .contains("#openuuycdisplay#")
    })
}
fn replacement(t: &Target, own: Option<Output>) -> bool {
    t.active
        && t.available
        && t.virtual_provider != Some(VirtualProvider::Uu)
        && own.is_none_or(|o| o.adapter != t.adapter || o.target != t.target)
}
pub(crate) fn pin() -> Result<Option<Driver>> {
    if Driver::device_instance().is_err() {
        return Ok(None);
    }
    let driver = Driver::open()?;
    if !driver.persistent_supported() {
        return Ok(None);
    }
    driver.pin_fallback()?;
    Ok(Some(driver))
}
pub(super) fn screen(driver: &Driver) -> Result<Option<capture::Screen>> {
    let Some(output) = driver.fallback()? else {
        return Ok(None);
    };
    let targets = Topology::query(true)?.metadata()?;
    let Some(target) = target(output, &targets) else {
        return Ok(None);
    };
    let Some(mut screen) = capture::screens()?
        .into_iter()
        .find(|s| s.identity.as_deref() == Some(target.identity.as_str()))
    else {
        return Ok(None);
    };
    screen.render_adapter = capture::encoding_adapters()?.first().map(|a| a.luid);
    Ok(Some(screen))
}

/// Caller holds DisplayMutation. `ignore` contains only its verified session
/// targets about to be removed; no other driver's target is excluded this way.
pub(super) fn ensure(
    driver: &Driver,
    width: u32,
    height: u32,
    ignore: &[String],
    verify_frame: bool,
) -> Result<Option<capture::Screen>> {
    let existing = driver.fallback()?;
    let targets = Topology::query(true)?.metadata()?;
    if targets
        .iter()
        .any(|t| replacement(t, existing) && !ignore.contains(&t.identity))
    {
        return screen(driver);
    }
    ensure!(
        driver.persistent_supported(),
        "无可用普通显示器，请更新虚拟显示驱动以启用常驻兜底屏"
    );
    let saved = store::read::<Record>(&path()?)?;
    let record = saved.unwrap_or(Record {
        width: width.clamp(2, 16384),
        height: height.clamp(2, 16384),
        hz: 144,
    });
    validate_modes(&[(record.width, record.height)])?;
    ensure!((1..=144).contains(&record.hz), "无效兜底屏刷新率");
    store::write(&path()?, &record)?; // Intent survives an owner/driver restart.
    if let Some(screen) = screen(driver)? {
        return Ok(Some(screen));
    }
    // Don't change an adapter underneath an existing manual display.
    if existing.is_none()
        && !targets
            .iter()
            .any(|t| t.virtual_provider == Some(VirtualProvider::OpenUuyc))
    {
        let gpu = capture::encoding_adapters()?
            .first()
            .cloned()
            .context("未找到虚拟显示渲染适配器")?;
        driver.render_adapter(gpu.luid)?;
    }
    let output = driver.add_fallback(record.width, record.height, record.hz)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let identity = loop {
        let targets = Topology::query(false)?.metadata()?;
        if let Some(target) = target(output, &targets) {
            break target.identity.clone();
        }
        ensure!(Instant::now() < deadline, "常驻虚拟屏创建后未出现");
        std::thread::sleep(Duration::from_millis(25));
    };
    // Re-read after arrival: keep newly connected physical monitors and do not
    // re-activate an official screen that its owner already reclaimed.
    let mut layout = Topology::query(true)?.snapshot()?;
    if !layout.targets.iter().any(|t| t.identity == identity) {
        let left = layout
            .targets
            .iter()
            .map(|t| i64::from(t.left) + i64::from(t.width))
            .max()
            .unwrap_or(0);
        layout.targets.push(SavedTarget {
            identity: identity.clone(),
            source_group: format!("fallback/{FALLBACK_ID}"),
            left: i32::try_from(left)?,
            top: 0,
            width: record.width,
            height: record.height,
            rotation: 1,
            refresh_numerator: record.hz,
            refresh_denominator: 1,
            scan_line_ordering: 1,
        });
        layout.apply()?;
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    let selected = loop {
        if let Some(screen) = screen(driver)? {
            break screen;
        }
        ensure!(Instant::now() < deadline, "常驻虚拟屏尚未成为可用采集目标");
        std::thread::sleep(Duration::from_millis(25));
    };
    if !verify_frame {
        return Ok(Some(selected));
    }
    // Prove an actual desktop frame; official cleanup is its own autonomous
    // action, so a failure never tears down the new sole target as a rollback.
    let _runtime = crate::features::host::encoder::Runtime::new()?;
    let mut desktop = capture::Desktop::open_selected(&selected)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if desktop.next(20, 2, false, false, (1280, 720))?.is_some() && desktop.available {
            tracing::info!(screen=%identity,backend=desktop.backend_name(), "persistent headless display ready");
            return Ok(Some(selected));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    anyhow::bail!("常驻虚拟屏已建立，但尚未得到桌面画面；保留屏幕等待恢复")
}

pub(super) fn retire(driver: &Driver, replacement_identity: Option<&str>) -> Result<bool> {
    let (output, users) = driver.fallback_state()?;
    if users > u32::from(driver.owns_fallback_pin()) {
        return Ok(false);
    }
    let Some(output) = output else {
        clear_record()?;
        return Ok(true);
    };
    let targets = Topology::query(true)?.metadata()?;
    if !targets.iter().any(|t| {
        replacement(t, Some(output)) && replacement_identity.is_none_or(|id| id == t.identity)
    }) {
        return Ok(false);
    }
    // The driver checks live per-handle pins atomically with removal. A second
    // connection appearing after this snapshot cannot lose its fallback.
    if let Err(error) = driver.remove(FALLBACK_ID) {
        if crate::platform::display::virtual_driver::busy(&error) {
            return Ok(false);
        }
        return Err(error);
    }
    clear_record()?;
    tracing::info!("persistent headless display retired after replacement became ready");
    Ok(true)
}

#[derive(Default)]
pub(crate) struct Maintainer {
    returning: Option<(String, Instant)>,
    protocol_retry: Option<Instant>,
}
impl Maintainer {
    pub(crate) fn tick(&mut self) -> Result<()> {
        if self.protocol_retry.is_some_and(|t| Instant::now() < t) {
            return Ok(());
        }
        if Driver::device_instance().is_err() {
            self.returning = None;
            return Ok(());
        }
        let _serial = display::recovery::Serial::acquire()?;
        let driver = Driver::open()?;
        if !driver.persistent_supported() {
            // The old driver's version query itself renews its global watchdog.
            // Do not poll it frequently and accidentally keep orphaned screens alive.
            self.protocol_retry = Some(Instant::now() + Duration::from_secs(30));
            return Ok(());
        }
        self.protocol_retry = None;
        let (own, users) = driver.fallback_state()?;
        if own.is_none() && !path()?.is_file() {
            self.returning = None;
            return Ok(());
        }
        // An active connection owns the capture handoff, including super-screen
        // restoration. File cleanup releases pins even after a process crash.
        if users != 0 {
            self.returning = None;
            return Ok(());
        }
        let targets = Topology::query(true)?.metadata()?;
        let other = targets.iter().find(|t| replacement(t, own));
        if let Some(other) = other {
            let now = Instant::now();
            let (id, since) = self
                .returning
                .get_or_insert_with(|| (other.identity.clone(), now));
            if id != &other.identity {
                *id = other.identity.clone();
                *since = now;
            }
            if now.duration_since(*since) >= Duration::from_secs(2) {
                retire(&driver, Some(&other.identity))?;
            }
        } else {
            self.returning = None;
            if own.is_some_and(|output| {
                target(output, &targets).is_some_and(|t| t.active && t.available)
            }) {
                // An idle, already active fallback needs neither a capture
                // inventory nor another durable write of the same intent.
                return Ok(());
            }
            if own.is_some() || store::read::<Record>(&path()?)?.is_some() {
                ensure(&driver, 1920, 1080, &[], false)?;
            }
        }
        Ok(())
    }
}
pub(crate) fn start_background() {
    // Installed mode has one account-independent service agent. A portable GUI
    // needs a local maintainer only while that service is unavailable.
    if crate::platform::host_service::install::running().unwrap_or(false) {
        return;
    }
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("display-fallback".into())
            .spawn(|| {
                let mut owner = Maintainer::default();
                let mut last_error = String::new();
                loop {
                    if crate::platform::host_service::install::running().unwrap_or(false) {
                        owner.returning = None;
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                    match owner.tick() {
                        Ok(()) => last_error.clear(),
                        Err(e) => {
                            let e = format!("{e:#}");
                            if e != last_error {
                                tracing::warn!(error=%e,"headless display maintenance deferred");
                                last_error = e;
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            });
    });
}
