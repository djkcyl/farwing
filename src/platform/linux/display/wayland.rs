//! The compositor's own output layout.
//!
//! It is not the X11 one, and that matters because it is what an absolute
//! pointer position is resolved against: a compositor reads the pointer
//! device's absolute axes as a fraction of this layout, so a host that aims
//! with X11 rectangles hits a different point than the one it asked for.
//!
//! XWayland cannot be asked instead. It sizes every output in that output's own
//! pixels while it places it at the compositor's logical position, so a
//! fractional scale makes the two disagree, and `wl_output.scale` is no help:
//! the protocol rounds it to a whole number (2 for both outputs here, where the
//! scales are 1.6 and 1.35). `xdg_output` is the only place the logical
//! position and size are stated.
//!
//! The input path asks for the layout on every batch of events, often while it
//! holds the input gate, so it must never wait on the compositor. A watcher
//! thread keeps one connection open, follows output hotplug and layout changes
//! as the compositor announces them, and publishes a snapshot; readers only
//! clone the latest one. A compositor that cannot describe its layout leaves
//! the snapshot empty and is asked again later, with a growing pause.
use crate::platform::{CompositorLayout, CompositorOutput, Rotation};
use anyhow::{Context, Result, bail};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};
use wayland_client::protocol::{wl_callback, wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};

/// `wl_output.name` arrived at 4; older revisions are matched by the name their
/// `xdg_output` reports instead.
const OUTPUT_VERSION: u32 = 4;
/// How long the first description of every output may take. A compositor that
/// has not answered by then is treated as one that cannot describe its layout.
const SETUP_TIMEOUT: Duration = Duration::from_secs(3);
/// The pause before asking again after a failure, doubled up to the maximum.
const RETRY_FIRST: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(60);
/// How often [`layout_soon`] looks while it waits.
const WAIT_POLL: Duration = Duration::from_millis(100);

static SNAPSHOT: RwLock<Option<Arc<CompositorLayout>>> = RwLock::new(None);
/// Set when the watcher's latest attempt failed, cleared when it next publishes
/// a layout: [`layout_soon`] does not wait on a compositor that has already
/// said it cannot describe one.
static UNAVAILABLE: AtomicBool = AtomicBool::new(false);

/// The latest layout the compositor described, without waiting for it.
///
/// `None` outside a Wayland session, before the first description arrives,
/// and while the compositor cannot describe its layout.
pub(crate) fn layout() -> Option<Arc<CompositorLayout>> {
    if !watching() {
        return None;
    }
    SNAPSHOT.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// [`layout`], waiting up to [`SETUP_TIMEOUT`] for the first description.
///
/// A caller that cannot go on without the layout, and is not in anyone's hot
/// path, asks this way; the wait is bounded so a compositor that never answers
/// costs one pause rather than hanging. Once the watcher has failed, later
/// calls return at once until it describes the layout again, so that pause is
/// paid by the process, not by every call. Without a Wayland session there is
/// nothing to wait for at all.
pub(crate) fn layout_soon() -> Option<Arc<CompositorLayout>> {
    if !watching() {
        return None;
    }
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        if let Some(layout) = layout() {
            return Some(layout);
        }
        if UNAVAILABLE.load(Ordering::Acquire) {
            return None;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        std::thread::sleep(WAIT_POLL.min(left));
    }
}

/// Whether the watcher thread is running, starting it on the first ask.
fn watching() -> bool {
    static WATCHER: OnceLock<bool> = OnceLock::new();
    *WATCHER.get_or_init(|| {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return false;
        }
        std::thread::Builder::new()
            .name("compositor-layout".into())
            .spawn(watch)
            .inspect_err(|error| tracing::warn!(%error, "无法启动合成器布局监视线程"))
            .is_ok()
    })
}

fn publish(layout: Option<CompositorLayout>) {
    let mut snapshot = SNAPSHOT.write().unwrap_or_else(|e| e.into_inner());
    if layout.is_some() {
        UNAVAILABLE.store(false, Ordering::Release);
    }
    if snapshot.as_deref() != layout.as_ref() {
        tracing::info!(?layout, "合成器输出布局");
        *snapshot = layout.map(Arc::new);
    }
}

