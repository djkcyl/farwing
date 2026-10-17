//! User-desktop XDND drag source, the X11 counterpart of the Windows OLE one.
//! Transport and file ownership stay with callers: the files arrive as local
//! paths (a mount of the remote's files) and leave as `text/uri-list`.
//!
//! XDND is driven by the source alone, so unlike OLE nothing has to hold a
//! mouse button: a drag placed from the peer moves the pointer and talks to
//! whatever window lies under it, and a drag the local user carries out of the
//! viewer follows the real pointer until its button comes up.
pub(crate) mod appearance;
pub(crate) mod linked;
pub(crate) mod original;
pub(crate) mod portal;
pub(crate) mod send_target;
pub(crate) mod wayland;
mod xdnd;

pub(crate) use wayland::Catcher;
// 剪贴板那边读 X11 选择区时也要解析 `text/uri-list`，一模一样的一份不必再写一遍。
pub(crate) use xdnd::parse_uri_list;

use anyhow::{Context as _, Result, bail, ensure};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};
use x11rb::connection::Connection;
use x11rb::protocol::Event as XEvent;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, KeyButMask, PropMode,
    SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// What a drag offers: local paths, and whatever keeps them readable (the
/// mount of a remote offer) for as long as the drag needs them.
pub(crate) struct Files {
    pub paths: Vec<PathBuf>,
    pub _hold: Arc<dyn std::any::Any + Send + Sync>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Position {
    pub x: i32,
    pub y: i32,
}
/// Whether this side can hand a file drag back to its original gesture
/// when it re-enters the viewer (`NATIVE_RETURN`). The Windows side does it
/// through OLE; XDND has no counterpart here yet, so it is not offered and
/// both ends treat a drag out as final.
pub(crate) const RETURN_CAPABLE: bool = false;

/// Whether the left mouse button is down on the local display.
pub(crate) fn left_held() -> bool {
    (|| -> Result<bool> {
        let (connection, screen) = x11rb::connect(None)?;
        let root = connection.setup().roots[screen].root;
        let pointer = connection.query_pointer(root)?.reply()?;
        Ok(pointer.mask.contains(KeyButMask::BUTTON1))
    })()
    .unwrap_or(false)
}

/// Only used once a returned drag is restored, which `RETURN_CAPABLE`
/// rules out here.
pub(crate) fn wake_viewer(_owner: u64) {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Dragging,
    Drop,
    Cancel,
}
struct Desired {
    point: Position,
    phase: Phase,
}
#[derive(Debug)]
pub(crate) enum Event {
    Feedback(u32),
    Released,
    Finished {
        accepted: bool,
        effect: u32,
        error: Option<String>,
    },
}
/// The only effect offered: the receiving side copies the files.
const COPY: u32 = 1;
const TICK: Duration = Duration::from_millis(8);
/// How long a target may take to answer a position before it counts as
/// refusing, and to confirm a drop it took.
const STATUS_WAIT: Duration = Duration::from_secs(2);
/// Many targets refuse the first position while they fetch the offer to look
/// at it, and accept the next one; a refusal only counts once it has held
/// this long, with the position asked again meanwhile.
const REFUSE_GRACE: Duration = Duration::from_millis(700);
const ASK_AGAIN: Duration = Duration::from_millis(50);
/// Positions are repeated now and then even when accepted, as a moving
/// pointer would, so a target that changes its mind is heard.
const KEEPALIVE: Duration = Duration::from_millis(500);
const FINISH_WAIT: Duration = Duration::from_secs(30);

struct Shared {
    desired: Mutex<Desired>,
    physical: bool,
    allowed: Arc<dyn Fn() -> bool + Send + Sync>,
    notify: Arc<dyn Fn(Event) + Send + Sync>,
    finished: AtomicBool,
    feedback: AtomicU32,
}
impl Shared {
    fn allowed(&self) -> bool {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.allowed)())).unwrap_or(false)
    }
    fn emit(&self, event: Event) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.notify)(event)));
    }
    fn feedback(&self, effect: u32) {
        if self.feedback.swap(effect, Ordering::AcqRel) != effect {
            self.emit(Event::Feedback(effect));
        }
    }
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) struct Session {
    /// X11 的 XDND 那条：指针走哪、什么时候落下都由我们驱动。
    xdnd: Option<Arc<Shared>>,
    /// Wayland 那条：合成器自己接管指针，我们只握着那个数据源等它报收官，
    /// 所以 `position`/`commit` 在这里没有对应物。
    wayland: Option<(Arc<linked::Linked>, u64)>,
}
impl Session {
    /// A drag placed by the peer: the pointer goes where `position` says and
    /// the drop happens at `commit`.
    pub fn start(
        point: Position,
        allowed: Arc<dyn Fn() -> bool + Send + Sync>,
        object: impl FnOnce() -> Result<Files> + Send + 'static,
        notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self> {
        Self::start_mode(point, false, allowed, object, notify)
    }
    /// A drag the local user is holding: it follows the real pointer and drops
    /// where the left button is released.
    ///
    /// Wayland 上这件事压根不该由我们做：合成器只认 `start_drag` 那一次交接，
    /// 之后指针归它管，落点也是它投递给目标窗口的。XDND 那套得自己假装按下、
    /// 自己跟着指针走，而 Wayland 会话里 `xdnd::connect()` 直接拒绝——那条路
    /// 只有在 X11 会话（或者拖到 XWayland 窗口上）才成立。
    pub fn physical(
        allowed: Arc<dyn Fn() -> bool + Send + Sync>,
        object: impl FnOnce() -> Result<Files> + Send + 'static,
        notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self> {
        if let Some(linked) = linked::Linked::shared() {
            // `object()` 可能要去挂载、去下载，一步都不能压在事件循环上：
            // 交给准备线程，UI 线程只负责在文件到手之后把拖动发出去。
            let (send, receive) = std::sync::mpsc::channel();
            std::thread::Builder::new()
                .name("drag files".into())
                .spawn(move || {
                    let _ = send.send(object());
                })
                .context("无法启动拖动准备线程")?;
            let id = linked.request_drag(receive, allowed, notify);
            return Ok(Self {
                xdnd: None,
                wayland: Some((linked, id)),
            });
        }
        let (connection, root, _) = xdnd::connect()?;
        let pointer = connection.query_pointer(root)?.reply()?;
        ensure!(
            pointer.mask.contains(KeyButMask::BUTTON1),
            "鼠标已松开，拖出已取消"
        );
        Self::start_mode(
            Position {
                x: pointer.root_x.into(),
                y: pointer.root_y.into(),
            },
            true,
            allowed,
            object,
            notify,
        )
    }
    fn start_mode(
        point: Position,
        physical: bool,
        allowed: Arc<dyn Fn() -> bool + Send + Sync>,
        object: impl FnOnce() -> Result<Files> + Send + 'static,
        notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self> {
        let shared = Arc::new(Shared {
            desired: Mutex::new(Desired {
                point,
                phase: Phase::Dragging,
            }),
            physical,
            allowed,
            notify,
            finished: AtomicBool::new(false),
            feedback: AtomicU32::new(u32::MAX),
        });
        ensure!(shared.allowed(), "拖放未获许可");
        let running = shared.clone();
        std::thread::Builder::new()
            .name("XDND remote drag".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(&running, object)
                }));
                running.finished.store(true, Ordering::Release);
                running.emit(match result {
                    Ok(Ok(accepted)) => Event::Finished {
                        accepted,
                        effect: if accepted { COPY } else { 0 },
                        error: None,
                    },
                    Ok(Err(error)) => Event::Finished {
                        accepted: false,
                        effect: 0,
                        error: Some(error.to_string()),
                    },
                    Err(_) => Event::Finished {
                        accepted: false,
                        effect: 0,
                        error: Some("拖放执行线程已停止".into()),
                    },
                });
            })?;
        Ok(Self {
            xdnd: Some(shared),
            wayland: None,
        })
    }
    pub fn position(&self, point: Position) {
        // Wayland 上指针归合成器，这里没有可推的位置。
        let Some(shared) = &self.xdnd else {
            return;
        };
        let mut desired = lock(&shared.desired);
        if desired.phase == Phase::Dragging {
            desired.point = point;
        }
    }
    pub fn commit(&self, point: Position) -> Result<()> {
        // 同理：落点由目标窗口自己决定，我们连等都不用等。
        let Some(shared) = &self.xdnd else {
            return Ok(());
        };
        let mut desired = lock(&shared.desired);
        ensure!(
            desired.phase == Phase::Dragging && !shared.finished.load(Ordering::Acquire),
            "拖放已结束"
        );
        desired.point = point;
        desired.phase = Phase::Drop;
        Ok(())
    }
    pub fn cancel(&self) {
        if let Some(shared) = &self.xdnd {
            lock(&shared.desired).phase = Phase::Cancel;
        }
        if let Some((linked, id)) = &self.wayland {
            linked.abort_drag(*id);
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// One drag owns the desktop at a time; wait only for a retiring one.
static DESKTOP: Mutex<()> = Mutex::new(());
/// Drags this process is running, which the send target does not take for
/// the local user's.
static OWN_DRAGS: AtomicU32 = AtomicU32::new(0);
pub(super) fn own_drag() -> bool {
    OWN_DRAGS.load(Ordering::Acquire) != 0
}
struct OwnDrag;
impl OwnDrag {
    fn new() -> Self {
        OWN_DRAGS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}
impl Drop for OwnDrag {
    fn drop(&mut self) {
        OWN_DRAGS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The window a drag speaks for, destroyed (which also gives up the
/// selection) when the drag ends.
struct SourceWindow<'a> {
    connection: &'a RustConnection,
    window: Window,
}
impl Drop for SourceWindow<'_> {
    fn drop(&mut self) {
        let _ = self.connection.destroy_window(self.window);
        let _ = self.connection.flush();
    }
}

struct Source<'a> {
    connection: &'a RustConnection,
    atoms: xdnd::Atoms,
    root: Window,
    window: Window,
    time: u32,
    uris: String,
    plain: String,
    current: Option<xdnd::Target>,
    accepted: bool,
    /// When the position now awaiting a status was sent.
    awaiting: Option<Instant>,
    sent: Option<Position>,
    sent_at: Option<Instant>,
    /// Since when the current target has kept refusing.
    refusing: Option<Instant>,
    finished: Option<bool>,
}
impl Source<'_> {
    fn types(&self) -> [u32; 3] {
        [
            self.atoms.URI_LIST,
            self.atoms.PLAIN_UTF8,
            self.atoms.UTF8_STRING,
        ]
    }
    fn send(&self, target: xdnd::Target, kind: u32, data: [u32; 5]) -> Result<()> {
        xdnd::send(self.connection, target.proxy, target.window, kind, data)
    }
    fn leave(&mut self) -> Result<()> {
        if let Some(target) = self.current.take() {
            self.send(target, self.atoms.XdndLeave, [self.window, 0, 0, 0, 0])?;
        }
        self.accepted = false;
        self.awaiting = None;
        self.sent = None;
        self.sent_at = None;
        self.refusing = None;
        Ok(())
    }
    /// Point the drag at whatever takes drops under `point`.
    fn retarget(&mut self, point: Position) -> Result<()> {
        let target = xdnd::target_at(
            self.connection,
            &self.atoms,
            self.root,
            point.x,
            point.y,
            self.window,
        )?;
        if target.map(|t| t.window) == self.current.map(|t| t.window) {
            return Ok(());
        }
        self.leave()?;
        if let Some(target) = target {
            let [a, b, c] = self.types();
            self.send(
                target,
                self.atoms.XdndEnter,
                [self.window, target.version << 24, a, b, c],
            )?;
            self.current = Some(target);
        }
        Ok(())
    }
    fn position(&mut self, point: Position) -> Result<()> {
        let Some(target) = self.current else {
            return Ok(());
        };
        if self.awaiting.is_some() {
            return Ok(());
        }
        let again = if self.accepted { KEEPALIVE } else { ASK_AGAIN };
        if self.sent == Some(point) && self.sent_at.is_some_and(|at| at.elapsed() < again) {
            return Ok(());
        }
        let x = point.x.clamp(0, 0x7fff) as u32;
        let y = point.y.clamp(0, 0x7fff) as u32;
        self.send(
            target,
            self.atoms.XdndPosition,
            [
                self.window,
                0,
                (x << 16) | y,
                self.time,
                self.atoms.XdndActionCopy,
            ],
        )?;
        self.sent = Some(point);
        self.sent_at = Some(Instant::now());
        self.awaiting = Some(Instant::now());
        Ok(())
    }
    /// Whether the target has answered the last position sent, giving up on
    /// one that does not answer.
    fn settled(&mut self) -> bool {
        if self
            .awaiting
            .is_some_and(|since| since.elapsed() > STATUS_WAIT)
        {
            self.awaiting = None;
            self.accepted = false;
            self.refusing.get_or_insert_with(Instant::now);
        }
        self.awaiting.is_none()
    }
    /// The effect to report, once it is settled: a copy as soon as the target
    /// takes it, a refusal only after it has held.
    fn effect(&self) -> Option<u32> {
        if self.current.is_none() {
            Some(0)
        } else if self.accepted {
            Some(COPY)
        } else if self
            .refusing
            .is_some_and(|since| since.elapsed() >= REFUSE_GRACE)
        {
            Some(0)
        } else {
            None
        }
    }
    /// Whether the target takes `action`. Only a copy is offered; a target
    /// that answers with a move or a private action still just reads the
    /// files, which sit on a read-only mount, but a link or a question to
    /// the user is not a copy.
    fn takes(&self, action: u32) -> bool {
        action != self.atoms.XdndActionLink && action != self.atoms.XdndActionAsk
    }
    fn events(&mut self) -> Result<()> {
        while let Some(event) = self.connection.poll_for_event()? {
            match event {
                XEvent::ClientMessage(message) => {
                    let data = message.data.as_data32();
                    let from_current = self.current.is_some_and(|t| t.window == data[0]);
                    if message.type_ == self.atoms.XdndStatus && from_current {
                        self.awaiting = None;
                        // A version 2+ target names the action it will take.
                        self.accepted = data[1] & 1 != 0 && self.takes(data[4]);
                        if self.accepted {
                            self.refusing = None;
                        } else {
                            self.refusing.get_or_insert_with(Instant::now);
                        }
                    } else if message.type_ == self.atoms.XdndFinished && from_current {
                        let version = self.current.map_or(0, |t| t.version);
                        self.finished =
                            Some(version < 5 || (data[1] & 1 != 0 && self.takes(data[2])));
                    }
                }
                XEvent::SelectionRequest(request)
                    if request.selection == self.atoms.XdndSelection =>
                {
                    self.answer(&request)?;
                }
                _ => {}
            }
        }
        Ok(())
    }
    fn answer(&self, request: &SelectionRequestEvent) -> Result<()> {
        let property = if request.property == x11rb::NONE {
            request.target
        } else {
            request.property
        };
        let atoms = &self.atoms;
        let served = if request.target == atoms.TARGETS {
            let mut targets = vec![atoms.TARGETS];
            targets.extend(self.types());
            self.connection.change_property32(
                PropMode::REPLACE,
                request.requestor,
                property,
                AtomEnum::ATOM,
                &targets,
            )?;
            true
        } else if request.target == atoms.URI_LIST {
            self.connection.change_property8(
                PropMode::REPLACE,
                request.requestor,
                property,
                request.target,
                self.uris.as_bytes(),
            )?;
            true
        } else if request.target == atoms.PLAIN_UTF8 || request.target == atoms.UTF8_STRING {
            self.connection.change_property8(
                PropMode::REPLACE,
                request.requestor,
                property,
                request.target,
                self.plain.as_bytes(),
            )?;
            true
        } else {
            false
        };
        self.connection.send_event(
            false,
            request.requestor,
            EventMask::NO_EVENT,
            SelectionNotifyEvent {
                response_type: SELECTION_NOTIFY_EVENT,
                sequence: 0,
                time: request.time,
                requestor: request.requestor,
                selection: request.selection,
                target: request.target,
                property: if served { property } else { x11rb::NONE },
            },
        )?;
        Ok(())
    }
}

/// The keycode the keyboard map gives Escape, for cancelling a held drag.
fn escape_keycode(connection: &RustConnection) -> Option<u8> {
    const ESCAPE: u32 = 0xff1b;
    let setup = connection.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let map = connection
        .get_keyboard_mapping(min, max - min + 1)
        .ok()?
        .reply()
        .ok()?;
    let per = usize::from(map.keysyms_per_keycode).max(1);
    map.keysyms
        .chunks(per)
        .position(|syms| syms.contains(&ESCAPE))
        .and_then(|index| u8::try_from(usize::from(min) + index).ok())
}

/// Run one drag to its end: whether the target took the files.
fn run(shared: &Shared, object: impl FnOnce() -> Result<Files>) -> Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let _desktop = loop {
        ensure!(
            shared.allowed() && lock(&shared.desired).phase != Phase::Cancel,
            "拖放已取消"
        );
        match DESKTOP.try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                ensure!(Instant::now() < deadline, "桌面正在处理另一个拖放");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    let _own = OwnDrag::new();
    let (connection, root, atoms) = xdnd::connect()?;
    let files = object()?;
    ensure!(shared.allowed(), "拖放许可已撤销");
    ensure!(!files.paths.is_empty(), "拖放文件为空");
    let uris = xdnd::uri_list(&files.paths);
    let plain = files
        .paths
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n");
    ensure!(uris.len() < 256 * 1024, "拖放文件路径过长");
    if !shared.physical {
        connection
            .xtest_get_version(2, 2)?
            .reply()
            .context("X 服务器不支持 XTEST，无法移动拖放指针")?;
    }
    let window = connection.generate_id()?;
    connection.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        -10,
        -10,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        x11rb::COPY_FROM_PARENT,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    let _window = SourceWindow {
        connection: &connection,
        window,
    };
    let time = xdnd::timestamp(&connection, &atoms, window)?;
    connection.set_selection_owner(window, atoms.XdndSelection, time)?;
    ensure!(
        connection
            .get_selection_owner(atoms.XdndSelection)?
            .reply()?
            .owner
            == window,
        "无法取得拖放数据所有权"
    );
    let escape = shared
        .physical
        .then(|| escape_keycode(&connection))
        .flatten();
    let mut source = Source {
        connection: &connection,
        atoms,
        root,
        window,
        time,
        uris,
        plain,
        current: None,
        accepted: false,
        awaiting: None,
        sent: None,
        sent_at: None,
        refusing: None,
        finished: None,
    };
    let mut moved: Option<Position> = None;
    let mut looked: Option<(Position, Instant)> = None;
    let mut drop_since: Option<Instant> = None;
    loop {
        source.events()?;
        let (point, phase) = if shared.physical {
            let pointer = connection.query_pointer(root)?.reply()?;
            let point = Position {
                x: pointer.root_x.into(),
                y: pointer.root_y.into(),
            };
            let escaped = escape.is_some_and(|code| {
                connection
                    .query_keymap()
                    .ok()
                    .and_then(|cookie| cookie.reply().ok())
                    .is_some_and(|keys| keys.keys[usize::from(code / 8)] & (1 << (code % 8)) != 0)
            });
            let mut desired = lock(&shared.desired);
            if pointer.mask.contains(KeyButMask::BUTTON3) || escaped {
                desired.phase = Phase::Cancel;
            } else if desired.phase == Phase::Dragging
                && !pointer.mask.contains(KeyButMask::BUTTON1)
            {
                desired.phase = Phase::Drop;
            }
            if desired.phase == Phase::Dragging {
                desired.point = point;
            }
            (desired.point, desired.phase)
        } else {
            let desired = lock(&shared.desired);
            (desired.point, desired.phase)
        };
        if phase == Phase::Cancel || !shared.allowed() {
            source.leave()?;
            connection.flush()?;
            return Ok(false);
        }
        if !shared.physical && moved != Some(point) {
            connection.xtest_fake_input(
                x11rb::protocol::xproto::MOTION_NOTIFY_EVENT,
                0,
                x11rb::CURRENT_TIME,
                root,
                point.x.clamp(i16::MIN.into(), i16::MAX.into()) as i16,
                point.y.clamp(i16::MIN.into(), i16::MAX.into()) as i16,
                0,
            )?;
            moved = Some(point);
        }
        // The window under a still pointer can change too (one closing, one
        // opening), so it is looked up again now and then, not only on moves.
        if looked
            .is_none_or(|(at, when)| at != point || when.elapsed() > Duration::from_millis(100))
        {
            source.retarget(point)?;
            looked = Some((point, Instant::now()));
        }
        source.position(point)?;
        let settled = source.settled();
        let effect = source.effect().filter(|_| settled);
        if let Some(effect) = effect {
            shared.feedback(effect);
        }
        connection.flush()?;
        if phase == Phase::Drop {
            // Drop only on an answer about the point it lands on.
            let since = *drop_since.get_or_insert_with(Instant::now);
            let decided = settled && source.sent == Some(point) && effect.is_some();
            if source.current.is_some() && !decided && since.elapsed() < STATUS_WAIT {
                std::thread::sleep(TICK);
                continue;
            }
            let Some(target) = source.current.filter(|_| source.accepted && settled) else {
                source.leave()?;
                connection.flush()?;
                return Ok(false);
            };
            source.send(target, atoms.XdndDrop, [window, 0, time, 0, 0])?;
            connection.flush()?;
            shared.emit(Event::Released);
            let deadline = Instant::now() + FINISH_WAIT;
            while source.finished.is_none() {
                if !shared.allowed() {
                    bail!("拖放许可已撤销");
                }
                ensure!(Instant::now() < deadline, "目标未确认拖放");
                source.events()?;
                connection.flush()?;
                std::thread::sleep(TICK);
            }
            // The target confirmed; the files themselves stay readable for
            // as long as the caller keeps the offer.
            drop(files);
            return Ok(source.finished == Some(true));
        }
        std::thread::sleep(TICK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn describe(event: Event) -> String {
        match event {
            Event::Feedback(effect) => format!("feedback {effect}"),
            Event::Released => "released".into(),
            Event::Finished {
                accepted, error, ..
            } => format!("finished {accepted} {error:?}"),
        }
    }

    fn files() -> Files {
        let path = std::env::temp_dir().join("openuuyc-dnd-test.txt");
        std::fs::write(&path, b"hello from openuuyc").unwrap();
        Files {
            paths: vec![path],
            _hold: Arc::new(()),
        }
    }

    /// Run with DISPLAY pointing at a test server whose window at
    /// OPENUUYC_DND_TARGET (`x,y`) takes file drops.
    #[test]
    #[ignore = "needs an X display with a drop target"]
    fn drops_into_target() {
        let target = std::env::var("OPENUUYC_DND_TARGET").unwrap();
        let (x, y) = target.split_once(',').unwrap();
        let point = Position {
            x: x.parse().unwrap(),
            y: y.parse().unwrap(),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let session = Session::start(
            Position { x: 5, y: 5 },
            Arc::new(|| true),
            || Ok(files()),
            Arc::new(move |event| {
                let _ = tx.send(describe(event));
            }),
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        session.position(point);
        loop {
            let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            eprintln!("{event}");
            if event == "feedback 1" {
                break;
            }
        }
        session.commit(point).unwrap();
        loop {
            let event = rx.recv_timeout(Duration::from_secs(35)).unwrap();
            eprintln!("{event}");
            if event.starts_with("finished") {
                assert_eq!(event, "finished true None");
                break;
            }
        }
    }

    /// Run while the test server's left button is held; release it over a
    /// window that takes file drops.
    #[test]
    #[ignore = "needs an X display with the left button held"]
    fn follows_held_pointer() {
        let (tx, rx) = std::sync::mpsc::channel();
        let _session = Session::physical(
            Arc::new(|| true),
            || Ok(files()),
            Arc::new(move |event| {
                let _ = tx.send(describe(event));
            }),
        )
        .unwrap();
        loop {
            let event = rx.recv_timeout(Duration::from_secs(40)).unwrap();
            eprintln!("{event}");
            if event.starts_with("finished") {
                assert_eq!(event, "finished true None");
                break;
            }
        }
    }
}
