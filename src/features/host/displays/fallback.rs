//! Machine-owned headless display. It is deliberately absent from session
//! journals/preferences: closing a connection must not unplug the only screen.
use super::*;
#[cfg(windows)]
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
    // Linux 没有这台机器级服务，vault::sid 会直接报「尚未提供后台被控服务」，
    // 所以这条判断只在 Windows 上做。
    #[cfg(windows)]
    let root = if crate::platform::host_service::vault::sid(std::process::id())? == "S-1-5-18" {
        crate::platform::host_service::vault::root()?.join("displays")
    } else {
        store::root()?
    };
    #[cfg(not(windows))]
    let root = store::root()?;
    Ok(root.join("fallback.json"))
}
fn clear_record() -> Result<()> {
    match std::fs::remove_file(path()?) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
#[cfg(windows)]
pub(super) fn target(output: Output, targets: &[Target]) -> Option<&Target> {
    targets.iter().find(|t| {
        t.adapter == output.adapter
            && t.target == output.target
            && t.adapter_path
                .to_ascii_lowercase()
                .contains("#openuuycdisplay#")
    })
}
#[cfg(windows)]
fn replacement(t: &Target, own: Option<Output>) -> bool {
    t.active
        && t.available
        && t.virtual_provider != Some(VirtualProvider::Uu)
        && own.is_none_or(|o| o.adapter != t.adapter || o.target != t.target)
}
#[cfg(windows)]
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
#[cfg(windows)]
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
#[cfg(windows)]
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

#[cfg(windows)]
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

#[cfg(windows)]
#[derive(Default)]
pub(crate) struct Maintainer {
    returning: Option<(String, Instant)>,
    protocol_retry: Option<Instant>,
}
#[cfg(windows)]
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
#[cfg(windows)]
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

// ---------------------------------------------------------------------------
// Linux：兜底屏是一路 VKMS 连接器加合成器接管，不是驱动里的常驻显示器
// ---------------------------------------------------------------------------

/// Linux 上「机器所有」只能做到进程级：辅助程序把连接器租给这条连接，进程一退它
/// 就收回去（见 `platform/linux/virtual_display.rs`）。断线不退还——连接结束时只是
/// 放弃使用权，屏留着；没有连接时由 [`start_background`] 的后台维护在别的屏回来后
/// 收掉。
#[cfg(not(windows))]
struct Machine {
    session: crate::platform::virtual_display::Session,
    connector: String,
    identity: String,
    /// 正在用兜底屏的连接。回收按进程算，任何一条连接在替代屏出帧后都能回收
    /// （和 Windows 按进程的 pin 一样）；这个集合只告诉后台维护「现在没人连着」。
    owners: std::collections::HashSet<String>,
}

#[cfg(not(windows))]
static MACHINE: std::sync::LazyLock<std::sync::Mutex<Option<Machine>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

/// 建屏、改尺寸、回收依次进行。等合成器认屏、等第一帧都只在这把锁里，所以读兜底屏
/// （`identity`、`screen`、`claim`）的连接不会被这几秒卡住；`MACHINE` 只在读写
/// 那几个字段时短暂持有。
#[cfg(not(windows))]
static CHANGING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 进程正在退出：兜底屏随进程一起被辅助程序收回，这时再建一块只会白白热插拔一次。
#[cfg(not(windows))]
static EXITING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 程序要退出了。之后会话收尾不再为「只剩虚拟屏」去建兜底屏。
///
/// Windows 的兜底屏归驱动、跨进程常驻，退出时建出来的屏正是要留给下一次连接的，
/// 所以那边什么也不做。
pub(crate) fn exiting() {
    #[cfg(not(windows))]
    EXITING.store(true, std::sync::atomic::Ordering::Release);
}

#[cfg(not(windows))]
pub(super) fn is_exiting() -> bool {
    EXITING.load(std::sync::atomic::Ordering::Acquire)
}

/// 兜底屏此刻在不在桌面里。
#[cfg(not(windows))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Presence {
    Present,
    Absent,
    /// 枚举失败，又没法确认它不在：保持现状，不拔、不重建。
    Unknown,
}