fn watch() {
    let mut pause = RETRY_FIRST;
    let mut reported = None::<String>;
    loop {
        let mut described = false;
        let error = match follow(&mut described) {
            Err(error) => error,
            Ok(never) => match never {},
        };
        publish(None);
        UNAVAILABLE.store(true, Ordering::Release);
        if described {
            pause = RETRY_FIRST;
        }
        // The same failure every minute is one message, not a log full of it.
        let message = format!("{error:#}");
        if reported.as_ref() != Some(&message) {
            tracing::warn!(error = %message, "合成器输出布局不可用：绝对定位改用相对移动，缩放的桌面上门户采集无法认屏");
            reported = Some(message);
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(RETRY_MAX);
    }
}

/// Describe the layout and keep describing it until the connection ends.
/// Returns only on failure; `described` tells whether anything was published.
fn follow(described: &mut bool) -> Result<std::convert::Infallible> {
    let connection =
        Connection::connect_to_env().context("没有可用的 Wayland 会话（WAYLAND_DISPLAY）")?;
    let mut queue = connection.new_event_queue::<State>();
    let handle = queue.handle();
    connection.display().get_registry(&handle, ());
    let mut state = State::default();
    sync(&connection, &mut queue, &mut state, "读取合成器全局对象")?;
    if state.manager.is_none() {
        bail!("合成器没有提供 zxdg_output_manager_v1，无法得知输出的逻辑位置");
    }
    // The outputs announced before the manager were asked for as soon as it
    // was bound; the descriptions arrive one round trip later.
    sync(&connection, &mut queue, &mut state, "读取输出描述")?;
    publish(Some(state.layout()));
    *described = true;
    state.changed = false;
    loop {
        dispatch(&connection, &mut queue, &mut state, None)?;
        if std::mem::take(&mut state.changed) {
            publish(Some(state.layout()));
        }
    }
}

/// One round trip, bounded by [`SETUP_TIMEOUT`].
fn sync(
    connection: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    what: &str,
) -> Result<()> {
    state.synced = false;
    connection.display().sync(&queue.handle(), ());
    let deadline = Instant::now() + SETUP_TIMEOUT;
    while !state.synced {
        dispatch(connection, queue, state, Some(deadline))
            .with_context(|| format!("{what}失败"))?;
    }
    Ok(())
}

/// Dispatch whatever has arrived, waiting for more until `deadline`.
fn dispatch(
    connection: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    deadline: Option<Instant>,
) -> Result<()> {
    queue.dispatch_pending(state)?;
    connection.flush()?;
    let Some(guard) = queue.prepare_read() else {
        // Events were queued meanwhile; the next call dispatches them.
        return Ok(());
    };
    let timeout = match deadline {
        Some(deadline) => {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("合成器没有在 {} 秒内应答", SETUP_TIMEOUT.as_secs());
            }
            i32::try_from(left.as_millis()).unwrap_or(i32::MAX).max(1)
        }
        None => -1,
    };
    let mut poll = libc::pollfd {
        fd: guard.connection_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut poll, 1, timeout) };
    if ready < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            return Ok(());
        }
        return Err(error).context("等待合成器消息失败");
    }
    if ready > 0 {
        match guard.read() {
            Ok(_) => {}
            Err(wayland_client::backend::WaylandError::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error).context("读取合成器消息失败"),
        }
    }
    queue.dispatch_pending(state)?;
    Ok(())
}

#[derive(Default)]
struct State {
    manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    entries: Vec<Entry>,
    synced: bool,
    /// Set when the compositor finished describing a change.
    changed: bool,
}

struct Entry {
    /// The registry name, which is how the compositor removes it again.
    global: u32,
    output: wl_output::WlOutput,
    description: Option<zxdg_output_v1::ZxdgOutputV1>,
    name: Option<String>,
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
    /// How far the compositor turned this panel; only the touch path reads it.
    rotation: Rotation,
}

impl State {
    /// Every described output, named or not: an unnamed one cannot be paired
    /// with a screen, but it still widens the area an absolute pointer covers.
    fn layout(&self) -> CompositorLayout {
        CompositorLayout {
            outputs: self
                .entries
                .iter()
                .filter_map(|entry| {
                    let (left, top) = entry.position?;
                    let (width, height) = entry.size?;
                    (width > 0 && height > 0).then(|| CompositorOutput {
                        name: entry.name.clone(),
                        left,
                        top,
                        width: width as u32,
                        height: height as u32,
                        rotation: entry.rotation,
                    })
                })
                .collect(),
        }
    }

    fn describe(&mut self, global: u32, handle: &QueueHandle<Self>) {
        let Some(manager) = &self.manager else {
            return;
        };
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.global == global && entry.description.is_none())
        {
            entry.description = Some(manager.get_xdg_output(&entry.output, handle, global));
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "zxdg_output_manager_v1" if state.manager.is_none() => {
                    state.manager = Some(registry.bind(name, version.min(3), handle, ()));
                    let globals: Vec<u32> = state.entries.iter().map(|e| e.global).collect();
                    for global in globals {
                        state.describe(global, handle);
                    }
                }
                "wl_output" => {
                    state.entries.push(Entry {
                        global: name,
                        output: registry.bind(name, version.min(OUTPUT_VERSION), handle, name),
                        description: None,
                        name: None,
                        position: None,
                        size: None,
                        rotation: Rotation::default(),
                    });
                    state.describe(name, handle);
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(index) = state.entries.iter().position(|e| e.global == name) {
                    let entry = state.entries.remove(index);
                    if let Some(description) = entry.description {
                        description.destroy();
                    }
                    if entry.output.version() >= 3 {
                        entry.output.release();
                    }
                    state.changed = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.entries.iter_mut().find(|e| e.global == *global) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => entry.name = Some(name),
            wl_output::Event::Geometry { transform, .. } => {
                entry.rotation = rotation(
                    transform
                        .into_result()
                        .unwrap_or(wl_output::Transform::Normal),
                );
            }
            // From xdg_output 3 on, a finished description ends here.
            wl_output::Event::Done => state.changed = true,
            _ => {}
        }
    }
}

/// `wl_output.transform` names the same turns this side does, so the mapping is
/// only the error case turning into "not rotated".
fn rotation(transform: wl_output::Transform) -> Rotation {
    match transform {
        wl_output::Transform::Normal => Rotation::Normal,
        wl_output::Transform::_90 => Rotation::Rotate90,
        wl_output::Transform::_180 => Rotation::Rotate180,
        wl_output::Transform::_270 => Rotation::Rotate270,
        wl_output::Transform::Flipped => Rotation::Flipped,
        wl_output::Transform::Flipped90 => Rotation::Flipped90,
        wl_output::Transform::Flipped180 => Rotation::Flipped180,
        wl_output::Transform::Flipped270 => Rotation::Flipped270,
        _ => Rotation::Normal,
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.entries.iter_mut().find(|e| e.global == *global) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => entry.position = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                entry.size = Some((width, height))
            }
            // Before xdg_output 3 a finished description ends here instead.
            zxdg_output_v1::Event::Done => state.changed = true,
            zxdg_output_v1::Event::Name { name } => {
                // wl_output's own name, when it has one, is the connector.
                entry.name.get_or_insert(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.synced = true;
        }
    }
}

wayland_client::delegate_noop!(State: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);