/// `listed`：采集枚举里有没有它，枚举失败为 `None`。`active`：RandR 拓扑里它是不是
/// 启用的，查不到为 `None`。
///
/// 一次暂时的枚举失败（XWayland 重启、X 连接抖动）不能当成「屏不在了」：那会把
/// 正在用的兜底屏拔掉再建一块，主控正在看的屏换了标识。只有枚举明确没有它、或者
/// 枚举失败但拓扑明确说它没启用时才算不在。
#[cfg(not(windows))]
fn presence(listed: Option<bool>, active: Option<bool>) -> Presence {
    match (listed, active) {
        (Some(true), _) => Presence::Present,
        // 拓扑里还启用着，只是采集这一下没列出来。
        (Some(false), Some(true)) => Presence::Unknown,
        (Some(false), _) | (None, Some(false)) => Presence::Absent,
        (None, _) => Presence::Unknown,
    }
}

#[cfg(not(windows))]
enum Seen {
    Present(capture::Screen),
    Absent,
    Unknown(anyhow::Error),
}

#[cfg(not(windows))]
fn see(identity: &str) -> Seen {
    let (listed, screen, error) = match capture::screens() {
        Ok(screens) => {
            let screen = screens
                .into_iter()
                .find(|s| s.identity.as_deref() == Some(identity));
            (Some(screen.is_some()), screen, None)
        }
        Err(error) => (None, None, Some(error)),
    };
    let active = if listed == Some(true) {
        None
    } else {
        Topology::query(false)
            .and_then(|topology| topology.metadata())
            .ok()
            .map(|targets| {
                targets
                    .iter()
                    .any(|t| t.identity == identity && t.active && t.available)
            })
    };
    match (presence(listed, active), screen) {
        (Presence::Present, Some(screen)) => Seen::Present(screen),
        (Presence::Absent, _) => Seen::Absent,
        (_, _) => Seen::Unknown(
            error.unwrap_or_else(|| anyhow::anyhow!("兜底虚拟屏还在桌面里，但采集暂时没列出它")),
        ),
    }
}

#[cfg(not(windows))]
fn current_identity() -> Option<String> {
    lock(&MACHINE).as_ref().map(|m| m.identity.clone())
}

/// 等合成器与采集都把新输出报出来。零输出时枚举会失败，等它就行。
#[cfg(not(windows))]
fn wait(identity: &str) -> Result<capture::Screen> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut last = None;
        match capture::screens() {
            Ok(screens) => {
                if let Some(screen) = screens
                    .into_iter()
                    .find(|s| s.identity.as_deref() == Some(identity))
                {
                    return Ok(screen);
                }
            }
            Err(error) => last = Some(format!("{error:#}")),
        }
        ensure!(
            Instant::now() < deadline,
            "{}",
            last.map_or_else(
                || "兜底虚拟屏创建后没有出现在桌面里".to_owned(),
                |error| format!("兜底虚拟屏创建后没有出现在桌面里：{error}")
            )
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(not(windows))]
fn verify_frame(screen: &capture::Screen) -> Result<()> {
    let _runtime = crate::features::host::encoder::Runtime::new()?;
    let mut desktop = capture::Desktop::open_selected(screen)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if desktop.next(20, 2, false, false, (1280, 720))?.is_some() && desktop.available {
            tracing::info!(
                screen = ?screen.identity,
                backend = desktop.backend_name(),
                "persistent headless display ready"
            );
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    anyhow::bail!("兜底虚拟屏已建立，但尚未得到桌面画面；保留屏幕等待恢复")
}

/// 把兜底屏从进程里拿走并还给辅助程序。调用方持有 `CHANGING`。
#[cfg(not(windows))]
fn take_and_remove(reason: &str) -> Result<()> {
    let Some(mut machine) = lock(&MACHINE).take() else {
        return Ok(());
    };
    if let Err(error) = machine.session.remove(&machine.connector) {
        // 放回去，下次再试；丢掉会话等于让辅助程序在断开时收回，结果一样，但
        // 这里先按失败处理，别把一块可能还在用的屏静默拔掉。
        tracing::warn!(%error, "兜底虚拟屏回收失败");
        *lock(&MACHINE) = Some(machine);
        return Err(error);
    }
    clear_record()?;
    tracing::info!(identity = %machine.identity, reason, "persistent headless display retired");
    Ok(())
}

/// 给兜底屏换分辨率。VKMS 的模式表里没有这些尺寸，只能让合成器现造一个模式；
/// 换过的尺寸记下来，下一次按它重建。
#[cfg(not(windows))]
pub(super) fn resize(identity: &str, width: u32, height: u32) -> Result<capture::Screen> {
    validate_modes(&[(width, height)])?;
    let _changing = lock(&CHANGING);
    ensure!(
        current_identity().as_deref() == Some(identity),
        "兜底屏已不在"
    );
    crate::platform::display::compositor::set_mode(
        crate::platform::display::topology::output_name(identity),
        width,
        height,
        Some(144),
    )?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(screens) = capture::screens()
            && let Some(screen) = screens
                .into_iter()
                .find(|s| s.identity.as_deref() == Some(identity))
            && (screen.width, screen.height) == (width, height)
        {
            let mut record = store::read::<Record>(&path()?)?.unwrap_or(Record {
                width,
                height,
                hz: 144,
            });
            record.width = width;
            record.height = height;
            store::write(&path()?, &record)?;
            return Ok(screen);
        }
        ensure!(Instant::now() < deadline, "兜底屏没有切到请求的分辨率");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(not(windows))]
pub(super) fn ensure(
    token: &str,
    width: u32,
    height: u32,
    verify: bool,
) -> Result<Option<capture::Screen>> {
    let changing = lock(&CHANGING);
    let mut owners = std::collections::HashSet::new();
    if let Some(identity) = current_identity() {
        match see(&identity) {
            Seen::Present(screen) => {
                if let Some(current) = lock(&MACHINE).as_mut() {
                    current.owners.insert(token.to_owned());
                }
                drop(changing);
                if verify {
                    verify_frame(&screen)?;
                }
                return Ok(Some(screen));
            }
            // 枚举失败不等于屏不在：报错让调用方稍后再来，别拔掉一块可能正在用的屏。
            Seen::Unknown(error) => {
                return Err(error.context("暂时无法确认兜底虚拟屏是否还在"));
            }
            Seen::Absent => {
                // 辅助程序重启、合成器丢了这块输出都会走到这里：旧屏已经不在桌面里，
                // 重新建一块，正在用它的其他连接跟着搬过去。先显式还一次，免得辅助
                // 程序还没处理完连接断开就多租了一块出去。
                if let Some(mut old) = lock(&MACHINE).take() {
                    let _ = old.session.remove(&old.connector);
                    owners = old.owners;
                }
            }
        }
    }
    // 合成器接不住就别去惊动内核（见 `compositor::ensure_available`）。
    crate::platform::display::compositor::ensure_available()?;
    ensure!(
        openuuyc_vkms::installed(),
        "没有安装虚拟显示器辅助程序（openuuyc-vkms）"
    );
    let record = store::read::<Record>(&path()?)?.unwrap_or(Record {
        width: width.clamp(2, 16384),
        height: height.clamp(2, 16384),
        hz: 144,
    });
    validate_modes(&[(record.width, record.height)])?;
    ensure!((1..=144).contains(&record.hz), "无效兜底屏刷新率");
    let mut session = crate::platform::virtual_display::Session::open()?;
    let connector = session.create(record.width, record.height, record.hz)?;
    let identity = crate::platform::display::topology::identity(&connector);
    let screen = match wait(&identity) {
        Ok(screen) => screen,
        Err(error) => {
            if let Err(rollback) = session.remove(&connector) {
                tracing::error!(%rollback, "兜底虚拟屏回滚失败");
            }
            return Err(error);
        }
    };
    // 屏真的出现了才记尺寸：记录只用来让下一次按同一块尺寸重建，失败的请求不该
    // 留下来影响以后。
    store::write(&path()?, &record)?;
    owners.insert(token.to_owned());
    *lock(&MACHINE) = Some(Machine {
        session,
        connector,
        identity,
        owners,
    });
    drop(changing);
    // 和 Windows 一样：屏已经建出来就留着等恢复，别把唯一一块屏回滚掉。
    if verify {
        verify_frame(&screen)?;
    }
    Ok(Some(screen))
}

/// 兜底屏现在的采集目标。不在时为 `None`；暂时确认不了时报错，调用方下次再看。
#[cfg(not(windows))]
pub(super) fn screen() -> Result<Option<capture::Screen>> {
    let Some(identity) = current_identity() else {
        return Ok(None);
    };
    match see(&identity) {
        Seen::Present(screen) => Ok(Some(screen)),
        Seen::Absent => Ok(None),
        Seen::Unknown(error) => Err(error.context("暂时无法确认兜底虚拟屏是否还在")),
    }
}

/// 兜底屏在桌面里的 identity，`annotate` 靠它把这块屏报成虚拟屏。
#[cfg(not(windows))]
pub(super) fn identity() -> Option<String> {
    current_identity()
}

/// 这个进程有兜底屏就可以回收它：和 Windows 按进程的 pin 计数一样，同一进程里的
/// 其他连接不挡回收（它们各自的维护会在兜底屏消失后换到替代屏）。
#[cfg(not(windows))]
pub(super) fn allows_retirement() -> bool {
    lock(&MACHINE).is_some()
}

#[cfg(not(windows))]
pub(super) fn retire(replacement_identity: &str) -> Result<bool> {
    let _changing = lock(&CHANGING);
    let Some(identity) = current_identity() else {
        return Ok(false);
    };
    match see(&identity) {
        // 确认已经不在了（辅助程序重启过）：没有东西要还，按已回收算，否则这条
        // 记录会一直挡着后续的回收。
        Seen::Absent => {
            if let Some(old) = lock(&MACHINE).take() {
                // 辅助程序那边多半已经收回了；还一次，免得它还占着名额。
                let mut old = old;
                let _ = old.session.remove(&old.connector);
            }
            clear_record()?;
            tracing::info!(%identity, "persistent headless display was already gone");
            return Ok(true);
        }
        // 确认不了就什么都不动。
        Seen::Unknown(error) => {
            tracing::debug!(error = %format!("{error:#}"), "兜底虚拟屏回收推迟：状态未知");
            return Ok(false);
        }
        Seen::Present(_) => {}
    }
    // 只有替代屏真的还在桌面里才回收；调用方已经确认它出过帧。
    let Ok(screens) = capture::screens() else {
        return Ok(false);
    };
    if !screens
        .iter()
        .any(|s| s.identity.as_deref() == Some(replacement_identity))
    {
        return Ok(false);
    }
    take_and_remove("replacement became ready").map(|()| true)
}

/// 一条连接开始使用已经存在的兜底屏时登记使用权。
#[cfg(not(windows))]
pub(super) fn claim(token: &str) {
    if let Some(current) = lock(&MACHINE).as_mut() {
        current.owners.insert(token.to_owned());
    }
}

/// 一条连接结束了：只是放弃它的使用权，屏留着，没人连着时交给后台维护。
#[cfg(not(windows))]
pub(super) fn release(token: &str) {
    if let Some(current) = lock(&MACHINE).as_mut() {
        current.owners.remove(token);
    }
}

/// 后台维护的一步：和 Windows 的 `Maintainer::tick` 同一个规则——没有连接在用兜底屏、
/// 本机有别的启用中的屏持续 2 秒，就把兜底屏收掉，免得本机用户接上显示器后桌面里
/// 还挂着一块看不见的输出。有连接时回收归连接自己的维护（它要先把画面切到替代屏）。
#[cfg(not(windows))]
#[derive(Default)]
struct Maintainer {
    returning: Option<(String, Instant)>,
}

#[cfg(not(windows))]
impl Maintainer {
    fn tick(&mut self, now: Instant) -> Result<()> {
        let idle = lock(&MACHINE)
            .as_ref()
            .map(|m| (m.identity.clone(), m.owners.is_empty()));
        let Some((identity, true)) = idle else {
            self.returning = None;
            return Ok(());
        };
        let targets = Topology::query(true)?.metadata()?;
        let other = targets
            .iter()
            .find(|t| {
                t.active
                    && t.available
                    && t.identity != identity
                    && t.virtual_provider != Some(display::topology::VirtualProvider::Uu)
            })
            .map(|t| t.identity.clone());
        let Some(other) = other else {
            self.returning = None;
            return Ok(());
        };
        if !self.settled(other, now) {
            return Ok(());
        }
        let _changing = lock(&CHANGING);
        // 拿锁的这段时间里可能有连接来了，那就交给它。
        if !lock(&MACHINE)
            .as_ref()
            .is_some_and(|m| m.identity == identity && m.owners.is_empty())
        {
            self.returning = None;
            return Ok(());
        }
        self.returning = None;
        take_and_remove("another display returned while idle")
    }

    /// 同一块替代屏连续在场满 2 秒。
    fn settled(&mut self, other: String, now: Instant) -> bool {
        let (id, since) = self.returning.get_or_insert_with(|| (other.clone(), now));
        if *id != other {
            *id = other;
            *since = now;
        }
        now.duration_since(*since) >= Duration::from_secs(2)
    }
}

#[cfg(not(windows))]
pub(crate) fn start_background() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("display-fallback".into())
            .spawn(|| {
                let mut owner = Maintainer::default();
                let mut last_error = String::new();
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    if is_exiting() {
                        continue;
                    }
                    match owner.tick(Instant::now()) {
                        Ok(()) => last_error.clear(),
                        Err(e) => {
                            let e = format!("{e:#}");
                            if e != last_error {
                                tracing::warn!(error=%e,"headless display maintenance deferred");
                                last_error = e;
                            }
                        }
                    }
                }
            });
    });
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn a_failed_enumeration_is_not_a_missing_screen() {
        assert_eq!(presence(Some(true), None), Presence::Present);
        assert_eq!(presence(Some(false), Some(false)), Presence::Absent);
        assert_eq!(presence(Some(false), None), Presence::Absent);
        assert_eq!(presence(None, Some(false)), Presence::Absent);
        // 枚举失败、拓扑也查不到，或者拓扑说它还启用着：都不能拔。
        assert_eq!(presence(None, None), Presence::Unknown);
        assert_eq!(presence(None, Some(true)), Presence::Unknown);
        assert_eq!(presence(Some(false), Some(true)), Presence::Unknown);
    }

    #[test]
    fn idle_retirement_waits_for_the_same_screen_for_two_seconds() {
        let mut maintainer = Maintainer::default();
        let start = Instant::now();
        assert!(!maintainer.settled("a".into(), start));
        assert!(!maintainer.settled("a".into(), start + Duration::from_millis(1500)));
        // 换了一块屏就重新计时。
        assert!(!maintainer.settled("b".into(), start + Duration::from_millis(1900)));
        assert!(!maintainer.settled("b".into(), start + Duration::from_millis(3800)));
        assert!(maintainer.settled("b".into(), start + Duration::from_millis(3900)));
    }
}
